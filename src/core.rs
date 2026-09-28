use crate::{
    adapters::StoreAdapter,
    context::Context,
    model::Session,
    util::{parse_ts, s},
};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub harness: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub subagents: bool,
}
#[derive(Debug)]
pub enum SelectionError {
    NotFound(String),
    Ambiguous(Vec<Session>),
    Invalid(String),
}
impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "no session matches {s:?}"),
            Self::Ambiguous(rows) => write!(
                f,
                "AMBIGUOUS: {} sessions match; resolve the full id",
                rows.len()
            ),
            Self::Invalid(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for SelectionError {}
pub fn adapters() -> Vec<Box<dyn StoreAdapter>> {
    vec![
        Box::new(crate::adapters::claude::Claude),
        Box::new(crate::adapters::codex::Codex),
        Box::new(crate::adapters::t3::T3),
        Box::new(crate::adapters::opencode::OpenCode),
        Box::new(crate::adapters::grok::Grok),
    ]
}
pub fn adapter(name: &str) -> Result<Box<dyn StoreAdapter>> {
    adapters()
        .into_iter()
        .find(|a| a.name() == name)
        .ok_or_else(|| anyhow::anyhow!("no adapter for {name}"))
}
pub fn merge_t3(rows: Vec<Session>) -> Vec<Session> {
    let (mut others, t3): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|r| s(r, "harness") != "t3");
    for r in t3 {
        if let Some(target) = others
            .iter_mut()
            .rev()
            .find(|p| s(p, "harness") == s(&r, "provider") && s(p, "id") == s(&r, "provider_id"))
        {
            target["t3_thread"] = r["id"].clone();
            target["t3_title"] = r["title"].clone();
            target["provider_instance"] = r["provider_instance"].clone();
        } else {
            others.push(r);
        }
    }
    others
}
pub fn cwd_match(ctx: &Context, cwd: &str, want: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    if want == "." || want == "here" {
        let here = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        cwd == here || cwd.starts_with(&(here + std::path::MAIN_SEPARATOR_STR))
    } else {
        cwd.to_lowercase()
            .contains(&ctx.expand(want).to_string_lossy().to_lowercase())
    }
}
pub fn gather(ctx: &Context, filters: &Filters) -> Result<Vec<Session>> {
    let names = filters
        .harness
        .as_ref()
        .map(|s| s.split(',').collect::<Vec<_>>());
    let mut rows = Vec::new();
    for a in adapters() {
        if names.as_ref().is_some_and(|n| !n.contains(&a.name())) {
            continue;
        }
        if a.homes(ctx).is_empty() {
            ctx.cover(
                a.name(),
                None,
                "sessions",
                "absent",
                Some("store not found".into()),
            );
        }
        match a.sessions(ctx, filters.since, filters.subagents) {
            Ok(rs) => {
                ctx.remember(&rs);
                rows.extend(rs.into_iter().filter(|r| {
                    filters
                        .cwd
                        .as_ref()
                        .is_none_or(|w| cwd_match(ctx, s(r, "cwd"), w))
                }));
            }
            Err(e) => ctx.diagnostic("store_unreadable", Some(a.name()), None, e.to_string()),
        }
    }
    if names.as_ref().is_none_or(|n| n.contains(&"t3")) {
        rows = merge_t3(rows);
    }
    if let Some(until) = filters.until {
        rows.retain(|r| parse_ts(&r["started"]).unwrap_or(ctx.now) <= until);
    }
    Ok(rows)
}
pub fn resolve_any(ctx: &Context, ident: &str) -> Result<Vec<Session>> {
    let mut id = ident.trim().to_owned();
    let path = ctx.expand(&id);
    if path.exists() {
        let base = path.file_name().unwrap_or_default().to_string_lossy();
        if let Some(rest) = base.strip_prefix("agent-") {
            id = rest.split('.').next().unwrap_or("").into();
        } else {
            let re =
                regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")?;
            if let Some(m) = re.find(&base) {
                id = m.as_str().into();
            }
        }
    }
    if id.chars().count() < 6 {
        return Err(SelectionError::Invalid(
            "id prefix too short; give at least 6 characters".into(),
        )
        .into());
    }
    let mut rows = Vec::new();
    for a in adapters() {
        if a.homes(ctx).is_empty() {
            ctx.cover(
                a.name(),
                None,
                "resolve",
                "absent",
                Some("store not found".into()),
            );
        }
        match a.resolve(ctx, &id) {
            Ok(rs) => {
                ctx.remember(&rs);
                rows.extend(rs)
            }
            Err(e) => ctx.diagnostic("store_unreadable", Some(a.name()), None, e.to_string()),
        }
    }
    Ok(rows)
}
pub fn pick_one(ctx: &Context, id: &str, prefer_provider: bool) -> Result<Session> {
    let mut rows = resolve_any(ctx, id)?;
    if rows.is_empty() {
        return Err(SelectionError::NotFound(id.into()).into());
    }
    if prefer_provider {
        rows = merge_t3(rows);
    }
    if rows.len() > 1 {
        return Err(SelectionError::Ambiguous(rows).into());
    }
    let row = rows.remove(0);
    if prefer_provider && s(&row, "harness") == "t3" && !s(&row, "provider_id").is_empty() {
        let mut p: Vec<_> = resolve_any(ctx, s(&row, "provider_id"))?
            .into_iter()
            .filter(|r| s(r, "harness") == s(&row, "provider"))
            .collect();
        if p.len() == 1 {
            let mut r = p.remove(0);
            r["t3_thread"] = row["id"].clone();
            r["t3_title"] = row["title"].clone();
            r["provider_instance"] = row["provider_instance"].clone();
            return Ok(r);
        }
    }
    Ok(row)
}
pub fn scope_summary(ctx: &Context) -> String {
    adapters()
        .iter()
        .map(|a| {
            let hs = a
                .homes(ctx)
                .iter()
                .map(|p| ctx.tilde(p))
                .collect::<Vec<_>>();
            format!(
                "{}({})",
                a.name(),
                if hs.is_empty() {
                    "absent".into()
                } else {
                    hs.join(", ")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}
pub fn legacy_row(row: &Session) -> Session {
    let mut r = row.clone();
    for key in ["started", "updated"] {
        r[key] = if let Some(d) = parse_ts(&r[key]) {
            Value::String(crate::util::iso(d))
        } else {
            Value::Null
        };
    }
    r
}
pub fn sorted_recent(rows: &mut [Session]) {
    rows.sort_by_key(|r| {
        std::cmp::Reverse(parse_ts(&r["updated"]).or_else(|| parse_ts(&r["started"])))
    });
}
pub fn exclude_self(ctx: &Context, rows: &mut Vec<Session>) {
    rows.retain(|r| {
        ![s(r, "id"), s(r, "parent"), s(r, "provider_id")]
            .iter()
            .any(|id| ctx.self_ids.contains(*id))
    });
}
pub fn absolute_path(ctx: &Context, path: &std::path::Path) -> Result<std::path::PathBuf> {
    Ok(clean(&std::path::absolute(
        ctx.expand(&path.to_string_lossy()),
    )?))
}
pub fn ensure_output_path(ctx: &Context, path: &std::path::Path) -> Result<std::path::PathBuf> {
    let path = ctx.expand(&path.to_string_lossy());
    let normalized = clean(&std::path::absolute(path)?);
    let physical = physical_path(&normalized);
    let mut protected = Vec::new();
    for a in adapters() {
        protected.extend(a.homes(ctx));
    }
    for parts in [
        vec![".hermes"],
        vec![".cursor"],
        vec![".gemini"],
        vec![".grok"],
        vec![".t3"],
        vec![".codex"],
        vec![".claude"],
        vec![".local", "share", "opencode"],
        vec!["Library", "Application Support", "Cursor"],
        vec!["Library", "Application Support", "Claude"],
    ] {
        let mut root = ctx.home.clone();
        for part in parts {
            root.push(part);
        }
        protected.push(root);
    }
    if let Ok(entries) = std::fs::read_dir(&ctx.home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".claude") || name.starts_with(".codex") {
                protected.push(entry.path());
            }
        }
    }
    for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        if let Some(p) = std::env::var_os(key).filter(|p| !p.is_empty()) {
            protected.push(ctx.expand(&p.to_string_lossy()));
        }
    }
    for source in ctx.protected_sources.borrow().iter() {
        protected.push(source.clone());
        if let Some(parent) = source.parent() {
            protected.push(parent.to_path_buf());
        }
    }
    if protected
        .iter()
        .any(|p| physical.starts_with(physical_path(&clean(p))))
    {
        bail!(
            "refusing to write inside a session store: {}",
            normalized.display()
        );
    }
    Ok(normalized)
}
pub fn write_artifacts(ctx: &Context, artifacts: &[(std::path::PathBuf, String)]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    for (path, content) in artifacts {
        let destination = ensure_output_path(ctx, path)?;
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("output has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temp = parent.join(format!(
            ".agent-traces-{}-{stamp}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            let text = if cfg!(windows) {
                content.replace("\n", "\r\n")
            } else {
                content.clone()
            };
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
    }
    Ok(())
}
fn physical_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        if let Some(name) = ancestor.file_name() {
            suffix.push(name.to_os_string());
        }
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => break,
        }
    }
    let mut out = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for s in suffix.into_iter().rev() {
        out.push(s);
    }
    out
}
fn clean(path: &std::path::Path) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                p.pop();
            }
            _ => p.push(c),
        }
    }
    p
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    #[test]
    fn protects_unsupported_and_external_sources() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().join("home");
        std::fs::create_dir_all(&ctx.home).unwrap();
        let hermes = ctx.home.join(".hermes").join("state.db");
        assert!(ensure_output_path(&ctx, &hermes).is_err());
        let external = temp.path().join("external").join("rollout.jsonl");
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::write(&external, "unchanged").unwrap();
        ctx.protect_source(&external);
        assert!(write_artifacts(&ctx, &[(external.clone(), "destructive".into())]).is_err());
        assert_eq!(std::fs::read_to_string(external).unwrap(), "unchanged");
    }
    #[test]
    fn artifact_replace_preserves_prior_target_on_rename_failure() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().join("home");
        std::fs::create_dir_all(&ctx.home).unwrap();
        let target = temp.path().join("output.json");
        std::fs::write(&target, "old").unwrap();
        write_artifacts(&ctx, &[(target.clone(), "new".into())]).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let dir = temp.path().join("existing-directory");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep"), "original").unwrap();
        assert!(write_artifacts(&ctx, &[(dir.clone(), "replace".into())]).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("keep")).unwrap(),
            "original"
        );
        assert!(!std::fs::read_dir(temp.path()).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".agent-traces-")
        }));
    }
}
