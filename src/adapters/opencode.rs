use crate::{
    adapters::StoreAdapter,
    context::Context,
    model::{Event, Session},
    storage::sqlite as db,
    util::{opt_s, parse_ts, s},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::PathBuf;
pub struct OpenCode;
impl OpenCode {
    fn path(ctx: &Context) -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.home.join(".local").join("share"))
            .join("opencode")
            .join("opencode.db")
    }
    fn query(&self, ctx: &Context, filter: &str, args: &[Value]) -> Result<Vec<Session>> {
        let path = Self::path(ctx);
        if !path.exists() {
            return Ok(vec![]);
        }
        let con = match db::open(ctx, "opencode", &path) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        if !db::table_exists(&con, "session_v2")? {
            ctx.diagnostic(
                "unsupported_store",
                Some("opencode"),
                Some(&path),
                "session_v2 table is absent",
            );
            ctx.cover(
                "opencode",
                Some(&path),
                "sessions",
                "unreadable",
                Some("session_v2 table is absent".into()),
            );
            return Ok(vec![]);
        }
        let rows = match db::query(
            &con,
            &format!(
                "select id,parent_id,directory,title,model,agent,time_created,time_updated,time_archived from session_v2 {filter}"
            ),
            args,
        ) {
            Ok(r) => r,
            Err(e) => {
                db::report(ctx, "opencode", &path, &e);
                return Ok(vec![]);
            }
        };
        ctx.cover("opencode", Some(&path), "sessions", "read", None);
        Ok(rows.iter().map(|r| {let model:Value=serde_json::from_str(s(r,"model")).unwrap_or(Value::Null);
            json!({"harness":"opencode","home":ctx.tilde(path.parent().unwrap()),"id":r["id"],"parent":r["parent_id"],"kind":if db::truth(&r["parent_id"]){"subagent"}else{"main"},"cwd":r["directory"],"branch":null,"title":r["title"],"first_prompt":null,"started":db::stamp(&r["time_created"]),"updated":db::stamp(&r["time_updated"]),"model":model["id"],"path":format!("{}#session={}",ctx.tilde(&path),s(r,"id")),"archived":db::truth(&r["time_archived"])})
        }).collect())
    }
}
impl StoreAdapter for OpenCode {
    fn name(&self) -> &'static str {
        "opencode"
    }
    fn homes(&self, ctx: &Context) -> Vec<PathBuf> {
        let p = Self::path(ctx);
        if p.exists() {
            vec![p.parent().unwrap().into()]
        } else {
            vec![]
        }
    }
    fn sessions(
        &self,
        ctx: &Context,
        since: Option<DateTime<Utc>>,
        subagents: bool,
    ) -> Result<Vec<Session>> {
        let mut conditions = Vec::new();
        let mut args = Vec::new();
        if let Some(t) = since {
            conditions.push("time_updated >= ?");
            args.push(json!(t.timestamp_millis()));
        }
        if !subagents {
            conditions.push("parent_id is null");
        }
        self.query(
            ctx,
            &if conditions.is_empty() {
                String::new()
            } else {
                format!("where {}", conditions.join(" and "))
            },
            &args,
        )
    }
    fn resolve(&self, ctx: &Context, id: &str) -> Result<Vec<Session>> {
        self.query(ctx, "where id like ?", &[json!(format!("{id}%"))])
    }
    fn events<'a>(
        &self,
        ctx: &'a Context,
        row: &'a Session,
        _needles: Option<&[String]>,
    ) -> Result<Box<dyn Iterator<Item = Event> + 'a>> {
        let path = Self::path(ctx);
        let con = db::open(ctx, "opencode", &path)?;
        let rows = match db::query(
            &con,
            "select type,data,time_created from session_message where session_id=? order by seq",
            &[row["id"].clone()],
        ) {
            Ok(v) => v,
            Err(e) => {
                db::report(ctx, "opencode", &path, &e);
                return Err(e);
            }
        };
        let mut out = Vec::new();
        for r in rows {
            let ts = parse_ts(&r["time_created"]);
            let d = match serde_json::from_str::<Value>(s(&r, "data")) {
                Ok(v) => v,
                Err(e) => {
                    ctx.diagnostic(
                        "malformed_record",
                        Some("opencode"),
                        Some(&path),
                        e.to_string(),
                    );
                    continue;
                }
            };
            match s(&r, "type") {
                "user" => out.push(Event::new("prompt", s(&d, "text"), ts)),
                "assistant" => {
                    for b in d["content"].as_array().into_iter().flatten() {
                        match s(b, "type") {
                            "text" if !s(b, "text").trim().is_empty() => {
                                let mut e = Event::new("assistant", s(b, "text"), ts);
                                e.model = opt_s(&d["model"], "id");
                                out.push(e)
                            }
                            "tool" => {
                                let state = &b["state"];
                                let mut e = Event::new("tool", "", ts);
                                e.name = opt_s(b, "name");
                                e.input = Some(state["input"].clone());
                                e.id = opt_s(b, "id");
                                out.push(e);
                                let mut body = state["content"].clone();
                                if let Some(arr) = body.as_array() {
                                    body = json!(
                                        arr.iter()
                                            .filter(|v| v.is_object())
                                            .map(|v| s(v, "text"))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    );
                                }
                                if db::truth(&body) || db::truth(&state["error"]) {
                                    let body = db::or(&body, &state["error"]);
                                    let text = body
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| body.to_string());
                                    let mut e = Event::new("result", text, ts);
                                    e.error =
                                        s(state, "status") == "error" || db::truth(&state["error"]);
                                    out.push(e);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "compaction" => out.push(Event::new("system", "[compacted]", ts)),
                "synthetic" | "system" => out.push(Event::new("notice", s(&d, "text"), ts)),
                _ => {}
            }
        }
        Ok(Box::new(out.into_iter()))
    }
    fn children(&self, ctx: &Context, row: &Session) -> Result<Vec<Session>> {
        self.query(ctx, "where parent_id = ?", &[row["id"].clone()])
    }
    fn where_info(&self, ctx: &Context) -> Result<Vec<Value>> {
        let path = Self::path(ctx);
        if !path.exists() {
            return Ok(vec![]);
        }
        let counts = db::read(
            ctx,
            "opencode",
            &path,
            "select count(*) as n,max(time_updated) as newest from session_v2",
            &[],
        )
        .unwrap_or_default();
        let r = counts.first().cloned().unwrap_or(Value::Null);
        Ok(vec![
            json!({"harness":"opencode","path":ctx.tilde(&path),"sessions":r["n"],"newest":parse_ts(&r["newest"]),"index":"session_v2 + session_message (not the storage/ json tree, not v1 tables)","notes":""}),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opencode_uses_v2_parent_filter_and_expands_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = dir.path().into();
        // The path override is read from the environment, so skip this fixture if the host explicitly configured another store.
        if std::env::var_os("XDG_DATA_HOME").is_some() {
            return;
        }
        let path = OpenCode::path(&ctx);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE session_v2(id TEXT,parent_id TEXT,directory TEXT,title TEXT,model TEXT,agent TEXT,time_created INTEGER,time_updated INTEGER,time_archived INTEGER);CREATE TABLE session_message(session_id TEXT,type TEXT,data TEXT,time_created INTEGER,seq INTEGER);
INSERT INTO session_v2 VALUES('main',NULL,'/repo','Title','{\"id\":\"m\"}','agent',1700000000000,1700000001000,NULL);
INSERT INTO session_v2 VALUES('child','main','/repo','Child',NULL,'agent',1700000000000,1700000001000,NULL);").unwrap();
        let data = json!({"content":[{"type":"tool","name":"read","id":"call","state":{"input":{"path":"a"},"content":[{"text":"broken"}],"status":"error"}}]});
        con.execute(
            "INSERT INTO session_message VALUES('main','assistant',?,1700000001000,1)",
            [data.to_string()],
        )
        .unwrap();
        let rows = OpenCode.sessions(&ctx, None, false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["model"], "m");
        assert_eq!(OpenCode.children(&ctx, &rows[0]).unwrap()[0]["id"], "child");
        let events = OpenCode
            .events(&ctx, &rows[0], None)
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("read"));
        assert_eq!(events[1].text, "broken");
        assert!(events[1].error);
    }
}
