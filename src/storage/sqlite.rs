use crate::context::Context;
use anyhow::{Result, bail};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::ValueRef};
use serde_json::{Map, Value};
use std::{path::Path, time::Duration};
pub fn open(ctx: &Context, harness: &str, path: &Path) -> Result<Connection> {
    ctx.protect_source(path);
    let result = (|| {
        let con = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        con.busy_timeout(Duration::from_secs(10))?;
        let mode: String = con.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!(
                "expected existing WAL journal mode, found {mode}; refusing to change live database"
            );
        }
        Ok(con)
    })();
    if let Err(ref e) = result {
        report(ctx, harness, path, e);
    }
    result
}
pub fn report(ctx: &Context, harness: &str, path: &Path, error: &anyhow::Error) {
    let code = match error.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            "store_busy"
        }
        _ if error.to_string().contains("expected existing WAL") => "unsupported_storage_mode",
        _ => "store_unreadable",
    };
    ctx.diagnostic(code, Some(harness), Some(path), error.to_string());
    ctx.cover(
        harness,
        Some(path),
        "sessions",
        match code {
            "store_busy" => "busy",
            "unsupported_storage_mode" => "unsupported_storage_mode",
            _ => "unreadable",
        },
        Some(error.to_string()),
    );
}
pub fn query(con: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Value>> {
    let args: Vec<rusqlite::types::Value> = params
        .iter()
        .map(|v| match v {
            Value::Null => rusqlite::types::Value::Null,
            Value::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
            Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap_or_default()),
            Value::String(s) => rusqlite::types::Value::Text(s.clone()),
            Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
            _ => rusqlite::types::Value::Text(v.to_string()),
        })
        .collect();
    let mut stmt = con.prepare(sql)?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let rows = stmt
        .query_map(params_from_iter(args), |r| {
            let mut map = Map::new();
            for (i, key) in columns.iter().enumerate() {
                let val = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => n.into(),
                    ValueRef::Real(n) => serde_json::json!(n),
                    ValueRef::Text(s) => Value::String(String::from_utf8_lossy(s).into_owned()),
                    ValueRef::Blob(_) => Value::Null,
                };
                map.insert(key.clone(), val);
            }
            Ok(Value::Object(map))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
pub fn table_exists(con: &Connection, name: &str) -> Result<bool> {
    Ok(con.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name=?)",
        [name],
        |r| r.get(0),
    )?)
}
pub fn truth(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
pub fn or(a: &Value, b: &Value) -> Value {
    if truth(a) { a.clone() } else { b.clone() }
}
pub fn stamp(v: &Value) -> Value {
    if truth(v) {
        crate::util::parse_ts(v)
            .map(crate::util::iso)
            .map(Value::String)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// Run a short read and attach schema/query failures to the same store diagnostics.
pub fn read(
    ctx: &Context,
    harness: &str,
    path: &Path,
    sql: &str,
    args: &[Value],
) -> Result<Vec<Value>> {
    let con = open(ctx, harness, path)?;
    query(&con, sql, args).inspect_err(|error| report(ctx, harness, path, error))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wal_reader_observes_writer_commit_and_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE x(n);INSERT INTO x VALUES(1);")
            .unwrap();
        let ctx = Context::from_env();
        let reader = open(&ctx, "test", &path).unwrap();
        writer.execute("INSERT INTO x VALUES(2)", []).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT sum(n) FROM x", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert!(reader.execute("INSERT INTO x VALUES(3)", []).is_err());
        assert_eq!(
            writer
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
    }
    #[test]
    fn rejects_non_wal_without_converting_or_creating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delete.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE x(n)").unwrap();
        let ctx = Context::from_env();
        assert!(open(&ctx, "test", &path).is_err());
        assert_eq!(
            writer
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        let missing = dir.path().join("missing.sqlite");
        assert!(open(&ctx, "test", &missing).is_err());
        assert!(!missing.exists());
    }
}
