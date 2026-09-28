use crate::{
    adapters::StoreAdapter,
    context::Context,
    model::{Event, Session},
    storage::sqlite as db,
    util::{one_line, parse_ts, s},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{collections::HashSet, path::PathBuf};
pub struct T3;
impl T3 {
    pub(crate) fn running(&self, ctx: &Context) -> Result<Vec<Session>> {
        self.query(ctx, "and r.status = 'running'", &[])
    }
    fn path(ctx: &Context) -> PathBuf {
        ctx.home.join(".t3").join("userdata").join("state.sqlite")
    }
    fn query(&self, ctx: &Context, filter: &str, args: &[Value]) -> Result<Vec<Session>> {
        let path = Self::path(ctx);
        if !path.exists() {
            return Ok(vec![]);
        }
        let con = match db::open(ctx, "t3", &path) {
            Ok(c) => c,
            Err(_) => return Ok(vec![]),
        };
        let sql = format!(
            "select t.thread_id, t.title, t.branch, t.worktree_path, t.created_at, t.updated_at, t.latest_user_message_at, t.model_selection_json, t.archived_at, t.deleted_at, p.workspace_root, p.title as project, r.provider_name, r.provider_instance_id, r.resume_cursor_json, r.status from projection_threads t left join projection_projects p on p.project_id=t.project_id left join provider_session_runtime r on r.thread_id=t.thread_id where t.deleted_at is null {filter}"
        );
        let rows = match db::query(&con, &sql, args) {
            Ok(r) => r,
            Err(e) => {
                db::report(ctx, "t3", &path, &e);
                return Ok(vec![]);
            }
        };
        ctx.cover("t3", Some(&path), "sessions", "read", None);
        let settings = ctx.t3_settings();
        Ok(rows.iter().map(|r|{
            let sel:Value=serde_json::from_str(s(r,"model_selection_json")).unwrap_or(Value::Null);
            let inst=db::or(&r["provider_instance_id"],&sel["instanceId"]);
            let driver=db::or(&settings["providerInstances"][inst.as_str().unwrap_or("")]["driver"],&db::or(&r["provider_name"],&inst));
            let driver=driver.as_str().unwrap_or("");let provider=match driver{"claudeAgent"=>"claude",v=>v};
            json!({"harness":"t3","home":ctx.tilde(path.parent().unwrap()),"id":r["thread_id"],"parent":null,"kind":"main","cwd":db::or(&r["worktree_path"],&r["workspace_root"]),"branch":r["branch"],"title":r["title"],"first_prompt":null,"started":r["created_at"],"updated":db::or(&r["latest_user_message_at"],&r["updated_at"]),"model":sel["model"],"path":format!("{}#thread={}",ctx.tilde(&path),s(r,"thread_id")),"provider_instance":inst,"provider":if provider.is_empty(){Value::Null}else{json!(provider)},"provider_id":provider_id(driver,s(r,"resume_cursor_json"),s(r,"thread_id")),"status":r["status"],"archived":db::truth(&r["archived_at"])})
        }).collect())
    }
}
pub fn provider_id(driver: &str, cursor: &str, thread: &str) -> Value {
    let cur: Value = serde_json::from_str(cursor).unwrap_or(Value::Null);
    match driver {
        "claudeAgent" => cur["resume"].clone(),
        "codex" => cur["threadId"].clone(),
        _ => ["sessionId", "sessionID", "session_id", "resume", "threadId"]
            .into_iter()
            .map(|k| cur[k].clone())
            .find(|v| db::truth(v) && v.as_str() != Some(thread))
            .unwrap_or(Value::Null),
    }
}
impl StoreAdapter for T3 {
    fn name(&self) -> &'static str {
        "t3"
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
        _subagents: bool,
    ) -> Result<Vec<Session>> {
        match since {
            Some(t) => self.query(
                ctx,
                "and coalesce(t.latest_user_message_at,t.updated_at)>=?",
                &[json!(t.format("%Y-%m-%dT%H:%M:%S").to_string())],
            ),
            None => self.query(ctx, "", &[]),
        }
    }
    fn resolve(&self, ctx: &Context, id: &str) -> Result<Vec<Session>> {
        let mut rows = self.query(ctx, "and t.thread_id like ?", &[json!(format!("{id}%"))])?;
        rows.extend(
            self.query(
                ctx,
                "and r.resume_cursor_json like ?",
                &[json!(format!("%{id}%"))],
            )?
            .into_iter()
            .filter(|r| db::truth(&r["provider_id"]) && s(r, "provider_id").starts_with(id)),
        );
        let mut seen = HashSet::new();
        rows.retain(|r| seen.insert(s(r, "id").to_owned()));
        Ok(rows)
    }
    fn events<'a>(
        &self,
        ctx: &'a Context,
        row: &'a Session,
        _needles: Option<&[String]>,
    ) -> Result<Box<dyn Iterator<Item = Event> + 'a>> {
        let path = Self::path(ctx);
        let con = db::open(ctx, "t3", &path)?;
        let result = (|| -> Result<Vec<(String, Event)>> {
            let msgs = db::query(
                &con,
                "select role,text,created_at from projection_thread_messages where thread_id=?",
                &[row["id"].clone()],
            )?;
            let acts = db::query(
                &con,
                "select kind,summary,payload_json,created_at from projection_thread_activities where thread_id=? and kind in ('tool.completed','runtime.error','tool.denied','context-compaction','runtime.warning')",
                &[row["id"].clone()],
            )?;
            let mut events = Vec::new();
            for m in msgs {
                events.push((
                    s(&m, "created_at").into(),
                    Event::new(
                        if s(&m, "role") == "user" {
                            "prompt"
                        } else {
                            "assistant"
                        },
                        s(&m, "text"),
                        parse_ts(&m["created_at"]),
                    ),
                ));
            }
            for a in acts {
                let kind = s(&a, "kind");
                let ts = parse_ts(&a["created_at"]);
                let mut e = if kind == "tool.completed" {
                    let data: Value =
                        serde_json::from_str(s(&a, "payload_json")).unwrap_or(Value::Null);
                    let mut e = Event::new("tool", "", ts);
                    e.name = db::or(&data["data"]["toolName"], &a["summary"])
                        .as_str()
                        .map(str::to_owned);
                    e.input = Some(if data["data"]["input"].is_null() {
                        a["summary"].clone()
                    } else {
                        data["data"]["input"].clone()
                    });
                    e
                } else {
                    Event::new(
                        "system",
                        format!("[{kind}] {}", one_line(s(&a, "summary"), 200)),
                        ts,
                    )
                };
                e.error = matches!(kind, "runtime.error" | "tool.denied");
                events.push((s(&a, "created_at").into(), e));
            }
            Ok(events)
        })();
        let mut items = match result {
            Ok(v) => v,
            Err(e) => {
                db::report(ctx, "t3", &path, &e);
                return Err(e);
            }
        };
        items.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Box::new(items.into_iter().map(|(_, e)| e)))
    }
    fn children(&self, _ctx: &Context, _row: &Session) -> Result<Vec<Session>> {
        Ok(vec![])
    }
    fn where_info(&self, ctx: &Context) -> Result<Vec<Value>> {
        let path = Self::path(ctx);
        if !path.exists() {
            return Ok(vec![]);
        }
        let counts=db::read(ctx, "t3", &path, "select count(*) as n,max(updated_at) as newest from projection_threads where deleted_at is null", &[]).unwrap_or_default();
        let r = counts.first().cloned().unwrap_or(Value::Null);
        let settings = ctx.t3_settings();
        let insts = settings["providerInstances"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        format!(
                            "{k}->{}",
                            if s(&v["config"], "homePath").is_empty() {
                                "default"
                            } else {
                                s(&v["config"], "homePath")
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut notes = format!("provider instances: {insts}");
        let stale = [".t3/userdata-v2", ".t3/dev"]
            .iter()
            .map(|p| ctx.home.join(p))
            .filter(|p| p.is_dir())
            .map(|p| ctx.tilde(&p))
            .collect::<Vec<_>>();
        if !stale.is_empty() {
            notes.push_str(&format!(
                "; ignore {} (dead or dev stores)",
                stale.join(", ")
            ));
        }
        Ok(vec![
            json!({"harness":"t3","path":ctx.tilde(&path),"sessions":r["n"],"newest":parse_ts(&r["newest"]),"index":"projection_threads + provider_session_runtime.resume_cursor_json","notes":notes}),
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_identity_respects_driver_and_skips_thread() {
        assert_eq!(
            provider_id(
                "codex",
                r#"{"threadId":"upstream","resume":"other"}"#,
                "local"
            ),
            json!("upstream")
        );
        assert_eq!(
            provider_id(
                "grok",
                r#"{"sessionId":"local","sessionID":"upstream"}"#,
                "local"
            ),
            json!("upstream")
        );
        assert!(provider_id("codex", "[]", "x").is_null());
    }
}

#[cfg(test)]
mod database_tests {
    use super::*;
    #[test]
    fn t3_resolves_provider_and_sorts_mixed_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = dir.path().into();
        let path = T3::path(&ctx);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path.parent().unwrap().join("settings.json"),
            r#"{"providerInstances":{"custom":{"driver":"claudeAgent"}}}"#,
        )
        .unwrap();
        let con = rusqlite::Connection::open(&path).unwrap();
        con.execute_batch("PRAGMA journal_mode=WAL;
CREATE TABLE projection_threads(thread_id TEXT,title TEXT,branch TEXT,worktree_path TEXT,created_at TEXT,updated_at TEXT,latest_user_message_at TEXT,model_selection_json TEXT,archived_at TEXT,deleted_at TEXT,project_id TEXT);
CREATE TABLE projection_projects(project_id TEXT,workspace_root TEXT,title TEXT);
CREATE TABLE provider_session_runtime(thread_id TEXT,provider_name TEXT,provider_instance_id TEXT,resume_cursor_json TEXT,status TEXT);
CREATE TABLE projection_thread_messages(thread_id TEXT,role TEXT,text TEXT,created_at TEXT);
CREATE TABLE projection_thread_activities(thread_id TEXT,kind TEXT,summary TEXT,payload_json TEXT,created_at TEXT);
INSERT INTO projection_projects VALUES('p','/repo','Project');
INSERT INTO projection_threads VALUES('local','Title',NULL,NULL,'2026-01-01','2026-01-03','2026-01-02','{\"model\":\"m\"}',NULL,NULL,'p');
INSERT INTO provider_session_runtime VALUES('local','custom','custom','{\"resume\":\"remote-session\"}','running');
INSERT INTO projection_thread_messages VALUES('local','assistant','later','2026-01-02');
INSERT INTO projection_thread_activities VALUES('local','runtime.error','failure','{}','2026-01-01');").unwrap();
        let rows = T3.resolve(&ctx, "remote").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["provider"], "claude");
        assert_eq!(rows[0]["cwd"], "/repo");
        assert_eq!(rows[0]["updated"], "2026-01-02");
        let events = T3.events(&ctx, &rows[0], None).unwrap().collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert!(events[0].error);
        assert_eq!(events[1].text, "later");
    }
}
