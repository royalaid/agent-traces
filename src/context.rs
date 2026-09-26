use std::{cell::{Cell,RefCell},collections::HashSet,path::{Path,PathBuf}};
use chrono::{DateTime,Utc};
use serde_json::Value;
use crate::model::{Coverage,Diagnostic};

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
}
impl Context {
    pub fn from_env() -> Self {
        let home=std::env::var_os("AGENT_TRACES_HOME").or_else(||std::env::var_os(if cfg!(windows){"USERPROFILE"}else{"HOME"})).map(PathBuf::from).unwrap_or_else(||PathBuf::from("."));
        let cache_dir=std::env::var_os("AGENT_TRACES_CACHE").map(PathBuf::from).unwrap_or_else(||home.join(".cache/agent-traces"));
        let out_dir=std::env::var_os("AGENT_TRACES_OUT").map(PathBuf::from).unwrap_or_else(||std::env::temp_dir().join("agent-traces"));
        let now=std::env::var("AGENT_TRACES_NOW").ok().and_then(|s|crate::util::parse_ts(&Value::String(s))).unwrap_or_else(Utc::now);
        Self{home,cache_dir,out_dir,now,host_id:std::env::var("AGENT_TRACES_HOST_ID").ok().filter(|s|!s.is_empty()),self_ids:["CLAUDE_CODE_SESSION_ID","CODEX_THREAD_ID","CODEX_SESSION_ID"].into_iter().filter_map(|k|std::env::var(k).ok()).filter(|s|!s.is_empty()).collect(),diagnostics:RefCell::new(vec![]),coverage:RefCell::new(vec![]),skipped:Cell::new(0)}
    }
    pub fn diagnostic(&self,code:&str,harness:Option<&str>,path:Option<&Path>,message:impl Into<String>) {
        self.diagnostics.borrow_mut().push(Diagnostic{code:code.into(),severity:"warning".into(),harness:harness.map(str::to_owned),store_id:path.map(|p|p.to_string_lossy().into_owned()),message:message.into(),retryable:code=="store_busy"});
    }
    pub fn cover(&self,harness:&str,path:Option<&Path>,capability:&str,status:&str,reason:Option<String>) {
        let item=Coverage{harness:harness.into(),store_id:path.map(|p|p.to_string_lossy().into_owned()),capability:capability.into(),status:status.into(),reason};
        if !self.coverage.borrow().iter().any(|c| c.harness==item.harness && c.store_id==item.store_id && c.capability==item.capability && c.status==item.status) {self.coverage.borrow_mut().push(item);}
    }
    pub fn t3_settings(&self)->Value { std::fs::read(self.home.join(".t3/userdata/settings.json")).ok().and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or(Value::Null) }
    pub fn expand(&self,path:&str)->PathBuf {
        if path=="~" {self.home.clone()} else if path.starts_with("~/")||path.starts_with("~\\") {self.home.join(&path[2..])} else {PathBuf::from(path)}
    }
    pub fn tilde(&self,path:&Path)->String {
        path.strip_prefix(&self.home).map(|p|format!("~{}{}",std::path::MAIN_SEPARATOR,p.display())).unwrap_or_else(|_|path.to_string_lossy().into_owned())
    }
}
