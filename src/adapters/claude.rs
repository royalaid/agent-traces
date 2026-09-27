use crate::{
    adapters::StoreAdapter,
    context::Context,
    model::{Event, Session},
    storage::jsonl::{records, tail_lines},
    util::{iso, one_line, opt_s, parse_ts, s},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub struct Claude;
pub(crate) fn entries(ctx: &Context, path: &Path) -> Vec<PathBuf> {
    let read = match fs::read_dir(path) {
        Ok(read) => read,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                ctx.diagnostic("store_unreadable", None, Some(path), e.to_string());
            }
            return vec![];
        }
    };
    let mut out = read
        .filter_map(|e| match e {
            Ok(e) => Some(e.path()),
            Err(e) => {
                ctx.diagnostic("store_unreadable", None, Some(path), e.to_string());
                None
            }
        })
        .collect::<Vec<_>>();
    out.sort();
    out
}
pub(crate) fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
}
pub(crate) fn strip_reminders(text: &str) -> String {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?s)<(?:system-reminder)>.*?</system-reminder>|<context_window_protection>.*?</context_window_protection>|<user-prompt-submit-hook>.*?</user-prompt-submit-hook>").unwrap()
    });
    RE.replace_all(text, "").into_owned()
}
pub fn classify_claude_text(text: &str, rec: &Value) -> (String, String) {
    let cleaned = strip_reminders(text);
    let t = cleaned.trim();
    if t.is_empty() {
        return ("notice".into(), one_line(text, 200));
    }
    if truthy(&rec["isCompactSummary"])
        || t.starts_with("This session is being continued from a previous")
    {
        return ("system".into(), format!("[compaction summary] {t}"));
    }
    let extract = |tag: &str| {
        let re = regex::Regex::new(&format!("(?s)<{tag}>(.*?)</{tag}>")).unwrap();
        re.captures(t).map(|c| c[1].trim().to_owned())
    };
    if t.chars()
        .take(200)
        .collect::<String>()
        .contains("<command-name>")
    {
        let mut cmd = extract("command-name").unwrap_or_else(|| "/?".into());
        if !cmd.starts_with('/') {
            cmd.insert(0, '/');
        }
        return (
            "command".into(),
            format!("{} {}", cmd, extract("command-args").unwrap_or_default())
                .trim()
                .into(),
        );
    }
    if t.starts_with("<bash-input>") {
        return (
            "command".into(),
            format!("!{}", extract("bash-input").unwrap_or_default()),
        );
    }
    if [
        "<task-notification>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<local-command-caveat>",
        "<bash-stdout>",
        "<bash-stderr>",
        "Caveat: The messages below",
        "[Request interrupted",
        "Another Claude session sent a message",
        "[SYSTEM NOTIFICATION",
        "<user-prompt-submit-hook>",
        "<system-reminder>",
        "<cross-session-message",
        "<agent-message",
    ]
    .iter()
    .any(|p| t.starts_with(p))
        || s(rec, "promptSource") == "system"
        || truthy(&rec["isMeta"])
    {
        return ("notice".into(), t.into());
    }
    ("prompt".into(), t.into())
}
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.),
    }
}
pub(crate) fn body(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.into()
    } else if let Some(a) = v.as_array() {
        a.iter()
            .filter(|b| b.is_object())
            .map(|b| s(b, "text"))
            .collect::<Vec<_>>()
            .join("\n")
    } else if !truthy(v) {
        String::new()
    } else {
        crate::util::python_json(v)
    }
}
fn top_files(ctx: &Context, home: &Path) -> Vec<PathBuf> {
    entries(ctx, &home.join("projects"))
        .iter()
        .filter(|p| p.is_dir())
        .flat_map(|p| entries(ctx, p))
        .filter(|p| p.extension().is_some_and(|s| s == "jsonl") && p.is_file())
        .collect()
}
fn agents(ctx: &Context, dir: &Path) -> Vec<PathBuf> {
    let mut out = entries(ctx, dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("agent-"))
                && p.extension().is_some_and(|e| e == "jsonl")
                && p.is_file()
        })
        .collect::<Vec<_>>();
    for w in entries(ctx, &dir.join("workflows")) {
        out.extend(entries(ctx, &w).into_iter().filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("agent-"))
                && p.extension().is_some_and(|e| e == "jsonl")
                && p.is_file()
        }));
    }
    out
}
fn sub_files(ctx: &Context, home: &Path) -> Vec<PathBuf> {
    entries(ctx, &home.join("projects"))
        .iter()
        .filter(|p| p.is_dir())
        .flat_map(|p| entries(ctx, p))
        .filter(|p| p.is_dir())
        .flat_map(|p| agents(ctx, &p.join("subagents")))
        .collect()
}
fn load_cache(ctx: &Context) -> Value {
    fs::read(ctx.cache_dir.join("claude-meta-rust-v2.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}
fn save_cache(ctx: &Context, cache: &Value) {
    use std::io::Write;
    let destination =
        match crate::core::ensure_output_path(ctx, &ctx.cache_dir.join("claude-meta-rust-v2.json"))
        {
            Ok(path) => path,
            Err(_) => return,
        };
    let cache_dir = destination.parent().expect("cache destination has parent");
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let _ = (|| -> std::io::Result<()> {
        fs::create_dir_all(cache_dir)?;
        let temp = cache_dir.join(format!(
            "claude-meta-rust-v2.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = (|| {
            f.write_all(&serde_json::to_vec(cache)?)?;
            drop(f);
            fs::rename(&temp, &destination)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    })();
}
fn extract(ctx: &Context, path: &Path) -> Value {
    let mut m = json!({"cwd":null,"branch":null,"title":null,"first_prompt":null,"started":null,"updated":null,"model":null,"entrypoint":null});
    let (mut ai, mut custom) = (Value::Null, Value::Null);
    for (i, r) in records(ctx, path, 0, None) {
        if !truthy(&m["cwd"]) && truthy(&r["cwd"]) {
            m["cwd"] = r["cwd"].clone();
            m["branch"] = r["gitBranch"].clone();
            m["entrypoint"] = r["entrypoint"].clone();
        }
        if !truthy(&m["started"]) && truthy(&r["timestamp"]) {
            m["started"] = r["timestamp"].clone();
        }
        match s(&r, "type") {
            "ai-title" => ai = r["aiTitle"].clone(),
            "custom-title" => custom = r["customTitle"].clone(),
            "assistant" if !truthy(&m["model"]) => m["model"] = r["message"]["model"].clone(),
            "user" if !truthy(&m["first_prompt"]) => {
                let c = &r["message"]["content"];
                let text = if let Some(a) = c.as_array() {
                    a.iter()
                        .filter(|b| s(b, "type") == "text")
                        .map(|b| s(b, "text"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    s(&r["message"], "content").into()
                };
                if !text.is_empty() {
                    let (role, text) = classify_claude_text(&text, &r);
                    if role == "prompt" || role == "command" {
                        m["first_prompt"] = json!(text.chars().take(600).collect::<String>());
                    }
                }
            }
            _ => {}
        }
        if i > 400
            || (truthy(&m["cwd"])
                && truthy(&m["first_prompt"])
                && truthy(&m["model"])
                && (truthy(&ai) || truthy(&custom)))
        {
            break;
        }
    }
    for r in tail_lines(ctx, path, 262144) {
        if truthy(&r["timestamp"]) {
            m["updated"] = r["timestamp"].clone();
        }
        if s(&r, "type") == "custom-title" {
            custom = r["customTitle"].clone();
        } else if s(&r, "type") == "ai-title" && !truthy(&ai) {
            ai = r["aiTitle"].clone();
        }
    }
    m["title"] = if truthy(&custom) { custom } else { ai };
    if !truthy(&m["updated"]) {
        m["updated"] = json!(mtime(path).map(iso));
    }
    m
}
fn meta(ctx: &Context, path: &Path, home: &Path, sub: bool, cache: &mut Value) -> Option<Value> {
    let st = match fs::metadata(path) {
        Ok(s) => s,
        Err(e) => {
            ctx.diagnostic(
                "store_unreadable",
                Some("claude"),
                Some(path),
                e.to_string(),
            );
            return None;
        }
    };
    let key = path.to_string_lossy().into_owned();
    let modified = st
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("{}:{}", d.as_secs(), d.subsec_nanos()));
    let cached = &cache[&key];
    let m = if cached["size"] == json!(st.len())
        && cached["mtime"] == json!(modified)
        && cached["meta"].is_object()
    {
        cached["meta"].clone()
    } else {
        // Failed reads must be retried so a cache hit cannot hide incomplete metadata.
        let diagnostics_before = ctx.diagnostics.borrow().len();
        let m = extract(ctx, path);
        if ctx.diagnostics.borrow().len() == diagnostics_before {
            cache[&key] = json!({"size":st.len(),"mtime":modified,"meta":m});
        } else if let Some(entries) = cache.as_object_mut() {
            entries.remove(&key);
        }
        m
    };
    let mut row = json!({"harness":"claude","home":ctx.tilde(home),"id":path.file_stem().unwrap_or_default().to_string_lossy(),"parent":null,"kind":"main","cwd":m["cwd"],"branch":m["branch"],"title":m["title"],"first_prompt":m["first_prompt"],"started":m["started"],"updated":m["updated"],"model":m["model"],"entrypoint":m["entrypoint"],"path":key,"agent_type":null});
    if sub {
        let root = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "subagents"))?;
        let workflow = path
            .parent()
            .filter(|p| *p != root)
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned());
        let info = fs::read(path.with_extension("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        let name = s(&info, "name");
        let desc = s(&info, "description");
        let mut title = if !name.is_empty() {
            name.to_owned()
        } else if !desc.is_empty() {
            desc.to_owned()
        } else {
            s(&m, "title").into()
        };
        if !name.is_empty() && !desc.is_empty() {
            title = format!("{name}: {desc}");
        }
        if let Some(w) = &workflow
            && name.is_empty()
            && desc.is_empty()
        {
            title = format!("{w}: {}", one_line(s(&m, "first_prompt"), 90));
        }
        row["id"] = json!(
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .strip_prefix("agent-")
                .unwrap_or_default()
        );
        row["parent"] = json!(
            root.parent()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy()
        );
        row["kind"] = json!(if workflow.is_some() {
            "workflow-agent"
        } else {
            "subagent"
        });
        row["title"] = json!(title);
        row["agent_type"] = info["agentType"].clone();
    }
    Some(row)
}
fn dedupe(rows: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for r in rows {
        let key = (s(&r, "parent").to_owned(), s(&r, "id").to_owned());
        if let Some(&i) = seen.get(&key) {
            let rank = |r: &Value| {
                let p = Path::new(s(r, "path"));
                (fs::metadata(p).map(|m| m.len()).unwrap_or(0), mtime(p))
            };
            let mut copies = out[i]["copies"].as_array().cloned().unwrap_or_default();
            if rank(&r) > rank(&out[i]) {
                copies.push(out[i]["home"].clone());
                out[i] = r;
            } else {
                copies.push(r["home"].clone());
            }
            copies.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            copies.dedup();
            out[i]["copies"] = json!(copies);
        } else {
            seen.insert(key, out.len());
            out.push(r);
        }
    }
    out
}
impl StoreAdapter for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }
    fn homes(&self, ctx: &Context) -> Vec<PathBuf> {
        let mut c = vec![];
        if let Some(p) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
            c.push(PathBuf::from(p));
        }
        c.push(ctx.home.join(".claude"));
        let roots = entries(ctx, &ctx.home)
            .into_iter()
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(".claude"))
            })
            .collect::<Vec<_>>();
        c.extend(roots.clone());
        for root in roots {
            for child in entries(ctx, &root) {
                let p = child.join("config");
                if p.join("projects").is_dir() {
                    c.push(p);
                }
            }
        }
        if let Some(inst) = ctx.t3_settings()["providerInstances"].as_object() {
            for v in inst.values() {
                if s(v, "driver") == "claudeAgent" {
                    let p = s(&v["config"], "homePath").trim();
                    if !p.is_empty() {
                        c.push(ctx.expand(p));
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        c.into_iter()
            .filter(|p| {
                p.join("projects").is_dir()
                    && seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            })
            .collect()
    }
    fn sessions(
        &self,
        ctx: &Context,
        since: Option<DateTime<Utc>>,
        subagents: bool,
    ) -> Result<Vec<Session>> {
        let mut cache = load_cache(ctx);
        let mut rows = vec![];
        for home in self.homes(ctx) {
            let mut files = top_files(ctx, &home);
            if subagents {
                files.extend(sub_files(ctx, &home));
            }
            for p in files {
                if since.is_some_and(|cut| mtime(&p).is_none_or(|t| t < cut)) {
                    continue;
                }
                let sub = p.components().any(|c| c.as_os_str() == "subagents");
                if let Some(r) = meta(ctx, &p, &home, sub, &mut cache) {
                    rows.push(r);
                }
            }
            ctx.cover("claude", Some(&home), "sessions", "read", None);
        }
        save_cache(ctx, &cache);
        Ok(dedupe(rows))
    }
    fn resolve(&self, ctx: &Context, ident: &str) -> Result<Vec<Session>> {
        let mut cache: Option<Value> = None;
        let mut rows = vec![];
        for home in self.homes(ctx) {
            for (sub, files) in [
                (false, top_files(ctx, &home)),
                (true, sub_files(ctx, &home)),
            ] {
                for p in files {
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                    let id = if sub {
                        stem.strip_prefix("agent-").unwrap_or_default()
                    } else {
                        &stem
                    };
                    if id.starts_with(ident)
                        && let Some(r) = meta(
                            ctx,
                            &p,
                            &home,
                            sub,
                            cache.get_or_insert_with(|| load_cache(ctx)),
                        )
                    {
                        rows.push(r);
                    }
                }
            }
        }
        if let Some(cache) = cache {
            save_cache(ctx, &cache);
        }
        Ok(dedupe(rows))
    }
    fn children(&self, ctx: &Context, row: &Session) -> Result<Vec<Session>> {
        if s(row, "kind") != "main" {
            return Ok(vec![]);
        }
        let mut cache = load_cache(ctx);
        let mut paths = agents(
            ctx,
            &Path::new(s(row, "path"))
                .with_extension("")
                .join("subagents"),
        );
        paths.sort();
        let rows = paths
            .iter()
            .filter_map(|p| meta(ctx, p, &ctx.expand(s(row, "home")), true, &mut cache))
            .collect();
        save_cache(ctx, &cache);
        Ok(dedupe(rows))
    }
    fn events<'a>(
        &self,
        ctx: &'a Context,
        row: &'a Session,
        needles: Option<&[String]>,
    ) -> Result<Box<dyn Iterator<Item = Event> + 'a>> {
        let mut seen = HashSet::new();
        Ok(Box::new(
            records(ctx, Path::new(s(row, "path")), 0, needles).flat_map(move |(_, r)| {
                let uid = s(&r, "uuid");
                if !uid.is_empty() && !seen.insert(uid.to_owned()) {
                    return vec![];
                }
                record_events(&r)
            }),
        ))
    }
    fn where_info(&self, ctx: &Context) -> Result<Vec<Value>> {
        let homes = self.homes(ctx);
        let mut names: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        for h in &homes {
            for f in top_files(ctx, h) {
                names
                    .entry(
                        f.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    )
                    .or_default()
                    .insert(h.clone());
            }
        }
        let shared = names.values().filter(|v| v.len() > 1).count();
        Ok(homes.iter().map(|h|{let files=top_files(ctx, h);json!({"harness":"claude","path":ctx.tilde(h),"sessions":files.len(),"newest":files.iter().filter_map(|p|mtime(p)).max().map(iso),"index":"projects/<cwd-slug>/<session>.jsonl (no index; scan files)","notes":format!("history.jsonl is a typed-prompt log only (misses SDK/T3/subagent prompts){}",if shared>0{format!("; {shared} sessions exist as copies in more than one home (listed once)")}else{String::new()})})}).collect())
    }
}
fn record_events(r: &Value) -> Vec<Event> {
    let ts = parse_ts(&r["timestamp"]);
    let mut out = vec![];
    match s(r, "type") {
        "user" => {
            let c = &r["message"]["content"];
            if let Some(t) = c.as_str() {
                let (role, text) = classify_claude_text(t, r);
                out.push(Event::new(&role, text, ts));
            } else if let Some(a) = c.as_array() {
                let mut texts = vec![];
                for b in a {
                    match s(b, "type") {
                        "tool_result" => {
                            let content = &b["content"];
                            let text = if let Some(a) = content.as_array() {
                                a.iter()
                                    .filter(|b| s(b, "type") == "text")
                                    .map(|b| s(b, "text"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            } else {
                                body(content)
                            };
                            let mut e = Event::new("result", text, ts);
                            e.error = truthy(&b["is_error"]);
                            e.tool_use_id = opt_s(b, "tool_use_id");
                            out.push(e);
                        }
                        "text" => texts.push(s(b, "text").to_owned()),
                        "image" => texts.push("[image]".into()),
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    let (role, text) = classify_claude_text(&texts.join("\n"), r);
                    out.push(Event::new(&role, text, ts));
                }
            }
        }
        "assistant" => {
            if let Some(a) = r["message"]["content"].as_array() {
                for b in a {
                    if s(b, "type") == "text" && !s(b, "text").trim().is_empty() {
                        let mut e = Event::new("assistant", s(b, "text"), ts);
                        e.model = opt_s(&r["message"], "model");
                        out.push(e);
                    } else if s(b, "type") == "tool_use" {
                        let mut e = Event::new("tool", "", ts);
                        e.name = opt_s(b, "name");
                        e.input = b.get("input").cloned();
                        e.id = opt_s(b, "id");
                        out.push(e);
                    }
                }
            }
            if truthy(&r["isApiErrorMessage"]) || truthy(&r["error"]) {
                let err = if truthy(&r["error"]) {
                    r["error"].clone()
                } else {
                    json!("")
                };
                let mut e = Event::new(
                    "system",
                    format!(
                        "[api error] {}",
                        one_line(&crate::util::python_json(&err), 300)
                    ),
                    ts,
                );
                e.error = true;
                out.push(e);
            }
        }
        "system" => {
            let sub = s(r, "subtype");
            if sub == "compact_boundary" {
                let m = &r["compactMetadata"];
                let py = |v: &Value| if v.is_null() { "None".into() } else { body(v) };
                out.push(Event::new(
                    "system",
                    format!(
                        "[compacted: {}, {} tokens before]",
                        py(&m["trigger"]),
                        py(&m["preTokens"])
                    ),
                    ts,
                ));
            } else if s(r, "level") == "error" {
                let mut e = Event::new(
                    "system",
                    format!(
                        "[error] {}",
                        one_line(
                            if s(r, "content").is_empty() {
                                sub
                            } else {
                                s(r, "content")
                            },
                            300
                        )
                    ),
                    ts,
                );
                e.error = true;
                out.push(e);
            }
        }
        _ => {}
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_user_intent() {
        assert_eq!(
            classify_claude_text("<system-reminder>x</system-reminder> hello", &json!({})),
            ("prompt".into(), "hello".into())
        );
        assert_eq!(
            classify_claude_text(
                "<command-name>compact</command-name><command-args>why</command-args>",
                &json!({})
            ),
            ("command".into(), "/compact why".into())
        );
        assert_eq!(
            classify_claude_text("ignored", &json!({"isMeta":true})).0,
            "notice"
        );
    }
    #[test]
    fn event_order_and_results() {
        let events = record_events(
            &json!({"type":"user","message":{"content":[{"type":"text","text":"hello"},{"type":"tool_result","content":[{"type":"text","text":"failure"}],"is_error":true,"tool_use_id":"x"},{"type":"image"}]}}),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].role, "result");
        assert!(events[0].error);
        assert_eq!(events[1].text, "hello\n[image]");
    }
    #[test]
    fn discovery_children_cache() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        ctx.cache_dir = temp.path().join("cache");
        let project = ctx.home.join(".claude").join("projects").join("proj");
        fs::create_dir_all(
            project
                .join("abc")
                .join("subagents")
                .join("workflows")
                .join("wf"),
        )
        .unwrap();
        fs::write(project.join("abc.jsonl"),"{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/work\",\"message\":{\"content\":\"hello\"}}\n").unwrap();
        fs::write(
            project
                .join("abc")
                .join("subagents")
                .join("workflows")
                .join("wf")
                .join("agent-child.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"task\"}}\n",
        )
        .unwrap();
        let rows = Claude.sessions(&ctx, None, false).unwrap();
        let parent = rows.iter().find(|r| s(r, "id") == "abc").unwrap();
        assert_eq!(parent["first_prompt"], "hello");
        let children = Claude.children(&ctx, parent).unwrap();
        assert_eq!(children[0]["kind"], "workflow-agent");
        assert_eq!(children[0]["parent"], "abc");
        assert_eq!(children[0]["title"], "wf: task");
        assert_eq!(Claude.resolve(&ctx, "chi").unwrap()[0]["id"], "child");
    }
    #[test]
    fn duplicate_prefers_largest_and_cache_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        ctx.cache_dir = temp.path().join("cache");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "larger").unwrap();
        let rows = dedupe(vec![
            json!({"id":"same","parent":null,"path":a,"home":"~/one"}),
            json!({"id":"same","parent":null,"path":b,"home":"~/two"}),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["home"], "~/two");
        assert_eq!(rows[0]["copies"], json!(["~/one"]));
        save_cache(&ctx, &json!({"one":1}));
        save_cache(&ctx, &json!({"two":2}));
        assert_eq!(load_cache(&ctx), json!({"two":2}));
    }
    #[test]
    fn malformed_metadata_remains_incomplete_after_cache_reload() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        ctx.cache_dir = temp.path().join("cache");
        let home = temp.path().join(".claude");
        let project = home.join("projects/project");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("broken.jsonl");
        fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"recoverable prompt\"}}\n{broken\n",
        )
        .unwrap();
        for _ in 0..2 {
            ctx.diagnostics.borrow_mut().clear();
            let mut cache = load_cache(&ctx);
            let row = meta(&ctx, &path, &home, false, &mut cache).unwrap();
            assert_eq!(row["first_prompt"], "recoverable prompt");
            assert!(
                ctx.diagnostics
                    .borrow()
                    .iter()
                    .any(|d| d.code == "malformed_record")
            );
            assert!(
                !cache
                    .as_object()
                    .unwrap()
                    .contains_key(path.to_string_lossy().as_ref())
            );
            save_cache(&ctx, &cache);
        }
    }

    #[test]
    fn ignores_legacy_cache_that_could_hide_incomplete_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        ctx.cache_dir = temp.path().join("cache");
        fs::create_dir_all(&ctx.cache_dir).unwrap();
        fs::write(
            ctx.cache_dir.join("claude-meta-rust-v1.json"),
            "{\"previously_broken\":{\"meta\":{}}}",
        )
        .unwrap();
        assert_eq!(load_cache(&ctx), json!({}));
    }
    #[test]
    fn resolving_missing_id_does_not_create_cache() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().into();
        ctx.cache_dir = temp.path().join("not-created");
        let project = ctx.home.join(".claude/projects/project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("different-session.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n",
        )
        .unwrap();
        assert!(
            Claude
                .resolve(&ctx, "__missing-fixture-id__")
                .unwrap()
                .is_empty()
        );
        assert!(
            !ctx.cache_dir.exists(),
            "a resolution miss must not persist an unchanged cache"
        );
    }
}
