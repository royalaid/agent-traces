//! Python-reference search ranking, with an in-process streaming candidate scan.
pub use crate::util::event_text;
use crate::{
    context::Context,
    model::{Event, Session},
    util::{label, one_line, parse_ts, s},
};
use anyhow::{Result, bail};
use regex::bytes::{Regex, RegexBuilder};
use rusqlite::params_from_iter;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub terms: Vec<String>,
    pub scope: String,
    pub sort: String,
    pub include_self: bool,
    pub limit: usize,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            terms: vec![],
            scope: "prompts".into(),
            sort: "relevance".into(),
            include_self: false,
            limit: 15,
        }
    }
}
#[derive(Debug)]
pub struct SearchMatch {
    pub row: Session,
    pub hits: Vec<Event>,
    pub score: f64,
    pub match_kind: String,
}
#[derive(Debug)]
pub struct SearchResult {
    pub rows: Vec<SearchMatch>,
    pub total: usize,
}

pub fn roles(scope: &str) -> Result<&'static [&'static str]> {
    Ok(match scope {
        "prompts" => &["prompt", "command"],
        "assistant" => &["assistant"],
        "tools" => &["tool"],
        "results" => &["result"],
        "all" => &["prompt", "command", "assistant", "tool", "result", "system"],
        _ => bail!("unknown search scope: {scope}"),
    })
}
/// Decimal formatting rounds the exact binary float, as Python round(value, 2) does.
pub fn rounded_score(score: f64) -> f64 {
    format!("{score:.2}").parse().unwrap_or(score)
}
pub fn snippet(text: &str, terms: &[String], width: usize) -> String {
    let low = text.to_lowercase();
    let pos = terms
        .iter()
        .filter_map(|t| low.find(&t.to_lowercase()))
        .map(|b| low[..b].chars().count())
        .min()
        .unwrap_or(0);
    let start = pos.saturating_sub(width / 3);
    format!(
        "{}{}",
        if start > 0 { "…" } else { "" },
        one_line(
            &text.chars().skip(start).take(width).collect::<String>(),
            width
        )
    )
}
fn file_matches(ctx: &Context, harness: &str, path: &Path, patterns: &[Regex]) -> bool {
    match file_matches_inner(path, patterns) {
        Ok(found) => found,
        Err(error) => {
            ctx.diagnostic(
                "store_unreadable",
                Some(harness),
                Some(path),
                error.to_string(),
            );
            false
        }
    }
}
fn file_matches_inner(path: &Path, patterns: &[Regex]) -> std::io::Result<bool> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut seen = vec![false; patterns.len()];
    let mut remaining = patterns.len();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(false);
        }
        for (i, p) in patterns.iter().enumerate() {
            if !seen[i] && p.is_match(&line) {
                seen[i] = true;
                remaining -= 1;
            }
        }
        if remaining == 0 {
            return Ok(true);
        }
    }
}
// Opening read-only participates in the existing WAL. Never issue journal_mode=WAL,
// which would mutate a live database. SQLite read transactions remain short-lived.
fn sql_candidates(
    ctx: &Context,
    harness: &str,
    terms: &[String],
    roles: &[&str],
) -> Result<HashSet<String>> {
    let path = if harness == "t3" {
        ctx.home.join(".t3").join("userdata").join("state.sqlite")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.home.join(".local").join("share"))
            .join("opencode")
            .join("opencode.db")
    };
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let con = crate::storage::sqlite::open(ctx, harness, &path)?;
    let query = |sql: &str, args: Vec<String>| -> Result<HashSet<String>> {
        Ok(con
            .prepare(sql)?
            .query_map(params_from_iter(args), |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let likes: Vec<String> = terms.iter().map(|t| format!("%{t}%")).collect();
    let mut ids = HashSet::new();
    if harness == "t3" {
        let mut msg_roles = Vec::new();
        if roles.iter().any(|r| matches!(*r, "prompt" | "command")) {
            msg_roles.push("user".to_owned());
        }
        if roles.contains(&"assistant") {
            msg_roles.push("assistant".to_owned());
        }
        if !msg_roles.is_empty() {
            let sql = format!(
                "select distinct thread_id from projection_thread_messages where role in ({}) and {}",
                vec!["?"; msg_roles.len()].join(","),
                vec!["text like ?"; terms.len()].join(" and ")
            );
            msg_roles.extend(likes.clone());
            ids.extend(query(&sql, msg_roles)?);
        }
        if roles.iter().any(|r| matches!(*r, "tool" | "result")) {
            ids.extend(query(&format!("select distinct thread_id from projection_thread_activities where kind='tool.completed' and {}",vec!["payload_json like ?";terms.len()].join(" and ")),likes)?);
        }
    } else {
        let types = if roles.iter().all(|r| matches!(*r, "prompt" | "command")) {
            vec!["user"]
        } else {
            vec!["user", "assistant", "synthetic", "system"]
        };
        let sql = format!(
            "select distinct session_id from session_message where type in ({}) and data like ?",
            vec!["?"; types.len()].join(",")
        );
        for (i, like) in likes.into_iter().enumerate() {
            let mut args: Vec<String> = types.iter().map(|t| t.to_string()).collect();
            args.push(like);
            let got = query(&sql, args)?;
            if i == 0 {
                ids = got;
            } else {
                ids.retain(|id| got.contains(id));
            }
        }
    }
    Ok(ids)
}
struct Doc {
    event: Event,
    tf: Vec<usize>,
    length: usize,
    first: bool,
}
fn bm25(tf: &[usize], length: usize, idf: &[f64], avgdl: f64) -> f64 {
    tf.iter()
        .zip(idf)
        .filter(|(f, _)| **f > 0)
        .map(|(f, idf)| {
            idf * (*f as f64) * 2.2 / (*f as f64 + 1.2 * (0.25 + 0.75 * length as f64 / avgdl))
        })
        .sum()
}
/// Select sessions whose raw sources contain every term without parsing their events.
/// Keeps harness encounter order and the reference SQL role predicates.
pub fn prefilter_candidates(
    ctx: &Context,
    rows: Vec<Session>,
    terms: &[String],
    scope: &[&str],
) -> Result<Vec<Session>> {
    if terms.is_empty() {
        return Ok(rows);
    }
    let patterns = terms
        .iter()
        .map(|t| {
            RegexBuilder::new(&regex::escape(t))
                .case_insensitive(true)
                .build()
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // Keep harness encounter order and stable ordering within every harness.
    let mut groups: Vec<(String, Vec<Session>)> = Vec::new();
    for row in rows {
        let h = s(&row, "harness").to_owned();
        if let Some((_, rs)) = groups.iter_mut().find(|(key, _)| *key == h) {
            rs.push(row);
        } else {
            groups.push((h, vec![row]));
        }
    }
    let mut candidates = Vec::new();
    for (h, rs) in groups {
        if matches!(h.as_str(), "claude" | "codex" | "grok") {
            let mut memo = HashMap::new();
            for row in rs {
                let p = s(&row, "path");
                let keep = *memo
                    .entry(p.to_owned())
                    .or_insert_with(|| file_matches(ctx, &h, &ctx.expand(p), &patterns));
                if keep {
                    candidates.push(row);
                }
            }
        } else if matches!(h.as_str(), "t3" | "opencode") {
            match sql_candidates(ctx, &h, terms, scope) {
                Ok(ids) => candidates.extend(rs.into_iter().filter(|r| ids.contains(s(r, "id")))),
                Err(e) => ctx.diagnostic("store_unreadable", Some(&h), None, e.to_string()),
            }
        }
    }
    Ok(candidates)
}
pub fn search(ctx: &Context, rows: Vec<Session>, options: &SearchOptions) -> Result<SearchResult> {
    let terms: Vec<String> = options
        .terms
        .iter()
        .filter(|t| !t.is_empty())
        .cloned()
        .collect();
    if terms.is_empty() {
        bail!("give at least one search term");
    }
    let scope = roles(&options.scope)?;
    let rows = rows
        .into_iter()
        .filter(|row| {
            options.include_self
                || !["id", "parent", "provider_id"]
                    .iter()
                    .any(|key| ctx.self_ids.contains(s(row, key)))
        })
        .collect();
    let candidates = prefilter_candidates(ctx, rows, &terms, scope)?;
    let lows: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut df = vec![0usize; lows.len()];
    let mut n_docs = 0usize;
    let mut total_len = 0usize;
    let mut sessions = Vec::new();
    ctx.skipped.set(0);
    for row in candidates {
        let ad = crate::core::adapter(s(&row, "harness"))?;
        let first = one_line(s(&row, "first_prompt"), 80).to_lowercase();
        let events = match ad.events(ctx, &row, Some(&lows)) {
            Ok(e) => e,
            Err(e) => {
                ctx.diagnostic(
                    "store_unreadable",
                    Some(s(&row, "harness")),
                    None,
                    e.to_string(),
                );
                continue;
            }
        };
        let mut docs = Vec::new();
        let mut covered = HashSet::new();
        for event in events {
            if !scope.contains(&event.role.as_str()) {
                continue;
            }
            let low = event_text(&event).to_lowercase();
            let length = low.matches(' ').count() + 1;
            let is_first = !first.is_empty()
                && matches!(event.role.as_str(), "prompt" | "command")
                && one_line(&low, 80) == first;
            n_docs += 1;
            total_len += length;
            let tf: Vec<usize> = lows
                .iter()
                .map(|t| low.matches(t.as_str()).count())
                .collect();
            if !tf.iter().any(|n| *n > 0) {
                continue;
            }
            // Python increments a dict entry once for each duplicate query term.
            for (i, t) in lows.iter().enumerate() {
                if tf[i] > 0 {
                    covered.insert(t.clone());
                    for (j, u) in lows.iter().enumerate() {
                        if u == t {
                            df[j] += 1;
                        }
                    }
                }
            }
            docs.push(Doc {
                event,
                tf,
                length,
                first: is_first,
            });
        }
        if !docs.is_empty() && covered.len() == lows.len() {
            sessions.push((row, docs));
        }
    }
    let avgdl = if n_docs > 0 {
        total_len as f64 / n_docs as f64
    } else {
        1.0
    };
    n_docs += ctx.skipped.get();
    let idf: Vec<f64> = df
        .iter()
        .map(|df| (1.0 + (n_docs as f64 - *df as f64 + 0.5) / (*df as f64 + 0.5)).ln())
        .collect();
    let mut matches = Vec::new();
    for (mut row, docs) in sessions {
        let mut scored: Vec<_> = docs
            .into_iter()
            .map(|d| {
                let weight = match d.event.role.as_str() {
                    "prompt" | "command" => 1.5,
                    "tool" => 0.8,
                    "result" | "system" => 0.5,
                    _ => 1.0,
                };
                (
                    bm25(&d.tf, d.length, &idf, avgdl) * weight * if d.first { 1.2 } else { 1.0 },
                    d,
                )
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let title = format!("{} {}", label(&row), s(&row, "t3_title")).to_lowercase();
        let ttf: Vec<usize> = lows
            .iter()
            .map(|t| title.matches(t.as_str()).count())
            .collect();
        let title_score = 2.0 * bm25(&ttf, title.split_whitespace().count().max(1), &idf, avgdl);
        let score = scored[0].0 + 0.5 * (1.0 + scored.len() as f64).ln() + title_score;
        let match_kind = if ttf.iter().all(|n| *n > 0) {
            "title"
        } else if scored
            .iter()
            .any(|(_, d)| d.first && d.tf.iter().all(|n| *n > 0))
        {
            "first prompt"
        } else {
            ""
        }
        .to_owned();
        let full = scored.iter().any(|(_, d)| d.tf.iter().all(|n| *n > 0));
        let hits = scored
            .into_iter()
            .filter(|(_, d)| !full || d.tf.iter().all(|n| *n > 0))
            .map(|(_, d)| d.event)
            .collect();
        row["_score"] = serde_json::json!(rounded_score(score));
        row["_match"] = serde_json::json!(match_kind);
        matches.push(SearchMatch {
            row,
            hits,
            score,
            match_kind,
        });
    }
    sort_matches(&mut matches, &options.sort);
    let total = matches.len();
    matches.truncate(options.limit);
    Ok(SearchResult {
        rows: matches,
        total,
    })
}
fn sort_matches(matches: &mut [SearchMatch], sort: &str) {
    match sort {
        "oldest" => matches.sort_by_key(|m| m.hits.iter().map(|e| e.ts).min().flatten()),
        "recent" => matches.sort_by(|a, b| {
            let rank = |m: &SearchMatch| match m.match_kind.as_str() {
                "title" => 2,
                "first prompt" => 1,
                _ => 0,
            };
            rank(b)
                .cmp(&rank(a))
                .then_with(|| parse_ts(&b.row["updated"]).cmp(&parse_ts(&a.row["updated"])))
        }),
        _ => matches.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| parse_ts(&b.row["updated"]).cmp(&parse_ts(&a.row["updated"])))
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_prefilter_groups_stably_and_memoizes_source_errors() {
        let ctx = Context::from_env();
        let temp = tempfile::tempdir().unwrap();
        let hit = temp.path().join("hit.jsonl");
        std::fs::write(&hit, b"TARGET.rs\n").unwrap();
        let miss = temp.path().join("miss.jsonl");
        std::fs::write(&miss, b"other.rs\n").unwrap();
        let missing = temp.path().join("missing.jsonl");
        let row = |h: &str, id: &str, p: &Path| serde_json::json!({"harness":h,"id":id,"path":p.to_str().unwrap()});
        let rows = vec![
            row("codex", "one", &hit),
            row("claude", "two", &hit),
            row("codex", "three", &hit),
            row("grok", "miss", &miss),
            row("grok", "absent-a", &missing),
            row("grok", "absent-b", &missing),
        ];
        let selected =
            prefilter_candidates(&ctx, rows, &["target.rs".into()], roles("all").unwrap()).unwrap();
        assert_eq!(
            selected.iter().map(|r| s(r, "id")).collect::<Vec<_>>(),
            vec!["one", "three", "two"]
        );
        assert_eq!(ctx.diagnostics.borrow().len(), 1);
    }
    #[test]
    fn unreadable_source_reports_partial_coverage_but_no_match_does_not() {
        let ctx = Context::from_env();
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("missing.jsonl");
        let patterns = vec![Regex::new("needle").unwrap()];
        assert!(!file_matches(&ctx, "claude", &absent, &patterns));
        assert_eq!(ctx.diagnostics.borrow().len(), 1);
        assert_eq!(ctx.diagnostics.borrow()[0].code, "store_unreadable");
        assert_eq!(
            ctx.diagnostics.borrow()[0].store_id.as_deref(),
            absent.to_str()
        );
        let empty = temp.path().join("empty.jsonl");
        std::fs::write(&empty, b"unrelated text\n").unwrap();
        assert!(!file_matches(&ctx, "claude", &empty, &patterns));
        assert_eq!(ctx.diagnostics.borrow().len(), 1);
        let defaults = SearchOptions::default();
        assert_eq!(
            (
                defaults.scope.as_str(),
                defaults.sort.as_str(),
                defaults.limit
            ),
            ("prompts", "relevance", 15)
        );
    }
    #[test]
    fn ordering_uses_full_scores_and_match_priority() {
        let make = |id: &str, score: f64, kind: &str, date: &str| SearchMatch {
            row: serde_json::json!({"id":id,"updated":date}),
            hits: vec![Event::new(
                "prompt",
                "hit",
                parse_ts(&serde_json::json!(date)),
            )],
            score,
            match_kind: kind.into(),
        };
        let mut rows = vec![
            make("a", 1.001, "title", "2020-01-01"),
            make("b", 1.002, "", "2022-01-01"),
            make("c", 1.001, "first prompt", "2021-01-01"),
        ];
        sort_matches(&mut rows, "score");
        assert_eq!(
            rows.iter().map(|m| s(&m.row, "id")).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        sort_matches(&mut rows, "recent");
        assert_eq!(
            rows.iter().map(|m| s(&m.row, "id")).collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
        sort_matches(&mut rows, "oldest");
        assert_eq!(
            rows.iter().map(|m| s(&m.row, "id")).collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
    }
    #[test]
    fn exact_binary_decimal_rounding() {
        assert_eq!(rounded_score(2.675), 2.67);
        assert_eq!(rounded_score(2.685), 2.69);
        assert_eq!(rounded_score(1.125), 1.12);
    }
    #[test]
    fn bm25_hand_calculation() {
        let idf = (1.0_f64 + 8.5 / 2.5).ln();
        assert!((bm25(&[1], 10, &[idf], 10.0) - idf).abs() < 1e-12);
        assert!(bm25(&[2], 10, &[idf], 10.0) > idf);
        assert!(bm25(&[1], 20, &[idf], 10.0) < idf);
    }
    #[test]
    fn snippet_counts_unicode_characters() {
        assert_eq!(snippet("ééé café later", &["café".into()], 6), "…é café");
    }
    #[test]
    fn prefilter_all_terms_across_lines_and_unicode() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(f, "first ALPHA\nsecond KELVIN").unwrap();
        let p = vec![
            RegexBuilder::new("alpha")
                .case_insensitive(true)
                .build()
                .unwrap(),
            RegexBuilder::new("kelvin")
                .case_insensitive(true)
                .build()
                .unwrap(),
        ];
        assert!(file_matches_inner(f.path(), &p).unwrap());
        let p = vec![Regex::new("absent").unwrap()];
        assert!(!file_matches_inner(f.path(), &p).unwrap());
    }
}

#[cfg(test)]
mod sql_tests {
    use super::*;
    #[test]
    fn t3_prefilter_preserves_same_message_and_role_constraints() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp
            .path()
            .join(".t3")
            .join("userdata")
            .join("state.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE projection_thread_messages(thread_id TEXT,role TEXT,text TEXT); CREATE TABLE projection_thread_activities(thread_id TEXT,kind TEXT,payload_json TEXT); INSERT INTO projection_thread_messages VALUES ('same','user','alpha beta'),('split','user','alpha'),('split','user','beta'),('reply','assistant','alpha beta'); INSERT INTO projection_thread_activities VALUES ('tool','tool.completed','alpha beta');").unwrap();
        let mut ctx = Context::from_env();
        ctx.home = temp.path().to_owned();
        let terms = vec!["alpha".into(), "beta".into()];
        assert_eq!(
            sql_candidates(&ctx, "t3", &terms, &["prompt"]).unwrap(),
            HashSet::from(["same".into()])
        );
        assert_eq!(
            sql_candidates(&ctx, "t3", &terms, &["assistant"]).unwrap(),
            HashSet::from(["reply".into()])
        );
        assert_eq!(
            sql_candidates(&ctx, "t3", &terms, &["tool"]).unwrap(),
            HashSet::from(["tool".into()])
        );
        // The reader must coexist with an uncommitted WAL writer and see only committed data.
        writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO projection_thread_messages VALUES ('uncommitted','user','alpha beta');").unwrap();
        assert_eq!(
            sql_candidates(&ctx, "t3", &terms, &["prompt"]).unwrap(),
            HashSet::from(["same".into()])
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }
}
