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
        // A v1-only home keeps 0.1.0's query, so `live` output there is unchanged.
        if Self::open_v2(ctx)?.is_none() {
            return self.query_v1(ctx, "and r.status = 'running'", &[]);
        }
        Ok(self
            .sessions(ctx, None, false)?
            .into_iter()
            .filter(|r| s(r, "status") == "running")
            .collect())
    }
    fn path(ctx: &Context) -> PathBuf {
        ctx.home.join(".t3").join("userdata").join("state.sqlite")
    }
    fn path_v2(ctx: &Context) -> PathBuf {
        Self::path(ctx).with_file_name("statev2.sqlite")
    }
    fn open_v2(ctx: &Context) -> Result<Option<rusqlite::Connection>> {
        let path = Self::path_v2(ctx);
        if !path.exists() {
            return Ok(None);
        }
        let con = db::open(ctx, "t3", &path)?;
        if db::table_exists(&con, "orchestration_v2_projection_threads")? {
            Ok(Some(con))
        } else {
            Ok(None)
        }
    }
    fn query_v2(ctx: &Context, con: &rusqlite::Connection) -> Result<Vec<Session>> {
        let path = Self::path_v2(ctx);
        let rows = db::query(con, "
            select t.*, p.workspace_root,
              (select max(created_at) from orchestration_v2_projection_messages
               where thread_id=t.thread_id and role='user') as latest_user_message_at,
              pt.driver, pt.payload_json as provider_payload,
              case when exists(select 1 from orchestration_v2_projection_runs
                where thread_id=t.thread_id and status in ('preparing','starting','running','waiting'))
                then 'running' else (select status from orchestration_v2_projection_runs
                where thread_id=t.thread_id order by ordinal desc limit 1) end as status
            from orchestration_v2_projection_threads t
            left join projection_projects p on p.project_id=t.project_id
            left join orchestration_v2_projection_provider_threads pt on pt.provider_thread_id=coalesce(
              (select provider_thread_id from orchestration_v2_projection_provider_threads
               where thread_id=t.thread_id and provider_thread_id=t.active_provider_thread_id),
              (select provider_thread_id from orchestration_v2_projection_provider_threads
               where thread_id=t.thread_id
                 and coalesce(json_extract(payload_json,'$.nativeThreadRef.nativeId'),'') <> ''
               order by updated_at desc limit 1))
            where t.deleted_at is null", &[])
            .inspect_err(|e| db::report(ctx, "t3", &path, e))?;
        ctx.cover("t3", Some(&path), "sessions", "read", None);
        Ok(rows.iter().map(|r| {
            let payload: Value = serde_json::from_str(s(r, "payload_json")).unwrap_or(Value::Null);
            let provider_payload: Value = serde_json::from_str(s(r, "provider_payload")).unwrap_or(Value::Null);
            let driver = match s(r, "driver") { "claudeAgent" => "claude", v => v };
            json!({"harness":"t3", "home":ctx.tilde(path.parent().unwrap()),
                "id":r["thread_id"], "parent":null, "kind":"main",
                "cwd":db::or(&payload["worktreePath"], &r["workspace_root"]),
                "branch":payload["branch"], "title":r["title"], "first_prompt":null,
                "started":r["created_at"], "updated":db::or(&r["latest_user_message_at"], &r["updated_at"]),
                "model":payload["modelSelection"]["model"],
                "path":format!("{}#thread={}",ctx.tilde(&path),s(r,"thread_id")),
                "provider_instance":r["provider_instance_id"].as_str().map(|v| json!(v)).unwrap_or_else(|| payload["modelSelection"]["instanceId"].clone()),
                "provider":if driver.is_empty() {Value::Null} else {json!(driver)},
                "provider_id":provider_payload["nativeThreadRef"]["nativeId"],
                "status":r["status"], "archived":!r["archived_at"].is_null()})
        }).collect())
    }
    fn merged(&self, ctx: &Context, con: &rusqlite::Connection) -> Result<Vec<Session>> {
        let ids = db::query(
            con,
            "select thread_id from orchestration_v2_projection_threads",
            &[],
        )?;
        let ids: HashSet<_> = ids.iter().map(|r| s(r, "thread_id")).collect();
        let mut rows = Self::query_v2(ctx, con)?;
        rows.extend(
            self.query_v1(ctx, "", &[])?
                .into_iter()
                .filter(|r| !ids.contains(s(r, "id"))),
        );
        Ok(rows)
    }
    fn events_v2(ctx: &Context, row: &Session) -> Result<Vec<(String, Event)>> {
        let path = Self::path_v2(ctx);
        let con = db::open(ctx, "t3", &path)?;
        let result = (|| -> Result<Vec<(String, Event)>> {
            let msgs = db::query(
                &con,
                "select role,created_at,payload_json from orchestration_v2_projection_messages where thread_id=? and role in ('user','assistant')",
                &[row["id"].clone()],
            )?;
            let items = db::query(
                &con,
                "select type,status,updated_at,payload_json from orchestration_v2_projection_turn_items where thread_id=? and (type='error' or (type in ('command_execution','dynamic_tool','file_change','file_search','web_search') and status in ('completed','failed')))",
                &[row["id"].clone()],
            )?;
            let mut events = Vec::new();
            for m in msgs {
                let data: Value =
                    serde_json::from_str(s(&m, "payload_json")).unwrap_or(Value::Null);
                events.push((
                    s(&m, "created_at").into(),
                    Event::new(
                        if s(&m, "role") == "user" {
                            "prompt"
                        } else {
                            "assistant"
                        },
                        s(&data, "text"),
                        parse_ts(&m["created_at"]),
                    ),
                ));
            }
            for item in items {
                let data: Value =
                    serde_json::from_str(s(&item, "payload_json")).unwrap_or(Value::Null);
                let time = db::or(&data["completedAt"], &item["updated_at"]);
                let mut event = if s(&item, "type") == "error" {
                    Event::new(
                        "system",
                        format!("[runtime.error] {}", s(&data["failure"], "message")),
                        parse_ts(&time),
                    )
                } else {
                    let mut event = Event::new("tool", "", parse_ts(&time));
                    event.name = db::or(&data["toolName"], &data["title"])
                        .as_str()
                        .map(str::to_owned);
                    event.input = Some(if data["input"].is_null() {
                        data["title"].clone()
                    } else {
                        data["input"].clone()
                    });
                    event
                };
                event.error = s(&item, "type") == "error";
                events.push((time.as_str().unwrap_or("").into(), event));
            }
            Ok(events)
        })();
        result.inspect_err(|e| db::report(ctx, "t3", &path, e))
    }
    fn query_v1(&self, ctx: &Context, filter: &str, args: &[Value]) -> Result<Vec<Session>> {
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
        if p.exists() || Self::path_v2(ctx).exists() {
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
        if let Some(con) = Self::open_v2(ctx)? {
            let mut rows = self.merged(ctx, &con)?;
            if let Some(since) = since {
                rows.retain(|r| parse_ts(&r["updated"]).is_some_and(|t| t >= since));
            }
            return Ok(rows);
        }
        match since {
            Some(t) => self.query_v1(
                ctx,
                "and coalesce(t.latest_user_message_at,t.updated_at)>=?",
                &[json!(t.format("%Y-%m-%dT%H:%M:%S").to_string())],
            ),
            None => self.query_v1(ctx, "", &[]),
        }
    }
    fn resolve(&self, ctx: &Context, id: &str) -> Result<Vec<Session>> {
        if let Some(con) = Self::open_v2(ctx)? {
            let matches = db::query(
                &con,
                "select distinct thread_id from orchestration_v2_projection_provider_threads where substr(json_extract(payload_json,'$.nativeThreadRef.nativeId'),1,length(?))=?",
                &[json!(id), json!(id)],
            )?;
            let ids: HashSet<_> = matches.iter().map(|r| s(r, "thread_id")).collect();
            let mut rows = self.merged(ctx, &con)?;
            rows.retain(|r| {
                s(r, "id").starts_with(id)
                    || ids.contains(s(r, "id"))
                    || s(r, "provider_id").starts_with(id)
            });
            return Ok(rows);
        }
        let mut rows = self.query_v1(ctx, "and t.thread_id like ?", &[json!(format!("{id}%"))])?;
        rows.extend(
            self.query_v1(
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
        if s(row, "path").starts_with(&format!("{}#thread=", ctx.tilde(&Self::path_v2(ctx)))) {
            let mut items = Self::events_v2(ctx, row)?;
            items.sort_by(|a, b| a.0.cmp(&b.0));
            return Ok(Box::new(items.into_iter().map(|(_, e)| e)));
        }
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
        let mut stores = Vec::new();
        let v2 = Self::open_v2(ctx)?;
        if let Some(con) = &v2 {
            let counts = db::query(
                con,
                "select count(*) as n,max(updated_at) as newest from orchestration_v2_projection_threads where deleted_at is null",
                &[],
            )?;
            let r = &counts[0];
            stores.push(json!({"harness":"t3","path":ctx.tilde(&Self::path_v2(ctx)),"sessions":r["n"],"newest":parse_ts(&r["newest"]),"index":"orchestration_v2_projection_threads + orchestration_v2_projection_provider_threads", "notes":"Read v2 threads; v2 thread IDs take precedence over v1."}));
        }
        if !path.exists() {
            return Ok(stores);
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
        // A v1-only home keeps 0.1.0's `where` text.
        let mut notes = if v2.is_some() {
            format!("Read v1 threads whose IDs are absent from v2; provider instances: {insts}")
        } else {
            format!("provider instances: {insts}")
        };
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
        stores.push(json!({"harness":"t3","path":ctx.tilde(&path),"sessions":r["n"],"newest":parse_ts(&r["newest"]),"index":"projection_threads + provider_session_runtime.resume_cursor_json","notes":notes}));
        Ok(stores)
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

#[cfg(test)]
mod v2_tests {
    use super::*;
    use rusqlite::{Connection, params};

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: Context,
        con: Connection,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut ctx = Context::from_env();
            ctx.home = dir.path().into();
            let path = T3::path_v2(&ctx);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let con = Connection::open(path).unwrap();
            con.execute_batch("PRAGMA journal_mode=WAL;
CREATE TABLE orchestration_v2_projection_threads(thread_id TEXT,project_id TEXT,title TEXT,active_provider_thread_id TEXT,created_at TEXT,updated_at TEXT,archived_at TEXT,deleted_at TEXT,payload_json TEXT,provider_instance_id TEXT);
CREATE TABLE projection_projects(project_id TEXT,workspace_root TEXT);
CREATE TABLE orchestration_v2_projection_provider_threads(provider_thread_id TEXT,thread_id TEXT,updated_at TEXT,payload_json TEXT,driver TEXT);
CREATE TABLE orchestration_v2_projection_runs(thread_id TEXT,ordinal INTEGER,status TEXT);
CREATE TABLE orchestration_v2_projection_messages(thread_id TEXT,role TEXT,created_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_turn_items(thread_id TEXT,type TEXT,status TEXT,updated_at TEXT,payload_json TEXT);
INSERT INTO projection_projects VALUES('project','/repo');").unwrap();
            Self {
                _dir: dir,
                ctx,
                con,
            }
        }
        fn thread(&self, id: &str) {
            self.con.execute("INSERT INTO orchestration_v2_projection_threads VALUES(?,'project','V2 title',NULL,'2026-01-01T00:00:00Z','2026-01-05T00:00:00Z',NULL,NULL,'{}',NULL)", [id]).unwrap();
        }
        fn provider(&self, id: &str, thread: &str, native: &str, driver: &str, updated: &str) {
            self.con
                .execute(
                    "INSERT INTO orchestration_v2_projection_provider_threads VALUES(?,?,?,?,?)",
                    params![
                        id,
                        thread,
                        updated,
                        json!({"nativeThreadRef":{"driver":driver,"nativeId":native}}).to_string(),
                        driver
                    ],
                )
                .unwrap();
        }
        fn message(&self, thread: &str, role: &str, text: &str, time: &str) {
            self.con
                .execute(
                    "INSERT INTO orchestration_v2_projection_messages VALUES(?,?,?,?)",
                    params![thread, role, time, json!({"text":text}).to_string()],
                )
                .unwrap();
        }
        fn item(&self, thread: &str, kind: &str, status: &str, time: &str, payload: Value) {
            self.con
                .execute(
                    "INSERT INTO orchestration_v2_projection_turn_items VALUES(?,?,?,?,?)",
                    params![thread, kind, status, time, payload.to_string()],
                )
                .unwrap();
        }
        fn v1(&self) -> Connection {
            let con = Connection::open(T3::path(&self.ctx)).unwrap();
            con.execute_batch("PRAGMA journal_mode=WAL;
CREATE TABLE projection_threads(thread_id TEXT,title TEXT,branch TEXT,worktree_path TEXT,created_at TEXT,updated_at TEXT,latest_user_message_at TEXT,model_selection_json TEXT,archived_at TEXT,deleted_at TEXT,project_id TEXT);
CREATE TABLE projection_projects(project_id TEXT,workspace_root TEXT,title TEXT);
CREATE TABLE provider_session_runtime(thread_id TEXT,provider_name TEXT,provider_instance_id TEXT,resume_cursor_json TEXT,status TEXT);
INSERT INTO projection_projects VALUES('project','/old','Old');
INSERT INTO projection_threads VALUES('legacy','V1 title',NULL,NULL,'2026-01-01','2026-01-05',NULL,'{}',NULL,NULL,'project');
INSERT INTO projection_threads VALUES('shadow','V1 shadow',NULL,NULL,'2026-01-01','2026-01-05',NULL,'{}',NULL,NULL,'project');").unwrap();
            con
        }
    }

    #[test]
    fn v2_fields_active_provider_and_newest_native_fallback() {
        let f = Fixture::new();
        f.thread("active");
        f.thread("fallback");
        f.thread("none");
        f.con.execute("UPDATE orchestration_v2_projection_threads SET active_provider_thread_id='selected', archived_at='', provider_instance_id='column-instance',payload_json=? WHERE thread_id='active'", [json!({"branch":"feat/test","worktreePath":"/worktree","modelSelection":{"model":"model","instanceId":"payload-instance"}}).to_string()]).unwrap();
        f.con.execute("UPDATE orchestration_v2_projection_threads SET payload_json=? WHERE thread_id='fallback'", [json!({"modelSelection":{"instanceId":"payload-instance"}}).to_string()]).unwrap();
        f.provider(
            "selected",
            "active",
            "claude-native",
            "claudeAgent",
            "2026-01-02",
        );
        f.provider("newer", "active", "codex-native", "codex", "2026-01-04");
        f.provider(
            "older",
            "fallback",
            "old-native",
            "claudeAgent",
            "2026-01-02",
        );
        f.provider("latest", "fallback", "new-native", "codex", "2026-01-03");
        f.provider("empty", "fallback", "", "codex", "2026-01-04");
        f.message("active", "user", "first", "2026-01-02T00:00:00Z");
        f.message("active", "user", "newest", "2026-01-03T00:00:00Z");
        f.message("active", "assistant", "later", "2026-01-04T00:00:00Z");
        let rows = T3.sessions(&f.ctx, None, false).unwrap();
        let row = rows.iter().find(|r| r["id"] == "active").unwrap();
        // Built with the platform's separator: Windows prints `~\.t3\userdata`.
        let userdata = f.ctx.home.join(".t3").join("userdata");
        let home = f.ctx.tilde(&userdata);
        let path = format!(
            "{}#thread=active",
            f.ctx.tilde(&userdata.join("statev2.sqlite"))
        );
        assert_eq!(
            *row,
            json!({"harness":"t3","home":home,"id":"active","parent":null,"kind":"main","cwd":"/worktree","branch":"feat/test","title":"V2 title","first_prompt":null,"started":"2026-01-01T00:00:00Z","updated":"2026-01-03T00:00:00Z","model":"model","path":path,"provider_instance":"column-instance","provider":"claude","provider_id":"claude-native","status":null,"archived":true})
        );
        let fallback = rows.iter().find(|r| r["id"] == "fallback").unwrap();
        assert_eq!(fallback["cwd"], "/repo");
        assert_eq!(fallback["provider_instance"], "payload-instance");
        assert_eq!(fallback["provider"], "codex");
        assert_eq!(fallback["provider_id"], "new-native");
        assert_eq!(fallback["updated"], "2026-01-05T00:00:00Z");
        let none = rows.iter().find(|r| r["id"] == "none").unwrap();
        assert!(none["provider_id"].is_null());
        assert!(none["provider"].is_null());
        assert_eq!(
            T3.homes(&f.ctx),
            vec![T3::path_v2(&f.ctx).parent().unwrap().to_path_buf()]
        );
    }

    #[test]
    fn v2_merges_v1_shadows_duplicate_and_skips_deleted() {
        let f = Fixture::new();
        let _v1 = f.v1();
        f.thread("shadow");
        f.thread("deleted");
        f.con.execute("UPDATE orchestration_v2_projection_threads SET deleted_at='2026-01-06' WHERE thread_id='deleted'", []).unwrap();
        let rows = T3.sessions(&f.ctx, None, false).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().find(|r| r["id"] == "shadow").unwrap()["title"],
            "V2 title"
        );
        assert_eq!(
            rows.iter().find(|r| r["id"] == "legacy").unwrap()["title"],
            "V1 title"
        );
        f.con.execute("UPDATE orchestration_v2_projection_threads SET deleted_at='2026-01-06' WHERE thread_id='shadow'", []).unwrap();
        assert_eq!(T3.sessions(&f.ctx, None, false).unwrap().len(), 1);
        let stores = T3.where_info(&f.ctx).unwrap();
        assert_eq!(stores.len(), 2);
        let v2 = f.ctx.tilde(
            &f.ctx
                .home
                .join(".t3")
                .join("userdata")
                .join("statev2.sqlite"),
        );
        assert_eq!(stores[0]["path"], json!(v2));
        assert_eq!(stores[0]["sessions"], 0);
        assert_eq!(stores[1]["sessions"], 2);
    }

    #[test]
    fn v2_status_active_runs_and_last_ordinal() {
        let f = Fixture::new();
        for status in [
            "preparing",
            "starting",
            "running",
            "waiting",
            "queued",
            "completed",
            "interrupted",
            "failed",
            "cancelled",
            "rolled_back",
        ] {
            f.thread(status);
            f.con
                .execute(
                    "INSERT INTO orchestration_v2_projection_runs VALUES(?,1,?)",
                    params![status, status],
                )
                .unwrap();
            f.con
                .execute(
                    "INSERT INTO orchestration_v2_projection_runs VALUES(?,2,'completed')",
                    [status],
                )
                .unwrap();
        }
        f.thread("last");
        f.con.execute_batch("INSERT INTO orchestration_v2_projection_runs VALUES('last',9,'failed'); INSERT INTO orchestration_v2_projection_runs VALUES('last',3,'completed');").unwrap();
        f.thread("no-run");
        let rows = T3.sessions(&f.ctx, None, false).unwrap();
        for row in &rows {
            let expected = match s(row, "id") {
                "preparing" | "starting" | "running" | "waiting" => json!("running"),
                "last" => json!("failed"),
                "no-run" => Value::Null,
                _ => json!("completed"),
            };
            assert_eq!(row["status"], expected);
        }
        assert_eq!(T3.running(&f.ctx).unwrap().len(), 4);
        for status in [
            "queued",
            "completed",
            "interrupted",
            "failed",
            "cancelled",
            "rolled_back",
        ] {
            f.con
                .execute(
                    "DELETE FROM orchestration_v2_projection_runs WHERE thread_id=? AND ordinal=2",
                    [status],
                )
                .unwrap();
            assert_eq!(T3.resolve(&f.ctx, status).unwrap()[0]["status"], status);
        }
    }

    #[test]
    fn v2_since_uses_latest_user_message_and_filters_after_shadowing() {
        let f = Fixture::new();
        let _v1 = f.v1();
        f.thread("shadow");
        f.message("shadow", "user", "old prompt", "2026-01-02T00:00:00Z");
        f.thread("new");
        let since = parse_ts(&json!("2026-01-03T00:00:00Z")).unwrap();
        let rows = T3.sessions(&f.ctx, Some(since), false).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["id"] != "shadow"));
        let since = parse_ts(&json!("2026-01-05T00:00:00Z")).unwrap();
        assert_eq!(T3.sessions(&f.ctx, Some(since), false).unwrap().len(), 2);
    }

    #[test]
    fn v2_resolve_thread_and_historical_native_prefix_deduplicates() {
        let f = Fixture::new();
        f.thread("thread-123");
        f.con
            .execute(
                "UPDATE orchestration_v2_projection_threads SET active_provider_thread_id='active'",
                [],
            )
            .unwrap();
        f.provider("active", "thread-123", "current", "codex", "2026-01-05");
        f.provider(
            "old",
            "thread-123",
            "historical-native",
            "claudeAgent",
            "2026-01-02",
        );
        f.provider(
            "old-copy",
            "thread-123",
            "historical-native-copy",
            "claudeAgent",
            "2026-01-03",
        );
        assert_eq!(T3.resolve(&f.ctx, "thread-").unwrap().len(), 1);
        let rows = T3.resolve(&f.ctx, "historical").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "thread-123");
        assert_eq!(rows[0]["provider_id"], "current");
        assert!(T3.resolve(&f.ctx, "native").unwrap().is_empty());
    }

    #[test]
    fn v2_events_messages_tools_errors_in_time_order() {
        let f = Fixture::new();
        f.thread("events");
        f.message("events", "assistant", "answer", "2026-01-04T00:00:00Z");
        f.message("events", "user", "prompt", "2026-01-01T00:00:00Z");
        f.message("events", "system", "ignored", "2026-01-01T00:00:00Z");
        f.item("events","dynamic_tool","completed","2026-01-09T00:00:00Z",json!({"toolName":"exec","title":"Command","input":{"cmd":"ls"},"completedAt":"2026-01-02T00:00:00Z"}));
        f.item(
            "events",
            "error",
            "failed",
            "2026-01-03T00:00:00Z",
            json!({"failure":{"message":"broken"}}),
        );
        f.item(
            "events",
            "command_execution",
            "failed",
            "2026-01-05T00:00:00Z",
            json!({"title":"Failed command"}),
        );
        for kind in ["file_change", "file_search", "web_search"] {
            f.item(
                "events",
                kind,
                "completed",
                "2026-01-06T00:00:00Z",
                json!({"title":kind}),
            );
        }
        f.item(
            "events",
            "dynamic_tool",
            "running",
            "2026-01-07T00:00:00Z",
            json!({"title":"ignored"}),
        );
        let row = T3.resolve(&f.ctx, "events").unwrap().remove(0);
        let events = T3.events(&f.ctx, &row, None).unwrap().collect::<Vec<_>>();
        assert_eq!(events.len(), 8);
        assert_eq!(events[0].role, "prompt");
        assert_eq!(events[0].text, "prompt");
        assert_eq!(events[1].name.as_deref(), Some("exec"));
        assert_eq!(events[1].input, Some(json!({"cmd":"ls"})));
        assert_eq!(events[1].ts, parse_ts(&json!("2026-01-02T00:00:00Z")));
        assert_eq!(events[2].role, "system");
        assert_eq!(events[2].text, "[runtime.error] broken");
        assert!(events[2].error);
        assert_eq!(events[3].role, "assistant");
        assert_eq!(events[3].text, "answer");
        assert_eq!(events[4].name.as_deref(), Some("Failed command"));
        assert_eq!(events[4].input, Some(json!("Failed command")));
        assert!(events.windows(2).all(|w| w[0].ts <= w[1].ts));
    }

    #[test]
    fn v2_file_without_thread_table_keeps_v1_behavior() {
        let f = Fixture::new();
        let _v1 = f.v1();
        f.con
            .execute("DROP TABLE orchestration_v2_projection_threads", [])
            .unwrap();
        let rows = T3.sessions(&f.ctx, None, false).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| s(r, "path").contains("state.sqlite#thread="))
        );
        assert_eq!(T3.where_info(&f.ctx).unwrap().len(), 1);
    }
}
