use crate::{
    context::Context,
    model::{Event, Session},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
pub trait StoreAdapter {
    fn name(&self) -> &'static str;
    fn homes(&self, ctx: &Context) -> Vec<std::path::PathBuf>;
    fn sessions(
        &self,
        ctx: &Context,
        since: Option<DateTime<Utc>>,
        subagents: bool,
    ) -> Result<Vec<Session>>;
    fn resolve(&self, ctx: &Context, ident: &str) -> Result<Vec<Session>>;
    fn events<'a>(
        &self,
        ctx: &'a Context,
        row: &'a Session,
        needles: Option<&[String]>,
    ) -> Result<Box<dyn Iterator<Item = Event> + 'a>>;
    fn children(&self, ctx: &Context, row: &Session) -> Result<Vec<Session>> {
        let id = crate::util::s(row, "id");
        Ok(self
            .sessions(ctx, None, true)?
            .into_iter()
            .filter(|r| crate::util::s(r, "parent") == id)
            .collect())
    }
    fn where_info(&self, ctx: &Context) -> Result<Vec<Value>>;
}
// The parent adds module declarations and registry after parallel adapters land.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;
pub mod t3;
