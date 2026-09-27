use crate::model::{Coverage, Diagnostic};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub struct Context {
    pub home: PathBuf,
    pub cache_dir: PathBuf,
    pub out_dir: PathBuf,
    pub now: DateTime<Utc>,
    pub host_id: Option<String>,
    pub self_ids: HashSet<String>,
    pub diagnostics: RefCell<Vec<Diagnostic>>,
    pub coverage: RefCell<Vec<Coverage>>,
    pub skipped: Cell<usize>,
    pub known_sessions: RefCell<Vec<Value>>,
    known_keys: RefCell<HashSet<(String, String, String)>>,
    pub(crate) known_by_id: RefCell<HashMap<String, Vec<usize>>>,
    pub(crate) known_t3_by_provider: RefCell<HashMap<(String, String), Vec<usize>>>,
    pub(crate) canonical_paths: RefCell<HashMap<PathBuf, String>>,
    pub protected_sources: RefCell<HashSet<PathBuf>>,
}
impl Context {
    pub fn from_env() -> Self {
        let home = std::env::var_os("AGENT_TRACES_HOME")
            .or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let cache_dir = std::env::var_os("AGENT_TRACES_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache").join("agent-traces"));
        let out_dir = std::env::var_os("AGENT_TRACES_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("agent-traces"));
        let now = std::env::var("AGENT_TRACES_NOW")
            .ok()
            .and_then(|s| crate::util::parse_ts(&Value::String(s)))
            .unwrap_or_else(Utc::now);
        Self {
            home,
            cache_dir,
            out_dir,
            now,
            host_id: std::env::var("AGENT_TRACES_HOST_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            self_ids: [
                "CLAUDE_CODE_SESSION_ID",
                "CODEX_THREAD_ID",
                "CODEX_SESSION_ID",
            ]
            .into_iter()
            .filter_map(|k| std::env::var(k).ok())
            .filter(|s| !s.is_empty())
            .collect(),
            diagnostics: RefCell::new(vec![]),
            coverage: RefCell::new(vec![]),
            skipped: Cell::new(0),
            known_sessions: RefCell::new(vec![]),
            known_keys: RefCell::new(HashSet::new()),
            known_by_id: RefCell::new(HashMap::new()),
            known_t3_by_provider: RefCell::new(HashMap::new()),
            canonical_paths: RefCell::new(HashMap::new()),
            protected_sources: RefCell::new(HashSet::new()),
        }
    }
    pub fn protect_source(&self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.protected_sources.borrow_mut().insert(path);
    }
    pub fn remember(&self, rows: &[Value]) {
        for row in rows {
            let key = (
                crate::util::s(row, "harness").to_owned(),
                crate::util::s(row, "home").to_owned(),
                crate::util::s(row, "id").to_owned(),
            );
            if self.known_keys.borrow_mut().insert(key) {
                let index = self.known_sessions.borrow().len();
                self.known_by_id
                    .borrow_mut()
                    .entry(crate::util::s(row, "id").to_owned())
                    .or_default()
                    .push(index);
                if crate::util::s(row, "harness") == "t3"
                    && !crate::util::s(row, "provider_id").is_empty()
                {
                    self.known_t3_by_provider
                        .borrow_mut()
                        .entry((
                            crate::util::s(row, "provider").to_owned(),
                            crate::util::s(row, "provider_id").to_owned(),
                        ))
                        .or_default()
                        .push(index);
                }
                self.known_sessions.borrow_mut().push(row.clone());
            }
            let source = crate::util::s(row, "path");
            let source = source
                .rsplit_once('#')
                .filter(|(_, s)| s.starts_with("thread=") || s.starts_with("session="))
                .map_or(source, |(p, _)| p);
            if !source.is_empty() {
                self.protect_source(&self.expand(source));
            }
        }
    }
    pub fn diagnostic(
        &self,
        code: &str,
        harness: Option<&str>,
        path: Option<&Path>,
        message: impl Into<String>,
    ) {
        self.diagnostics.borrow_mut().push(Diagnostic {
            code: code.into(),
            severity: "warning".into(),
            harness: harness.map(str::to_owned),
            store_id: path.map(|p| p.to_string_lossy().into_owned()),
            message: message.into(),
            retryable: code == "store_busy",
        });
    }
    pub fn cover(
        &self,
        harness: &str,
        path: Option<&Path>,
        capability: &str,
        status: &str,
        reason: Option<String>,
    ) {
        let item = Coverage {
            harness: harness.into(),
            store_id: path.map(|p| p.to_string_lossy().into_owned()),
            capability: capability.into(),
            status: status.into(),
            reason,
        };
        if !self.coverage.borrow().iter().any(|c| {
            c.harness == item.harness
                && c.store_id == item.store_id
                && c.capability == item.capability
                && c.status == item.status
        }) {
            self.coverage.borrow_mut().push(item);
        }
    }
    pub fn t3_settings(&self) -> Value {
        std::fs::read(self.home.join(".t3").join("userdata").join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null)
    }
    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            self.home.clone()
        } else if path.starts_with("~/") || path.starts_with("~\\") {
            self.home.join(&path[2..])
        } else {
            PathBuf::from(path)
        }
    }
    pub fn tilde(&self, path: &Path) -> String {
        path.strip_prefix(&self.home)
            .map(|p| {
                if p.as_os_str().is_empty() {
                    "~".to_owned()
                } else {
                    format!("~{}{}", std::path::MAIN_SEPARATOR, p.display())
                }
            })
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }
}
