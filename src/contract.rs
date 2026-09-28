//! Explicit, closed v1 projection. This module never discovers or opens stores.
use std::path::{Component, PathBuf};

use chrono::SecondsFormat;
use serde_json::{Value, json};

use crate::{
    context::Context,
    model::Session,
    util::{parse_ts, redact, s},
};

pub fn timestamp(value: &Value) -> Value {
    parse_ts(value)
        .map(|dt| json!(dt.to_rfc3339_opts(SecondsFormat::Micros, true)))
        .unwrap_or(Value::Null)
}

/// Resolve tilde and relative paths without opening a store or enumerating a directory.
fn absolute(ctx: &Context, text: &str) -> String {
    let path = ctx.expand(text);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| ctx.home.clone())
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(part.as_os_str()),
        }
    }
    if let Some(cached) = ctx.canonical_paths.borrow().get(&normalized) {
        return cached.clone();
    }
    let canonical = std::fs::canonicalize(&normalized).unwrap_or_else(|_| normalized.clone());
    let text = canonical.to_string_lossy().into_owned();
    // canonicalize on Windows uses extended-length spelling; retain ordinary native paths.
    let text = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    };
    ctx.canonical_paths
        .borrow_mut()
        .insert(normalized, text.clone());
    text
}

fn source_parts(row: &Session) -> (&str, Option<&str>, bool) {
    let raw = s(row, "path");
    // A '#' in a real JSONL filename is not a SQLite selector.
    let split = raw.rsplit_once('#').filter(|(_, selector)| {
        selector.starts_with("thread=") || selector.starts_with("session=")
    });
    let (path, selector) = split.map_or((raw, None), |(p, v)| (p, Some(v)));
    let sqlite = selector.is_some() || path.ends_with(".sqlite") || path.ends_with(".db");
    (path, selector, sqlite)
}

fn source(ctx: &Context, row: &Session) -> Value {
    let (path, selector, sqlite) = source_parts(row);
    json!({"kind": if sqlite {"sqlite"} else {"jsonl"}, "path":absolute(ctx,path), "selector":selector})
}

pub fn identity(ctx: &Context, row: &Session) -> Value {
    let (path, _, sqlite) = source_parts(row);
    let store = if !s(row, "store_id").is_empty() {
        s(row, "store_id").to_owned()
    } else if sqlite {
        absolute(ctx, path)
    } else {
        absolute(ctx, s(row, "home"))
    };
    json!({"host_id":ctx.host_id,"store_id":store,"harness":s(row,"harness"),"id":s(row,"id")})
}

fn nullable_text(row: &Value, field: &str) -> Value {
    row.get(field)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(|v| json!(redact(v)))
        .unwrap_or(Value::Null)
}

pub fn session(ctx: &Context, row: &Session) -> Value {
    session_with_rows(ctx, row, &[])
}

/// Combine explicit rows with remembered pre-merge discovery, without scanning stores.
pub fn session_with_rows(ctx: &Context, row: &Session, all_rows: &[Session]) -> Value {
    let own = identity(ctx, row);
    let mut aliases = Vec::new();
    let mut add = |relation: &str, id: Value| {
        if id != own
            && !aliases
                .iter()
                .any(|a: &Value| a["relation"] == relation && a["identity"] == id)
        {
            aliases.push(json!({"relation":relation,"identity":id}));
        }
    };
    if let Some(copies) = row.get("copies").and_then(Value::as_array) {
        for home in copies.iter().filter_map(Value::as_str) {
            let mut id = own.clone();
            id["store_id"] = json!(absolute(ctx, home));
            add("duplicate", id);
        }
    }
    let remembered = ctx.known_sessions.borrow();
    let by_id = ctx.known_by_id.borrow();
    let by_provider = ctx.known_t3_by_provider.borrow();
    let related_ids = [
        s(row, "parent"),
        s(row, "t3_thread"),
        if s(row, "harness") == "t3" {
            s(row, "provider_id")
        } else {
            ""
        },
    ];
    let remembered_candidates = related_ids
        .iter()
        .filter(|id| !id.is_empty())
        .filter_map(|id| by_id.get(*id))
        .flatten()
        .chain(
            by_provider
                .get(&(s(row, "harness").to_owned(), s(row, "id").to_owned()))
                .into_iter()
                .flatten(),
        )
        .map(|index| &remembered[*index]);
    for other in all_rows.iter().chain(remembered_candidates) {
        let parent = !s(row, "parent").is_empty()
            && s(row, "parent") == s(other, "id")
            && s(row, "harness") == s(other, "harness");
        let t3_thread = s(other, "harness") == "t3"
            && ((!s(row, "t3_thread").is_empty() && s(other, "id") == s(row, "t3_thread"))
                || (!s(other, "provider_id").is_empty()
                    && s(other, "provider_id") == s(row, "id")
                    && s(other, "provider") == s(row, "harness")));
        let provider = s(row, "harness") == "t3"
            && !s(row, "provider_id").is_empty()
            && s(row, "provider_id") == s(other, "id")
            && s(row, "provider") == s(other, "harness");
        // Only canonicalize paths for actual relationship candidates.
        if !parent && !t3_thread && !provider {
            continue;
        }
        let id = identity(ctx, other);
        if parent && own["store_id"] == id["store_id"] {
            add("parent", id.clone());
        }
        if t3_thread {
            add("t3_thread", id.clone());
        }
        if provider {
            add("provider", id);
        }
    }
    aliases.sort_by_key(|v| (s(v, "relation").to_owned(), identity_key(&v["identity"])));
    let mut attributes = serde_json::Map::new();
    for key in [
        "entrypoint",
        "agent_type",
        "originator",
        "nickname",
        "provider_instance",
        "provider",
        "provider_id",
        "status",
    ] {
        attributes.insert(key.into(), nullable_text(row, key));
    }
    json!({
        "identity":own,"home":absolute(ctx,s(row,"home")),
        "parent":row.get("parent").and_then(Value::as_str).filter(|v| !v.is_empty()),
        "kind":if s(row,"kind").is_empty() {"main"} else {s(row,"kind")},
        "cwd":row.get("cwd").and_then(Value::as_str).filter(|v| !v.is_empty()),
        "branch":nullable_text(row,"branch"),"title":nullable_text(row,"title"),
        "first_prompt":nullable_text(row,"first_prompt"),"started":timestamp(&row["started"]),
        "updated":timestamp(&row["updated"]),"model":nullable_text(row,"model"),
        "archived":row.get("archived").and_then(|v| v.as_bool().or_else(|| v.as_i64().map(|n| n != 0))),
        "source":source(ctx,row),"aliases":aliases,"attributes":attributes
    })
}

pub fn identity_key(id: &Value) -> (String, String, String, String) {
    (
        s(id, "host_id").into(),
        s(id, "store_id").into(),
        s(id, "harness").into(),
        s(id, "id").into(),
    )
}

pub fn ls_result(ctx: &Context, rows: &[Session], total: usize, limit: usize) -> Value {
    ctx.remember(rows);
    json!({"sessions": rows.iter().take(limit).map(|r| session(ctx,r)).collect::<Vec<_>>(),
        "total":total,"limit":limit,"truncated":total > rows.len().min(limit)})
}

pub fn resolve_result(ctx: &Context, query: &str, rows: &[Session]) -> Value {
    ctx.remember(rows);
    let mut candidates: Vec<_> = rows.iter().map(|r| session(ctx, r)).collect();
    candidates.sort_by_key(|r| identity_key(&r["identity"]));
    candidates.dedup_by(|a, b| a["identity"] == b["identity"]);
    json!({"query":redact(query),"resolution":match candidates.len() {0=>"not_found",1=>"resolved",_=>"ambiguous"},"candidates":candidates})
}

/// Identifiers come from the caller's captured environment, not a second environment read.
pub fn me_result(ctx: &Context, identifiers: &[(String, String)], rows: &[Session]) -> Value {
    let mut ids:Vec<_> = identifiers.iter().filter(|(key,id)| !id.is_empty() &&
        ["CLAUDE_CODE_SESSION_ID","CODEX_THREAD_ID","CODEX_SESSION_ID"].contains(&key.as_str()))
        .map(|(key,id)|json!({"environment_variable":key,"id":id,"resolved":rows.iter().any(|r|
            s(r,"id") == id && s(r,"harness") == if key == "CLAUDE_CODE_SESSION_ID" {"claude"} else {"codex"})})).collect();
    ids.sort_by_key(|id| {
        (
            s(id, "environment_variable").to_owned(),
            s(id, "id").to_owned(),
        )
    });
    ids.dedup();
    let resolved = resolve_result(ctx, "", rows);
    json!({"identifiers":ids,"sessions":resolved["candidates"]})
}

pub fn envelope(ctx: &Context, command: &str, status: &str, result: Value) -> Value {
    let capability = match command {
        "find" => "search",
        "resolve" | "me" => "resolve",
        "live" => "live",
        "handoff" => "events",
        _ => "sessions",
    };
    let mut coverage:Vec<Value> = ctx.coverage.borrow().iter().map(|c| {
        let unsupported = command == "live" && !["claude","t3"].contains(&c.harness.as_str());
        json!({"harness":c.harness,"store_id":c.store_id,"capability":capability,
            "status":if unsupported && c.status != "absent" {"unsupported"} else {&c.status},
            "reason":if unsupported && c.status != "absent" {Some("Harness has no supported liveness evidence".to_owned())} else {c.reason.as_deref().map(redact)}})
    }).collect();
    coverage.sort_by_key(|c| {
        (
            s(c, "harness").to_owned(),
            s(c, "store_id").to_owned(),
            s(c, "status").to_owned(),
        )
    });
    coverage.dedup();
    let mut diagnostics:Vec<Value> = ctx.diagnostics.borrow().iter().map(|d| {
        let code = if ["store_unreadable","store_busy","unsupported_store","malformed_record","process_unverifiable","invalid_argument","ambiguous_identity","not_found","internal_error","unsupported_storage_mode"].contains(&d.code.as_str()) {d.code.as_str()} else {"internal_error"};
        json!({"code":code,"severity":if ["info","warning","error"].contains(&d.severity.as_str()) {d.severity.as_str()} else {"warning"},
            "harness":d.harness,"store_id":d.store_id,"message":redact(&d.message),"retryable":d.retryable})
    }).collect();
    let incomplete = coverage
        .iter()
        .any(|c| ["unreadable", "busy", "unsupported_storage_mode"].contains(&s(c, "status")))
        || diagnostics.iter().any(|d| {
            ![
                "unsupported_store",
                "not_found",
                "ambiguous_identity",
                "invalid_argument",
            ]
            .contains(&s(d, "code"))
        });
    let status = if status == "error" {
        "error"
    } else if status == "partial" || incomplete {
        "partial"
    } else {
        status
    };
    if diagnostics.is_empty() && ["error", "partial"].contains(&status) {
        diagnostics.push(json!({"code":"internal_error","severity":if status == "error" {"error"} else {"warning"},"harness":null,"store_id":null,
            "message":"The operation did not complete; consult coverage for affected stores","retryable":false}));
    }
    if command == "handoff"
        && status == "not_found"
        && !diagnostics.iter().any(|d| d["code"] == "not_found")
    {
        diagnostics.push(json!({"code":"not_found","severity":"info","harness":null,"store_id":null,"message":"No matching session","retryable":false}));
    }
    json!({"schema_version":"1.0","command":command,"host_id":ctx.host_id,
        "observed_at":ctx.now.to_rfc3339_opts(SecondsFormat::Micros,true),"status":status,
        "complete":!["partial","error"].contains(&status),"redaction":"agent-traces-redact-v1",
        "diagnostics":diagnostics,"coverage":coverage,"result":if status == "error" {Value::Null} else {result}})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> Context {
        let mut ctx = Context::from_env();
        ctx.host_id = Some("test-host".into());
        ctx.home = std::env::temp_dir().join("agent-traces-contract-fixture");
        ctx
    }
    #[test]
    fn unindexed_codex_keeps_nullable_fields_and_normalizes_time() {
        let ctx = context();
        let row = json!({"harness":"codex","home":"~/.codex","id":"abc123","path":"~/.codex/sessions/rollout.jsonl",
            "started":"2026-09-26T03:04:05.123456-07:00","updated":"invalid","title":"sk-abcdefghijklmnopqrstuvwxyz"});
        let out = session(&ctx, &row);
        assert_eq!(out["started"], "2026-09-26T10:04:05.123456Z");
        assert!(out["updated"].is_null());
        assert!(out["branch"].is_null());
        assert!(out["archived"].is_null());
        assert_eq!(out["title"], "<redacted>");
        let schema: Value =
            serde_json::from_str(include_str!("../schemas/cli-v1.schema.json")).unwrap();
        let required = schema["$defs"]["session"]["required"].as_array().unwrap();
        assert_eq!(out.as_object().unwrap().len(), required.len());
        for field in required {
            assert!(
                out.get(field.as_str().unwrap()).is_some(),
                "missing {field}"
            );
        }
        assert_eq!(out["attributes"].as_object().unwrap().len(), 8);
        assert_eq!(
            out["source"]["path"],
            json!(
                ctx.home
                    .join(".codex")
                    .join("sessions")
                    .join("rollout.jsonl")
                    .to_string_lossy()
            )
        );
    }
    #[test]
    fn aliases_require_real_provider_identity_and_sqlite_selector_is_separate() {
        let ctx = context();
        let t3 = json!({"harness":"t3","home":"~/.t3/userdata","id":"thread","path":"~/.t3/userdata/state.sqlite#thread=thread","provider":"codex","provider_id":"provider"});
        let provider = json!({"harness":"codex","home":"~/.codex-work","id":"provider","path":"~/.codex-work/sessions/a.jsonl","t3_thread":"thread"});
        assert_eq!(session(&ctx, &t3)["aliases"], json!([]));
        let rows = vec![t3.clone(), provider.clone()];
        let out = session_with_rows(&ctx, &t3, &rows);
        assert_eq!(out["aliases"][0]["identity"], identity(&ctx, &provider));
        assert_eq!(out["source"]["selector"], "thread=thread");
        assert!(!s(&out["source"], "path").contains('#'));
        assert_eq!(
            session_with_rows(&ctx, &provider, &rows)["aliases"][0]["relation"],
            "t3_thread"
        );
    }
    #[test]
    fn zero_limit_and_partial_coverage_preserve_full_query_information() {
        let ctx = context();
        ctx.cover(
            "codex",
            Some(&ctx.home.join(".codex")),
            "sessions",
            "busy",
            Some("database busy".into()),
        );
        let result = ls_result(&ctx, &[], 7, 0);
        assert_eq!(result["total"], 7);
        assert_eq!(result["truncated"], true);
        let out = envelope(&ctx, "ls", "ok", result);
        assert_eq!(out["status"], "partial");
        assert_eq!(out["complete"], false);
        assert!(!out["diagnostics"].as_array().unwrap().is_empty());
    }
    #[test]
    fn unsupported_liveness_is_complete_coverage() {
        let ctx = context();
        ctx.cover(
            "opencode",
            Some(&ctx.home.join("opencode.db")),
            "sessions",
            "read",
            None,
        );
        let out = envelope(&ctx, "live", "not_found", json!({"observations":[]}));
        assert_eq!(out["coverage"][0]["status"], "unsupported");
        assert_eq!(out["coverage"][0]["capability"], "live");
        assert_eq!(out["complete"], true);
    }

    #[test]
    fn healthy_search_prefilter_skips_are_not_incomplete_records() {
        let ctx = context();
        ctx.skipped.set(420);
        ctx.cover(
            "claude",
            Some(&ctx.home.join(".claude")),
            "search",
            "read",
            None,
        );
        let out = envelope(&ctx, "find", "ok", json!({"matches":[]}));
        assert_eq!(out["status"], "ok");
        assert_eq!(out["complete"], true);
        assert_eq!(out["diagnostics"], json!([]));
    }

    #[test]
    fn remembered_relationships_survive_merge_filter_and_limit() {
        let ctx = context();
        let parent = json!({"harness":"codex","home":"~/.codex","id":"parent","path":"~/.codex/sessions/parent.jsonl"});
        let raw_provider = json!({"harness":"codex","home":"~/.codex","id":"provider","parent":"parent","path":"~/.codex/sessions/provider.jsonl"});
        let t3 = json!({"harness":"t3","home":"~/.t3/userdata","id":"thread","path":"~/.t3/userdata/state.sqlite#thread=thread","provider":"codex","provider_id":"provider"});
        ctx.remember(&[parent.clone(), raw_provider.clone(), t3.clone()]);
        let mut merged_provider = raw_provider.clone();
        merged_provider["t3_thread"] = json!("thread");
        let legacy_before_projection = merged_provider.clone();
        let rows = [merged_provider];
        let listed = ls_result(&ctx, &rows, 2, 1);
        let expected = json!([
            {"relation":"parent","identity":identity(&ctx,&parent)},
            {"relation":"t3_thread","identity":identity(&ctx,&t3)}
        ]);
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(listed["sessions"][0]["aliases"], expected);
        assert_eq!(
            resolve_result(&ctx, "provider", &rows)["candidates"][0]["aliases"],
            expected
        );
        assert_eq!(session(&ctx, &raw_provider)["aliases"], expected);
        assert_eq!(
            session_with_rows(&ctx, &raw_provider, &[parent, t3])["aliases"],
            expected
        );
        assert_eq!(rows[0], legacy_before_projection);
        assert!(rows[0].get("known_sessions").is_none());
    }

    #[test]
    fn indexed_aliases_preserve_store_boundaries_and_late_discovery() {
        let ctx = context();
        let provider = json!({"harness":"codex","home":"~/.codex","id":"same","parent":"parent","path":"~/.codex/sessions/a.jsonl"});
        let other_provider = json!({"harness":"codex","home":"~/.codex-work","id":"same","path":"~/.codex-work/sessions/a.jsonl"});
        let parent = json!({"harness":"codex","home":"~/.codex","id":"parent","path":"~/.codex/sessions/p.jsonl"});
        let other_parent = json!({"harness":"codex","home":"~/.codex-work","id":"parent","path":"~/.codex-work/sessions/p.jsonl"});
        let t3 = json!({"harness":"t3","home":"~/.t3/userdata","id":"thread","path":"~/.t3/userdata/state.sqlite#thread=thread","provider":"codex","provider_id":"same"});
        ctx.remember(&[
            provider.clone(),
            other_provider.clone(),
            other_parent,
            parent.clone(),
        ]);
        assert_eq!(
            session(&ctx, &provider)["aliases"],
            json!([{"relation":"parent","identity":identity(&ctx,&parent)}])
        );
        ctx.remember(std::slice::from_ref(&t3));
        let out = session(&ctx, &t3);
        let mut ids = vec![identity(&ctx, &provider), identity(&ctx, &other_provider)];
        ids.sort_by_key(identity_key);
        assert_eq!(
            out["aliases"],
            json!(
                ids.into_iter()
                    .map(|id| json!({"relation":"provider","identity":id}))
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            session(&ctx, &provider)["aliases"][1],
            json!({"relation":"t3_thread","identity":identity(&ctx,&t3)})
        );
    }
}
