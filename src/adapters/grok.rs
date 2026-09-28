use crate::{
    adapters::{
        StoreAdapter,
        claude::{body, entries, mtime, strip_reminders, truthy},
    },
    context::Context,
    model::{Event, Session},
    storage::jsonl::records,
    util::{iso, opt_s, parse_ts, s},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub struct Grok;
pub fn grok_user_text(v: &Value) -> String {
    let text = body(v);
    static QUERY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?s)<user_query>\s*(.*?)\s*(?:</user_query>|$)").unwrap()
    });
    static RUNTIME: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?s)<runtime_info>.*?</runtime_info>").unwrap());
    let text = QUERY
        .captures(&text)
        .map(|c| c[1].to_owned())
        .unwrap_or(text);
    strip_reminders(&RUNTIME.replace_all(&text, ""))
        .trim()
        .into()
}
fn dirs(ctx: &Context, ident: &str) -> Vec<PathBuf> {
    entries(ctx, &ctx.home.join(".grok").join("sessions"))
        .iter()
        .filter(|p| p.is_dir())
        .flat_map(|p| entries(ctx, p))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(ident))
                && p.join("chat_history.jsonl").exists()
        })
        .collect()
}
fn row(ctx: &Context, dir: &Path) -> Value {
    let summary = dir.join("summary.json");
    let v = match fs::read(&summary) {
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) => v,
            Err(e) => {
                ctx.diagnostic(
                    "malformed_record",
                    Some("grok"),
                    Some(&summary),
                    e.to_string(),
                );
                Value::Null
            }
        },
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                ctx.diagnostic(
                    "store_unreadable",
                    Some("grok"),
                    Some(&summary),
                    e.to_string(),
                );
            }
            Value::Null
        }
    };
    let history = dir.join("chat_history.jsonl");
    let mt = json!(mtime(&history).map(iso));
    let mut first = Value::Null;
    let title = if truthy(&v["generated_title"]) {
        v["generated_title"].clone()
    } else {
        v["session_summary"].clone()
    };
    if !truthy(&title) {
        for (i, r) in records(ctx, &history, 0, None) {
            if s(&r, "type") == "user" && !r["prompt_index"].is_null() {
                let t = grok_user_text(&r["content"]);
                if !t.is_empty() {
                    first = json!(t.chars().take(600).collect::<String>());
                    break;
                }
            }
            if i > 300 {
                break;
            }
        }
    }
    let fallback_cwd = dir
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    json!({"harness":"grok","home":ctx.tilde(&ctx.home.join(".grok")),"id":dir.file_name().unwrap_or_default().to_string_lossy(),"parent":null,"kind":"main","cwd":if truthy(&v["info"]["cwd"]){v["info"]["cwd"].clone()}else{json!(percent_encoding::percent_decode_str(&fallback_cwd).decode_utf8_lossy())},"branch":v["head_branch"],"title":title,"first_prompt":first,"started":if truthy(&v["created_at"]){v["created_at"].clone()}else{mt.clone()},"updated":if truthy(&v["last_active_at"]){v["last_active_at"].clone()}else if truthy(&v["updated_at"]){v["updated_at"].clone()}else{mt},"model":v["current_model_id"],"path":history.to_string_lossy(),"originator":if s(&v,"request_id").starts_with("t3-"){Some("t3")}else{None}})
}
fn event_records(r: &Value) -> Vec<Event> {
    let ts = parse_ts(if truthy(&r["timestamp"]) {
        &r["timestamp"]
    } else {
        &r["created_at"]
    });
    let mut out = vec![];
    match s(r, "type") {
        "user" => {
            if !r["prompt_index"].is_null() {
                let t = grok_user_text(&r["content"]);
                if !t.is_empty() {
                    out.push(Event::new("prompt", t, ts));
                }
            }
        }
        "assistant" => {
            let text = body(&r["content"]);
            if !text.trim().is_empty() {
                let mut e = Event::new("assistant", text, ts);
                e.model = opt_s(r, "model_id");
                out.push(e);
            }
            if let Some(a) = r["tool_calls"].as_array() {
                for tc in a {
                    let f = if tc["function"].is_object() {
                        &tc["function"]
                    } else {
                        tc
                    };
                    let mut e = Event::new("tool", "", ts);
                    e.name = opt_s(f, "name");
                    e.input = f.get("arguments").cloned();
                    e.id = opt_s(tc, "id");
                    out.push(e);
                }
            }
        }
        "tool_result" => {
            let text = body(&r["content"]);
            let mut e = Event::new("result", text.clone(), ts);
            e.error = text.trim_start().starts_with("{\"error\"");
            e.tool_use_id = opt_s(r, "tool_call_id");
            out.push(e);
        }
        _ => {}
    }
    out
}
impl StoreAdapter for Grok {
    fn name(&self) -> &'static str {
        "grok"
    }
    fn homes(&self, ctx: &Context) -> Vec<PathBuf> {
        if ctx.home.join(".grok").join("sessions").is_dir() {
            vec![ctx.home.join(".grok")]
        } else {
            vec![]
        }
    }
    fn sessions(
        &self,
        ctx: &Context,
        since: Option<DateTime<Utc>>,
        _subagents: bool,
    ) -> Result<Vec<Session>> {
        let rows = dirs(ctx, "")
            .iter()
            .filter(|p| {
                since.is_none_or(|cut| {
                    mtime(&p.join("chat_history.jsonl")).is_some_and(|mt| mt >= cut)
                })
            })
            .map(|p| row(ctx, p))
            .collect();
        for home in self.homes(ctx) {
            ctx.cover("grok", Some(&home), "sessions", "read", None);
        }
        Ok(rows)
    }
    fn resolve(&self, ctx: &Context, ident: &str) -> Result<Vec<Session>> {
        Ok(dirs(ctx, ident).iter().map(|p| row(ctx, p)).collect())
    }
    fn events<'a>(
        &self,
        ctx: &'a Context,
        row: &'a Session,
        needles: Option<&[String]>,
    ) -> Result<Box<dyn Iterator<Item = Event> + 'a>> {
        Ok(Box::new(
            records(ctx, Path::new(s(row, "path")), 0, needles)
                .flat_map(|(_, r)| event_records(&r)),
        ))
    }
    fn children(&self, _ctx: &Context, _row: &Session) -> Result<Vec<Session>> {
        Ok(vec![])
    }
    fn where_info(&self, ctx: &Context) -> Result<Vec<Value>> {
        if self.homes(ctx).is_empty() {
            return Ok(vec![]);
        }
        let dirs = dirs(ctx, "");
        Ok(vec![
            json!({"harness":"grok","path":ctx.tilde(&ctx.home.join(".grok").join("sessions")),"sessions":dirs.len(),"newest":dirs.iter().filter_map(|p|mtime(&p.join("chat_history.jsonl"))).max().map(iso),"index":"sessions/<urlencoded cwd>/<id>/summary.json + chat_history.jsonl","notes":"session_search.sqlite is stale; subagents run inline (spawn_subagent tool calls)"}),
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_stale_index_file_beside_session_directories() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().to_path_buf();
        let root = ctx.home.join(".grok").join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("session_search.sqlite"),
            "not a session directory",
        )
        .unwrap();
        assert!(dirs(&ctx, "").is_empty());
        assert!(ctx.diagnostics.borrow().is_empty());
    }
    #[test]
    fn wrapped_prompt_and_injections() {
        assert_eq!(
            grok_user_text(&json!(
                "prefix<user_query> hi <runtime_info>x</runtime_info><system-reminder>y</system-reminder></user_query>suffix"
            )),
            "hi"
        );
        assert!(event_records(&json!({"type":"user","content":"injected"})).is_empty());
        assert_eq!(
            event_records(&json!({"type":"user","prompt_index":0,"content":"hello"}))[0].text,
            "hello"
        );
    }
    #[test]
    fn tools_and_results() {
        let events = event_records(
            &json!({"type":"assistant","content":[{"text":"working"}],"tool_calls":[{"id":"a","function":{"name":"shell","arguments":"{}"}}]}),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].name.as_deref(), Some("shell"));
        assert_eq!(events[1].input, Some(json!("{}")));
        let result = event_records(
            &json!({"type":"tool_result","content":" {\"error\":\"bad\"}","tool_call_id":"a"}),
        );
        assert!(result[0].error);
        assert_eq!(result[0].tool_use_id.as_deref(), Some("a"));
    }
    #[test]
    fn title_fallback_and_encoded_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        let d = ctx
            .home
            .join(".grok")
            .join("sessions")
            .join("%2Ftmp%2Fproj")
            .join("abc");
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("chat_history.jsonl"),
            "{\"type\":\"user\",\"prompt_index\":0,\"content\":\"goal\"}\n",
        )
        .unwrap();
        let r = Grok.resolve(&ctx, "ab").unwrap();
        assert_eq!(r[0]["cwd"], "/tmp/proj");
        assert_eq!(r[0]["first_prompt"], "goal");
        fs::write(
            d.join("summary.json"),
            "{\"generated_title\":\"Title\",\"request_id\":\"t3-1\"}",
        )
        .unwrap();
        let r = Grok.resolve(&ctx, "ab").unwrap();
        assert!(r[0]["first_prompt"].is_null());
        assert_eq!(r[0]["originator"], "t3");
    }
}
