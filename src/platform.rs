use crate::{
    context::Context,
    core,
    util::{fmt_ts, label, one_line, parse_ts, redact, s},
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub alive: bool,
    pub start_time: Option<DateTime<Utc>>,
}
pub fn query_process(pid: u32) -> Result<ProcessInfo> {
    if let Some(path) = std::env::var_os("AGENT_TRACES_PROCESS_SNAPSHOT") {
        let data: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let rows = data
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("process snapshot must be an array"))?;
        return Ok(rows
            .iter()
            .find(|r| r["pid"].as_u64() == Some(pid as u64))
            .map(|r| ProcessInfo {
                alive: r["alive"].as_bool().unwrap_or(false),
                start_time: parse_ts(&r["start_time"]),
            })
            .unwrap_or(ProcessInfo {
                alive: false,
                start_time: None,
            }));
    }
    native_process(pid)
}
#[cfg(windows)]
fn native_process(pid: u32) -> Result<ProcessInfo> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    // SAFETY: query-only access, handles closed on every return path; output pointers valid.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(87) {
                return Ok(ProcessInfo {
                    alive: false,
                    start_time: None,
                });
            }
            return Err(e.into());
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(handle);
            return Err(e.into());
        }
        let mut created: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let has_time =
            GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        let ticks = ((created.dwHighDateTime as u64) << 32) | (created.dwLowDateTime as u64);
        let start_time = if has_time {
            let unix = ticks.saturating_sub(116444736000000000);
            Utc.timestamp_opt((unix / 10000000) as i64, ((unix % 10000000) * 100) as u32)
                .single()
        } else {
            None
        };
        Ok(ProcessInfo {
            alive: code == 259,
            start_time,
        })
    }
}
#[cfg(unix)]
fn native_process(pid: u32) -> Result<ProcessInfo> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let id = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[id]),
        true,
        ProcessRefreshKind::nothing(),
    );
    Ok(system
        .process(id)
        .map(|p| ProcessInfo {
            alive: !matches!(
                p.status(),
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            ),
            start_time: Utc.timestamp_opt(p.start_time() as i64, 0).single(),
        })
        .unwrap_or(ProcessInfo {
            alive: false,
            start_time: None,
        }))
}
// Claude on Windows records exact FILETIME ticks; POSIX ps has second precision.
fn recorded_start_matches(record: &str, actual: DateTime<Utc>) -> Option<bool> {
    if let Ok(ticks) = record.parse::<u64>()
        && ticks >= 116444736000000000
    {
        let unix = ticks - 116444736000000000;
        return Utc
            .timestamp_opt((unix / 10000000) as i64, ((unix % 10000000) * 100) as u32)
            .single()
            .map(|recorded| recorded == actual);
    }
    let normalized = record.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Ok(recorded) = chrono::NaiveDateTime::parse_from_str(&normalized, "%a %b %e %H:%M:%S %Y")
    {
        return Some(recorded.and_utc().timestamp() == actual.timestamp());
    }
    parse_ts(&Value::String(record.into()))
        .map(|recorded| recorded.timestamp() == actual.timestamp())
}
pub struct LiveResult {
    pub observations: Vec<Value>,
    pub text: String,
    pub stderr: String,
    pub found: bool,
}
pub fn live(ctx: &Context) -> Result<LiveResult> {
    let claude = core::adapter("claude")?;
    let mut observations = vec![];
    let mut text = String::new();
    let mut count = 0;
    let mut others = 0;
    let homes = claude.homes(ctx);
    if homes.is_empty() {
        ctx.cover(
            "claude",
            None,
            "live",
            "absent",
            Some("store not found".into()),
        );
    }
    for home in homes {
        let dir = home.join("sessions");
        if !dir.exists() {
            ctx.cover(
                "claude",
                Some(&home),
                "live",
                "absent",
                Some("no PID records".into()),
            );
            continue;
        }
        ctx.cover("claude", Some(&home), "live", "read", None);
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let d: Value = match std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
            {
                Some(v) => v,
                None => {
                    ctx.diagnostic(
                        "malformed_record",
                        Some("claude"),
                        Some(&path),
                        "cannot parse PID record",
                    );
                    continue;
                }
            };
            let pid = d["pid"]
                .as_u64()
                .or_else(|| d["pid"].as_str().and_then(|x| x.parse().ok()))
                .filter(|p| *p > 0 && *p <= u32::MAX as u64);
            let Some(pid) = pid else {
                continue;
            };
            let sid = s(&d, "sessionId");
            if sid.is_empty() {
                continue;
            }
            let process = query_process(pid as u32);
            let mut confidence = "process_exists_identity_unverified";
            let mut start = None;
            let mut status = "running";
            match process {
                Ok(p) => {
                    if !p.alive {
                        continue;
                    }
                    start = p.start_time;
                    if let Some(t) = start {
                        let record = s(&d, "procStart");
                        if !record.is_empty() {
                            match recorded_start_matches(record, t) {
                                Some(true) => confidence = "process_identity_verified",
                                Some(false) => continue,
                                None => ctx.diagnostic(
                                    "process_unverifiable", Some("claude"), Some(&path),
                                    "unrecognized process-start format; PID exists but identity is unverified",
                                ),
                            }
                        }
                    }
                }
                Err(e) => {
                    ctx.diagnostic(
                        "process_unverifiable",
                        Some("claude"),
                        Some(&path),
                        e.to_string(),
                    );
                    confidence = "unverifiable";
                    status = "unknown";
                }
            }
            let rows = claude.resolve(ctx, sid)?;
            ctx.remember(&rows);
            let row = rows.iter().find(|r| s(r, "kind") == "main");
            let last = row.and_then(|r| parse_ts(&r["updated"]));
            let mut last_prompt = String::new();
            if let Some(r) = row {
                let records = crate::storage::jsonl::tail_lines(
                    ctx,
                    std::path::Path::new(s(r, "path")),
                    1048576,
                );
                for rec in records.iter().rev() {
                    if s(rec, "type") == "user" && !rec["isSidechain"].as_bool().unwrap_or(false) {
                        let c = &rec["message"]["content"];
                        let t = c.as_str().map(str::to_owned).unwrap_or_else(|| {
                            c.as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter(|b| s(b, "type") == "text")
                                        .map(|b| s(b, "text"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default()
                        });
                        if !t.is_empty() {
                            // Use the adapter classification on this tail through its public helper when available.
                            let (role, clean) =
                                crate::adapters::claude::classify_claude_text(&t, rec);
                            if role == "prompt" || role == "command" {
                                last_prompt = clean;
                                break;
                            }
                        }
                    }
                }
            }
            let is_self = ctx.self_ids.contains(sid);
            let session = row
                .map(|r| crate::contract::session(ctx, r))
                .unwrap_or(Value::Null);
            let identity=row.map(|r|crate::contract::identity(ctx,r)).unwrap_or_else(||json!({"host_id":ctx.host_id.as_deref().unwrap_or("local"),"store_id":home.to_string_lossy(),"harness":"claude","id":sid}));
            observations.push(json!({"identity":identity,"session":session,"evidence":"claude_pid","pid":pid,"process_started":start.map(|t|t.to_rfc3339_opts(chrono::SecondsFormat::Micros,true)),"observed_status":status,"confidence":confidence,"is_self":is_self,"last_activity":last.map(|t|t.to_rfc3339_opts(chrono::SecondsFormat::Micros,true)),"last_prompt":if last_prompt.is_empty(){Value::Null}else{Value::String(redact(&one_line(&last_prompt,160)))}}));
            if status == "running" {
                let idle = last
                    .filter(|t| ctx.now.signed_duration_since(*t) > chrono::Duration::hours(12))
                    .map(|t| format!("  IDLE {}d", ctx.now.signed_duration_since(t).num_days()))
                    .unwrap_or_default();
                let title = row.map(label).unwrap_or_default();
                let entrypoint = if !s(&d, "entrypoint").is_empty() {
                    s(&d, "entrypoint")
                } else {
                    s(&d, "kind")
                };
                text.push_str(&format!(
                    "claude  {:<18} pid={:<6} {}  {}  last activity {}{}\n    {}  {}{}\n",
                    ctx.tilde(&home),
                    pid,
                    sid,
                    entrypoint,
                    fmt_ts(last),
                    idle,
                    ctx.tilde(&ctx.expand(s(&d, "cwd"))),
                    one_line(&title, 90),
                    if is_self { "  [THIS SESSION]" } else { "" }
                ));
                if !last_prompt.is_empty() {
                    text.push_str(&format!(
                        "    last prompt: {}\n",
                        redact(&one_line(&last_prompt, 160))
                    ));
                }
                count += 1;
                if !is_self {
                    others += 1;
                }
            }
        }
    }
    let t3 = core::adapter("t3")?;
    if t3.homes(ctx).is_empty() {
        ctx.cover("t3", None, "live", "absent", Some("store not found".into()));
    }
    let rows = t3.sessions(ctx, None, true)?;
    ctx.remember(&rows);
    // Preserve the reference SQL filter and its row order; keep all discovered
    // rows above for provider aliases even when their runtime is not running.
    let running = crate::adapters::t3::T3.running(ctx)?;
    for row in &running {
        let mine = ctx.self_ids.contains(s(row, "provider_id"));
        observations.push(json!({"identity":crate::contract::identity(ctx,row),"session":crate::contract::session(ctx,row),"evidence":"t3_runtime","pid":null,"process_started":null,"observed_status":"running","confidence":"provider_reported","is_self":mine,"last_activity":crate::contract::timestamp(&row["updated"]),"last_prompt":null}));
        text.push_str(&format!(
            "t3      {:<18} {}  -> {}:{}\n    {}  {}{}\n",
            "",
            s(row, "id"),
            s(row, "provider_instance"),
            s(row, "provider_id"),
            ctx.tilde(&ctx.expand(s(row, "cwd"))),
            one_line(s(row, "title"), 90),
            if mine { "  [THIS SESSION]" } else { "" }
        ));
    }
    for h in ["codex", "opencode", "grok", "hermes"] {
        ctx.cover(
            h,
            None,
            "live",
            "unsupported",
            Some("no liveness record adapter".into()),
        );
    }
    text.push_str(&format!("\n{count} live Claude Code sessions, {others} besides this one. T3 rows above repeat Claude or Codex sessions under their T3 thread ids.\n"));
    for observation in &mut observations {
        let known = ctx.known_sessions.borrow();
        if let Some(row) = known
            .iter()
            .find(|r| crate::contract::identity(ctx, r) == observation["identity"])
        {
            observation["session"] = crate::contract::session(ctx, row);
        }
    }
    Ok(LiveResult{found: observations.iter().any(|o|o["observed_status"]=="running"),observations,text,stderr:"claude: live pid in <config>/sessions/<pid>.json; t3: provider_session_runtime.status='running'. Codex, OpenCode, and Grok have no liveness record; use `ls --since 30m`.\n".into()})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_filetime_identity_and_pid_reuse() {
        let actual = Utc.timestamp_opt(1790459380, 464171200).unwrap();
        assert_eq!(
            recorded_start_matches("134349329804641712", actual),
            Some(true)
        );
        assert_eq!(
            recorded_start_matches("134349329804641713", actual),
            Some(false)
        );
        assert_eq!(recorded_start_matches("unknown-format", actual), None);
        assert_eq!(
            recorded_start_matches("2026-09-26T21:49:40Z", actual),
            Some(true)
        );
    }
    #[test]
    fn query_current_process_is_read_only_and_alive() {
        let p = native_process(std::process::id()).unwrap();
        assert!(p.alive);
        assert!(p.start_time.is_some());
    }
}
