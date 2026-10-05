use crate::{
    context::Context,
    model::{Event, Session},
    render::*,
    util::*,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    borrow::Borrow,
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};
#[derive(Default, Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub artifacts: Vec<(PathBuf, String)>,
}
#[derive(Default, Debug)]
pub struct ShowOptions {
    pub turns: Option<String>,
    pub tail: i64,
    pub grep: Vec<String>,
    pub notices: bool,
    pub tools: bool,
    pub full: bool,
    pub results: bool,
    pub full_stdout: bool,
    pub output: Option<PathBuf>,
}
fn artifact(path: PathBuf, text: String, quiet: bool) -> CommandOutput {
    CommandOutput {
        stdout: if quiet {
            String::new()
        } else {
            format!(
                "wrote {} ({} chars)\n",
                path.display(),
                text.chars().count()
            )
        },
        artifacts: vec![(path, text)],
        ..Default::default()
    }
}
pub fn show(
    ctx: &Context,
    row: &Session,
    events: &[Event],
    opts: &ShowOptions,
) -> Result<CommandOutput> {
    let events = events
        .iter()
        .filter(|e| opts.notices || e.role != "notice")
        .cloned()
        .collect::<Vec<_>>();
    let turns = turns_of(&events);
    let n = turns.len();
    let mut sel = parse_range(opts.turns.as_deref(), n)?;
    if opts.tail != 0 {
        sel = Some(if opts.tail < 0 {
            BTreeSet::new()
        } else {
            (n.saturating_sub(usize::try_from(opts.tail).unwrap_or(usize::MAX)) + 1..=n).collect()
        });
    }
    if !opts.grep.is_empty() {
        let gs = opts
            .grep
            .iter()
            .map(|s| s.to_lowercase())
            .collect::<Vec<_>>();
        let allowed = sel.as_ref().filter(|s| !s.is_empty());
        sel = Some(
            turns
                .iter()
                .enumerate()
                .filter(|(i, t)| {
                    allowed.is_none_or(|a| a.contains(&(i + 1)))
                        && gs
                            .iter()
                            .all(|g| t.iter().any(|e| event_text(e).to_lowercase().contains(g)))
                })
                .map(|(i, _)| i + 1)
                .collect(),
        );
    }
    let mut lines = vec![
        header(ctx, row),
        format!(
            "- turns: {n}{}",
            sel.as_ref()
                .map(|s| format!(" (showing {})", compress(s)))
                .unwrap_or_default()
        ),
    ];
    for (i, turn) in turns.iter().enumerate() {
        if sel.as_ref().is_some_and(|s| !s.contains(&(i + 1))) {
            continue;
        }
        lines.push(format!("\n---- turn {}/{n} ----", i + 1));
        for e in turn {
            if e.role == "tool" && !opts.tools {
                continue;
            }
            let line = render_event(e, opts.full, opts.results);
            if !line.is_empty() {
                lines.push(line);
            }
        }
    }
    let text = lines.join("\n") + "\n";
    if let Some(p) = &opts.output {
        return Ok(artifact(ctx.expand(&p.to_string_lossy()), text, false));
    }
    if text.chars().count() <= 20000 || opts.full_stdout {
        return Ok(CommandOutput {
            stdout: text,
            ..Default::default()
        });
    }
    let path = ctx
        .out_dir
        .join(format!("{}-{}.md", s(row, "harness"), s(row, "id")));
    let len = text.chars().count();
    let mut result = artifact(path.clone(), text, true);
    let mut index = vec![
        header(ctx, row),
        format!("- turns: {n}"),
        format!(
            "\nOutput is {len} chars, over the 20000 budget. Full render: {}",
            path.display()
        ),
        "Read it in slices, or narrow with --turns 3-5, --tail 2, or --grep TEXT.\n".into(),
        "Turn index:".into(),
    ];
    for (i, turn) in turns.iter().enumerate() {
        let first = &turn[0];
        let kind = if ["prompt", "command"].contains(&first.role.as_str()) {
            "user"
        } else {
            &first.role
        };
        let tools = turn.iter().filter(|e| e.role == "tool").count();
        let errs = turn.iter().filter(|e| e.error).count();
        index.push(format!(
            "  {:>3} {} {:<9} {}{}",
            i + 1,
            &fmt_ts(first.ts)[5..],
            kind,
            redact(&one_line(&event_text(first), 110)),
            if tools > 0 {
                format!(
                    "  ({tools} tools{})",
                    if errs > 0 {
                        format!(", {errs} errors")
                    } else {
                        String::new()
                    }
                )
            } else {
                String::new()
            }
        ));
    }
    if let Some(e) = turns
        .last()
        .and_then(|t| t.iter().rev().find(|e| e.role == "assistant"))
    {
        index.push(format!(
            "\nLast assistant message:\n{}",
            redact(&clip(&e.text, 3000))
        ));
    }
    result.stdout = index.join("\n") + "\n";
    Ok(result)
}
pub fn tree(ctx: &Context, row: &Session, kids: &[Session]) -> CommandOutput {
    let mut out = fmt_row(ctx, row) + "\n";
    for k in kids {
        out.push_str(&format!("  {}\n", fmt_row(ctx, k).replace('\n', "\n  ")));
    }
    CommandOutput {
        stdout: out,
        stderr: if kids.is_empty() {
            "no child sessions recorded\n".into()
        } else {
            String::new()
        },
        ..Default::default()
    }
}
fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some_and(|n| n != "..") {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            _ => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".")
    }
    out
}
struct Handoff<'a> {
    events: Vec<Event>,
    prompts: Vec<&'a Event>,
    assistant: Vec<&'a Event>,
    tools: Vec<&'a Event>,
    errors: Vec<&'a Event>,
    commands: Vec<&'a Event>,
    files: Vec<String>,
    plan: Option<&'a Event>,
    tickets: Vec<String>,
    urls: Vec<String>,
}
fn extract_handoff<'a>(ctx: &Context, row: &Session, events: &'a [Event]) -> Handoff<'a> {
    let mut h = Handoff {
        events: vec![],
        prompts: vec![],
        assistant: vec![],
        tools: vec![],
        errors: vec![],
        commands: vec![],
        files: vec![],
        plan: None,
        tickets: vec![],
        urls: vec![],
    };
    for e in events.iter().filter(|e| e.role != "notice") {
        h.events.push(e.clone());
        match e.role.as_str() {
            "prompt" | "command" => h.prompts.push(e),
            "assistant" => h.assistant.push(e),
            "tool" => h.tools.push(e),
            _ => {}
        }
        if e.error {
            h.errors.push(e)
        }
    }
    for e in &h.tools {
        let name = e.name.as_deref().unwrap_or("");
        for p in touched_paths(name, e.input.as_ref().unwrap_or(&Value::Null)) {
            let p = if s(row, "cwd").is_empty() {
                p
            } else {
                normalized(&Path::new(s(row, "cwd")).join(ctx.expand(&p)))
                    .to_string_lossy()
                    .into_owned()
            };
            if !h.files.contains(&p) {
                h.files.push(p)
            }
        }
        if [
            "bash",
            "shell",
            "exec_command",
            "local_shell",
            "exec",
            "execute",
            "shell_command",
            "run_terminal_cmd",
            "run_command",
        ]
        .contains(&name.to_lowercase().as_str())
        {
            h.commands.push(e)
        }
        if ["TodoWrite", "todowrite", "update_plan"].contains(&name) {
            h.plan = Some(e)
        }
    }
    static T: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Z]{2,6}-\d{1,5}\b").unwrap());
    static U: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"https://(?:gitlab\.com|github\.com)/[^\s)"'>\]]+/(?:-/)?(?:merge_requests|pull)/\d+"#).unwrap()
    });
    let text = h
        .events
        .iter()
        .map(event_text)
        .collect::<Vec<_>>()
        .join("\n");
    h.tickets = T
        .find_iter(&text)
        .map(|m| m.as_str().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    h.tickets.sort_by_key(|t| {
        let (p, n) = t.split_once('-').unwrap();
        (p.to_owned(), n.parse::<usize>().unwrap_or(0))
    });
    h.urls = U
        .find_iter(&text)
        .map(|m| m.as_str().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    h
}
fn pretty_plan(input: &Value) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(input, &mut serializer).expect("serialize Value");
    String::from_utf8(out).expect("UTF-8 JSON")
}
/// A brief section's count: "N", or "last SHOWN of N" when only the tail is shown.
fn counted(total: usize, shown: usize) -> String {
    if total <= shown {
        total.to_string()
    } else {
        format!("last {shown} of {total}")
    }
}
fn tail<T>(items: &[T], n: usize) -> &[T] {
    &items[items.len().saturating_sub(n)..]
}
pub fn handoff(
    ctx: &Context,
    row: &Session,
    events: &[Event],
    kids: &[Session],
    output: Option<&Path>,
) -> CommandOutput {
    let h = extract_handoff(ctx, row, events);
    let mut lines=vec![header(ctx,row).replacen("# ","# Handoff draft: ",1),String::new(),"> Draft extracted from a transcript by agent-traces. It records what the session said and did,".into(),"> not what is true now. Verify branch, files, and ticket state before acting on it.".into(),String::new(),"## Goal (first request)".into(),h.prompts.first().map(|e|redact(&clip(&e.text,3000))).unwrap_or_else(||"(no user prompt found)".into()),String::new(),format!("## All user requests ({})",h.prompts.len())];
    for (i, p) in h.prompts.iter().enumerate() {
        lines.push(format!(
            "{}. [{}] {}",
            i + 1,
            fmt_ts(p.ts),
            redact(&if i + 1 > h.prompts.len().saturating_sub(3) {
                clip(&p.text, 1500)
            } else {
                one_line(&p.text, 300)
            })
        ));
    }
    lines.extend([String::new(),"## Where it ended (last assistant message; from the transcript, unverified)".into(),h.assistant.last().map(|e|redact(&clip(&e.text,6000))).unwrap_or_else(||"(no assistant text)".into()),String::new(),"## Current state checks (fill in before handing off)".into(),"For each claim above about branches, commits, files, tickets, MRs, or tests, run a live check and record".into(),"`verified: <command> -> <result>`, or leave the claim under 'from the transcript, unverified'.".into(),String::new()]);
    if h.assistant.len() > 1 {
        lines.push("## Earlier assistant conclusions (last 4, truncated)".into());
        for e in tail(&h.assistant, 5)
            .iter()
            .take(tail(&h.assistant, 5).len() - 1)
        {
            lines.push(format!(
                "- [{}] {}",
                fmt_ts(e.ts),
                redact(&one_line(&e.text, 400))
            ));
        }
        lines.push(String::new());
    }
    if let Some(input) = h.plan.and_then(|e| e.input.as_ref()).filter(|v| {
        !v.is_null()
            && v.as_array().is_none_or(|a| !a.is_empty())
            && v.as_object().is_none_or(|o| !o.is_empty())
    }) {
        lines.extend([
            "## Last todo / plan state".into(),
            "```".into(),
            redact(&clip(&pretty_plan(input), 3000)),
            "```".into(),
            String::new(),
        ]);
    }
    lines.push(format!("## Files written or edited ({})", h.files.len()));
    if h.files.is_empty() {
        lines.push("(none recorded)".into())
    } else {
        for f in h.files.iter().take(80) {
            lines.push(format!("- {}", ctx.tilde(Path::new(f))))
        }
    }
    lines.extend([
        String::new(),
        format!("## Commands run ({})", counted(h.commands.len(), 25)),
    ]);
    if h.commands.is_empty() {
        lines.push("(none)".into())
    } else {
        for e in tail(&h.commands, 25) {
            lines.push(format!(
                "- {}",
                redact(&tool_summary(
                    e.name.as_deref().unwrap_or(""),
                    e.input.as_ref().unwrap_or(&Value::Null)
                ))
            ));
        }
    }
    lines.extend([
        String::new(),
        format!(
            "## Errors and interruptions ({})",
            counted(h.errors.len(), 15)
        ),
    ]);
    if h.errors.is_empty() {
        lines.push("(none)".into())
    } else {
        for e in tail(&h.errors, 15) {
            lines.push(format!(
                "- [{}] {}",
                fmt_ts(e.ts),
                redact(&one_line(&e.text, 240))
            ));
        }
    }
    lines.extend([
        String::new(),
        "## Tickets and merge requests mentioned".into(),
        if h.tickets.is_empty() {
            "(none)".into()
        } else {
            h.tickets
                .iter()
                .take(60)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        },
    ]);
    for u in h.urls.iter().take(20) {
        lines.push(format!("- {u}"));
    }
    if !kids.is_empty() {
        lines.extend([String::new(), format!("## Child sessions ({})", kids.len())]);
        for k in kids.iter().take(30) {
            lines.push(format!(
                "- {} {}: {}",
                s(k, "harness"),
                s(k, "id"),
                one_line(&label(k), 120)
            ));
        }
    }
    lines.extend([
        String::new(),
        "## Size".into(),
        format!(
            "{} turns, {} tool calls, {} errors",
            turns_of(&h.events).len(),
            h.tools.len(),
            h.errors.len()
        ),
    ]);
    let text = lines.join("\n") + "\n";
    let path = output
        .map(|p| ctx.expand(&p.to_string_lossy()))
        .unwrap_or_else(|| {
            ctx.out_dir.join(format!(
                "handoff-{}-{}.md",
                s(row, "harness"),
                s(row, "id").chars().take(12).collect::<String>()
            ))
        });
    artifact(path, text, false)
}
fn extract(e: &Event, n: usize, line: bool) -> Value {
    let text = if line {
        one_line(&e.text, n)
    } else {
        clip(&e.text, n)
    };
    let before = if line {
        e.text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        e.text.trim().to_owned()
    };
    json!({"ts":e.ts.map(|t|t.to_rfc3339_opts(chrono::SecondsFormat::Micros,true)),"text":redact(&text),"truncated":before.chars().count()>n})
}
pub fn handoff_data(ctx: &Context, row: &Session, events: &[Event], kids: &[Session]) -> Value {
    let h = extract_handoff(ctx, row, events);
    let earlier = if h.assistant.is_empty() {
        &[][..]
    } else {
        &h.assistant[..h.assistant.len() - 1]
    };
    let requests = h
        .prompts
        .iter()
        .enumerate()
        .map(|(i, e)| {
            extract(
                e,
                if i + 1 > h.prompts.len().saturating_sub(3) {
                    1500
                } else {
                    300
                },
                i < h.prompts.len().saturating_sub(3),
            )
        })
        .collect::<Vec<_>>();
    let plan = h.plan.and_then(|e| {
        e.input.as_ref().map(|input| {
            let mut event = e.clone();
            event.text = pretty_plan(input);
            extract(&event, 3000, false)
        })
    });
    json!({"session":row,"verification":"unverified","goal":h.prompts.first().map(|e|extract(e,3000,false)),"requests":requests,"last_assistant":h.assistant.last().map(|e|extract(e,6000,false)),"earlier_conclusions":tail(earlier,4).iter().map(|e|extract(e,400,true)).collect::<Vec<_>>(),"last_plan":plan,"files":h.files.iter().take(80).collect::<Vec<_>>(),"commands":tail(&h.commands,25).iter().map(|e|{let mut event=(*e).clone();event.text=tool_summary_unclipped(e.name.as_deref().unwrap_or(""),e.input.as_ref().unwrap_or(&Value::Null));extract(&event,160,true)}).collect::<Vec<_>>(),"errors":tail(&h.errors,15).iter().map(|e|extract(e,240,true)).collect::<Vec<_>>(),"tickets":h.tickets.iter().take(60).collect::<Vec<_>>(),"pull_requests":h.urls.iter().take(20).collect::<Vec<_>>(),"children":kids.iter().take(30).collect::<Vec<_>>(),"counts":{"turns":turns_of(&h.events).len(),"requests":h.prompts.len(),"tool_calls":h.tools.len(),"commands":h.commands.len(),"errors":h.errors.len(),"files":h.files.len(),"tickets":h.tickets.len(),"pull_requests":h.urls.len(),"children":kids.len()},"truncation":{"requests":false,"earlier_conclusions":earlier.len()>4,"files":h.files.len()>80,"commands":h.commands.len()>25,"errors":h.errors.len()>15,"tickets":h.tickets.len()>60,"pull_requests":h.urls.len()>20,"children":kids.len()>30}})
}
fn row_json(row: &Session) -> Value {
    let mut d = row.clone();
    for k in ["started", "updated"] {
        d[k] = d
            .get(k)
            .and_then(parse_ts)
            .map(|t| json!(iso(t)))
            .unwrap_or(Value::Null);
    }
    d
}
/// The span of events `failures` scores. Session selection (`--since` against
/// last activity) is separate: a session active this week can hold errors
/// from weeks ago, and those must not count toward this week's score.
#[derive(Clone, Copy, Default)]
pub struct EventWindow {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}
impl EventWindow {
    /// An undated event cannot be placed, so it stays in.
    fn contains(&self, ts: Option<DateTime<Utc>>) -> bool {
        ts.is_none_or(|t| self.since.is_none_or(|s| t >= s) && self.until.is_none_or(|u| t <= u))
    }
    fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }
}

pub fn failures<I, T>(
    ctx: &Context,
    rows_events: I,
    min: i64,
    limit: i64,
    json_output: bool,
    since: Option<DateTime<Utc>>,
    window: EventWindow,
) -> CommandOutput
where
    I: IntoIterator<Item = T>,
    T: Borrow<(Session, Vec<Event>)>,
{
    static CORRECTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:no\b|nope\b|stop\b|wait\b|hold on|that'?s (?:not|wrong)|you (?:didn'?t|did not|missed|forgot|never)|wrong\b|why did you|don'?t\b|undo\b|revert\b|this is (?:wrong|broken)|it (?:still )?(?:doesn'?t|does not) work)").unwrap()
    });
    let mut scored = vec![];
    let mut total = 0;
    for item in rows_events {
        total += 1;
        let (row, events) = item.borrow();
        let events = events
            .iter()
            .filter(|e| e.role != "notice")
            .collect::<Vec<_>>();
        let in_window = events
            .iter()
            .filter(|e| window.contains(e.ts))
            .copied()
            .collect::<Vec<_>>();
        let te = in_window
            .iter()
            .filter(|e| e.role == "result" && e.error)
            .copied()
            .collect::<Vec<_>>();
        let se = in_window
            .iter()
            .filter(|e| e.role == "system" && e.error)
            .copied()
            .collect::<Vec<_>>();
        let cp = in_window
            .iter()
            .filter(|e| e.role == "system" && e.text.contains("compact"))
            .count();
        let prompts = events
            .iter()
            .filter(|e| ["prompt", "command"].contains(&e.role.as_str()))
            .copied()
            .collect::<Vec<_>>();
        // The session's first prompt is never a correction, even when it
        // falls outside the window, so skip it before windowing.
        let co = prompts
            .iter()
            .skip(1)
            .filter(|e| window.contains(e.ts) && CORRECTION.is_match(&e.text))
            .copied()
            .collect::<Vec<_>>();
        let prompts = prompts
            .into_iter()
            .filter(|e| window.contains(e.ts))
            .collect::<Vec<_>>();
        let score = te.len() + 3 * se.len() + 2 * co.len() + cp;
        if (score as i128) < i128::from(min) {
            continue;
        }
        let mut bad = se
            .iter()
            .map(|e| (*e, "system"))
            .chain(te.iter().map(|e| (*e, "tool error")))
            .chain(co.iter().map(|e| (*e, "correction")))
            .collect::<Vec<_>>();
        bad.sort_by_key(|(e, _)| e.ts);
        // Preserve stable timestamp/role ordering, retaining only printable samples.
        let bad = bad
            .into_iter()
            .take(3)
            .map(|(e, kind)| {
                (
                    e.ts,
                    kind,
                    redact(&one_line(&e.text, 240)),
                    redact(&one_line(&e.text, 200)),
                )
            })
            .collect::<Vec<_>>();
        scored.push((
            score,
            row.clone(),
            te.len(),
            se.len(),
            co.len(),
            cp,
            prompts.len(),
            bad,
        ));
    }
    scored.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut stdout = String::new();
    let shown = if limit < 0 {
        scored
            .len()
            .saturating_sub(usize::try_from(limit.unsigned_abs()).unwrap_or(usize::MAX))
    } else {
        usize::try_from(limit).unwrap_or(usize::MAX)
    };
    for (score, row, te, se, co, cp, np, bad) in scored.iter().take(shown) {
        if json_output {
            let mut d = row_json(row);
            for (k, n) in [
                ("score", score),
                ("tool_errors", te),
                ("system_errors", se),
                ("corrections", co),
                ("compactions", cp),
                ("prompts", np),
            ] {
                d[k] = json!(n);
            }
            d["samples"] = json!(
                bad.iter()
                    .take(3)
                    .map(|(_, _, sample, _)| sample)
                    .collect::<Vec<_>>()
            );
            stdout.push_str(&python_json(&d));
            stdout.push('\n');
            continue;
        }
        stdout.push_str(&format!("{}\n      score {score}: {te} tool errors, {se} api/abort/runtime errors, {co} user corrections, {cp} compactions, {np} prompts\n",fmt_row(ctx,row)));
        for (ts, kind, _, text) in bad.iter().take(3) {
            let when = if ts.is_some() {
                fmt_ts(*ts)[5..].to_owned()
            } else {
                " ".repeat(11)
            };
            stdout.push_str(&format!("      {when} {kind:<10} {}\n", text));
        }
    }
    CommandOutput {
        stdout,
        stderr: format!(
            "\n{} of {} sessions active since {} scored >= {min} on {} (tool error 1, api/abort/runtime error 3, user correction 2, compaction 1). Scores find candidates, not failure rates; read the turns around the first problem with `show ID --grep`.\n",
            scored.len(),
            total,
            fmt_ts(since),
            if window.is_open() {
                "all their events".to_owned()
            } else {
                format!(
                    "events {}{}",
                    window
                        .since
                        .map(|d| format!("since {}", fmt_ts(Some(d))))
                        .unwrap_or_default(),
                    window
                        .until
                        .map(|d| format!(" until {}", fmt_ts(Some(d))))
                        .unwrap_or_default()
                )
            }
        ),
        ..Default::default()
    }
}
pub fn classify_touch(e: &Event, needle: &str) -> &'static str {
    let name = e.name.as_deref().unwrap_or("").to_lowercase();
    let input = e.input.as_ref().unwrap_or(&Value::Null);
    if [
        "write",
        "edit",
        "multiedit",
        "notebookedit",
        "apply_patch",
        "create_file",
        "edit_file",
        "write_file",
        "str_replace_based_edit_tool",
        "patch",
    ]
    .contains(&name.as_str())
        || touched_paths(&name, input)
            .iter()
            .any(|p| p.ends_with(needle))
    {
        return "write";
    }
    static WRITE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:>|\btee\b|--out(?:put)?\b|\bmv\b|\bcp\b|\brm\b|build|render|generate)")
            .unwrap()
    });
    static READ: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:ls|cat|head|tail|stat|wc|grep|rg|find|file|git (?:status|log|diff|show)|open)\b").unwrap()
    });
    if WRITE.is_match(&event_text(e)) {
        return "write?";
    }
    if READ.is_match(&tool_summary(&name, input))
        || ["read", "grep", "glob", "read_file", "view", "search"].contains(&name.as_str())
    {
        return "read";
    }
    "mention"
}
pub fn touched<I, T>(
    ctx: &Context,
    rows_events: I,
    path: &Path,
    exact: bool,
    all: bool,
    limit: i64,
) -> Result<CommandOutput>
where
    I: IntoIterator<Item = T>,
    T: Borrow<(Session, Vec<Event>)>,
{
    let path = ctx.expand(&path.to_string_lossy());
    let target = normalized(&if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    });
    let needle = if exact {
        target.to_string_lossy().into_owned()
    } else {
        target
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let mut stdout = if let Ok(meta) = std::fs::metadata(&target) {
        format!(
            "file: {}\n  mtime {} (the last writer should end near this time)\n",
            ctx.tilde(&target),
            fmt_ts(meta.modified().ok().map(DateTime::<Utc>::from))
        )
    } else {
        format!("file: {} (not on disk now)\n", ctx.tilde(&target))
    };
    let mut calls = vec![];
    for item in rows_events {
        let (r, events) = item.borrow();
        for e in events {
            if e.role == "tool" && event_text(e).contains(&needle) {
                let kind = classify_touch(e, &needle);
                if !all && ["read", "mention"].contains(&kind) {
                    continue;
                }
                calls.push((
                    e.ts.or_else(|| r.get("started").and_then(parse_ts)),
                    kind,
                    r.clone(),
                    e.clone(),
                ));
            }
        }
    }
    calls.sort_by_key(|x| x.0);
    // Python calls[-0:] deliberately displays all calls for legacy --limit 0.
    let shown = if limit == 0 {
        &calls[..]
    } else if limit < 0 {
        &calls[usize::try_from(limit.unsigned_abs())
            .unwrap_or(usize::MAX)
            .min(calls.len())..]
    } else {
        tail(&calls, usize::try_from(limit).unwrap_or(usize::MAX))
    };
    for (ts, kind, r, e) in shown {
        stdout.push_str(&format!(
            "{}{}  {kind:<7} {:<8} {}  {}\n    {}\n",
            if e.ts.is_some() { "" } else { "~" },
            fmt_ts(*ts),
            s(r, "harness"),
            s(r, "id"),
            one_line(&label(r), 60),
            redact(&tool_summary(
                e.name.as_deref().unwrap_or(""),
                e.input.as_ref().unwrap_or(&Value::Null)
            ))
        ));
    }
    if let (Some(first), Some(last)) = (calls.first(), calls.last()) {
        stdout.push_str(&format!(
            "\nfirst write-like call: {} {} {} ({})\nlast write-like call:  {} {} {} ({})\n",
            s(&first.2, "harness"),
            s(&first.2, "id"),
            fmt_ts(first.0),
            first.1,
            s(&last.2, "harness"),
            s(&last.2, "id"),
            fmt_ts(last.0),
            last.1
        ));
        if s(&first.2, "id") != s(&last.2, "id") {
            stdout.push_str("the creator and the last writer differ; report both\n");
        }
    }
    Ok(CommandOutput {
        stdout,
        stderr: format!(
            "\n{} {} calls naming {needle}; oldest first; '~' marks a session start time used where the transcript has no per-call timestamp. Confirm the last writer against the mtime and `git log -- <path>`.\n",
            calls.len(),
            if all { "tool" } else { "write-like" }
        ),
        exit_code: if calls.is_empty() { 1 } else { 0 },
        ..Default::default()
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn row() -> Value {
        json!({"id":"fixture-session","harness":"claude","cwd":"/work","started":"2026-09-01T00:00:00+00:00","updated":"2026-09-01T00:00:00+00:00","title":"Fixture"})
    }
    fn tool(name: &str, input: Value) -> Event {
        Event {
            name: Some(name.into()),
            input: Some(input),
            ..Event::new("tool", "", None)
        }
    }
    #[test]
    fn show_filter_and_spill_are_observable() {
        let ctx = Context::from_env();
        let events = vec![
            Event::new("prompt", "alpha", None),
            Event::new("assistant", "A", None),
            Event::new("prompt", "beta", None),
            Event::new("assistant", "B", None),
        ];
        let opts = ShowOptions {
            tail: 1,
            ..Default::default()
        };
        let out = show(&ctx, &row(), &events, &opts).unwrap();
        assert!(out.stdout.contains("showing 2"));
        assert!(!out.stdout.contains("alpha"));
        assert!(out.stdout.contains("beta"));
        let events = vec![Event::new("assistant", "x".repeat(20001), None)];
        let out = show(
            &ctx,
            &row(),
            &events,
            &ShowOptions {
                full: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.artifacts.len(), 1);
        assert!(out.stdout.contains("Turn index:"));
        assert!(out.artifacts[0].1.contains(&"x".repeat(20001)));
    }
    #[test]
    fn failures_weights_and_correction_excludes_first_prompt() {
        let ctx = Context::from_env();
        let mut err = Event::new("result", "failed", None);
        err.error = true;
        let mut sys = Event::new("system", "compact aborted", None);
        sys.error = true;
        let rows = vec![(
            row(),
            vec![
                Event::new("prompt", "No first request", None),
                err,
                Event::new("prompt", "Nope, wrong", None),
                sys,
            ],
        )];
        let out = failures(&ctx, &rows, 1, 10, true, None, EventWindow::default());
        let value: Value = serde_json::from_str(out.stdout.trim()).unwrap();
        assert_eq!(value["score"], 7);
        assert_eq!(value["corrections"], 1);
        assert_eq!(
            value["samples"],
            json!(["compact aborted", "failed", "Nope, wrong"])
        );
    }
    #[test]
    fn handoff_has_numerical_ticket_order_and_deduplicated_files() {
        let ctx = Context::from_env();
        let events = vec![
            Event::new("prompt", "FIX-10 FIX-2 https://github.com/o/r/pull/7", None),
            tool("Edit", json!({"file_path":"src/main.rs"})),
            tool("Edit", json!({"file_path":"src/main.rs"})),
            Event::new("assistant", "Done sk-abcdefghijklmnopqrstuvwxyz", None),
        ];
        let data = handoff_data(&ctx, &row(), &events, &[]);
        assert_eq!(data["tickets"], json!(["FIX-2", "FIX-10"]));
        assert_eq!(data["counts"]["files"], 1);
        assert_eq!(data["last_assistant"]["text"], "Done <redacted>");
        assert_eq!(data["verification"], "unverified");
        assert_eq!(
            data["pull_requests"],
            json!(["https://github.com/o/r/pull/7"])
        );
    }
    #[test]
    fn touch_classification_distinguishes_mutations() {
        assert_eq!(
            classify_touch(&tool("Bash", json!({"command":"cat file.txt"})), "file.txt"),
            "read"
        );
        assert_eq!(
            classify_touch(
                &tool("Bash", json!({"command":"echo x > file.txt"})),
                "file.txt"
            ),
            "write?"
        );
        assert_eq!(
            classify_touch(&tool("Edit", json!({"file_path":"file.txt"})), "file.txt"),
            "write"
        );
    }
    #[test]
    fn handoff_truncation_counts_include_omitted_items() {
        let ctx = Context::from_env();
        let events = (0..26)
            .map(|i| tool("Bash", json!({"command":format!("{i}:{}","x".repeat(200))})))
            .collect::<Vec<_>>();
        let data = handoff_data(&ctx, &row(), &events, &[]);
        assert_eq!(data["counts"]["commands"], 26);
        assert_eq!(data["commands"].as_array().unwrap().len(), 25);
        assert_eq!(data["truncation"]["commands"], true);
        assert_eq!(data["commands"][0]["truncated"], true);
        assert!(
            data["commands"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("1:")
        );
    }
    #[test]
    fn touched_reports_creator_and_last_writer_despite_limit() {
        let ctx = Context::from_env();
        let mut first = row();
        first["id"] = json!("creator");
        let mut last = row();
        last["id"] = json!("last-writer");
        let mut a = tool("Edit", json!({"file_path":"unlikely-fixture-file.txt"}));
        a.ts = parse_ts(&json!("2026-09-01T00:00:00Z"));
        let mut b = a.clone();
        b.ts = parse_ts(&json!("2026-09-02T00:00:00Z"));
        let out = touched(
            &ctx,
            &[(last, vec![b]), (first, vec![a])],
            Path::new("unlikely-fixture-file.txt"),
            false,
            false,
            1,
        )
        .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("first write-like call: claude creator"));
        assert!(
            out.stdout
                .contains("last write-like call:  claude last-writer")
        );
        assert!(
            out.stdout
                .contains("the creator and the last writer differ")
        );
        assert!(!out.stdout.contains("creator  Fixture"));
        let out = touched(
            &ctx,
            &[],
            Path::new("unlikely-fixture-file.txt"),
            false,
            false,
            1,
        )
        .unwrap();
        assert_eq!(out.exit_code, 1);
    }

    #[test]
    fn scan_commands_drop_each_input_session_before_loading_the_next() {
        use std::{cell::Cell, rc::Rc};
        struct TrackedSession {
            pair: (Session, Vec<Event>),
            alive: Rc<Cell<bool>>,
        }
        impl Borrow<(Session, Vec<Event>)> for TrackedSession {
            fn borrow(&self) -> &(Session, Vec<Event>) {
                &self.pair
            }
        }
        impl Drop for TrackedSession {
            fn drop(&mut self) {
                self.alive.set(false);
            }
        }
        let ctx = Context::from_env();
        for touch in [false, true] {
            let alive = Rc::new(Cell::new(false));
            let input = (0..4).map(|i| {
                assert!(
                    !alive.get(),
                    "previous session was retained across iterator advancement"
                );
                alive.set(true);
                let mut session = row();
                session["id"] = json!(format!("session-{i}"));
                let mut error = Event::new("result", "failed", None);
                error.error = true;
                TrackedSession {
                    pair: (
                        session,
                        vec![
                            error,
                            tool("Edit", json!({"file_path":"streaming-fixture.txt"})),
                        ],
                    ),
                    alive: alive.clone(),
                }
            });
            if touch {
                let out = touched(
                    &ctx,
                    input,
                    Path::new("streaming-fixture.txt"),
                    false,
                    false,
                    2,
                )
                .unwrap();
                assert!(out.stderr.contains("4 write-like calls"));
                assert!(
                    out.stdout
                        .contains("first write-like call: claude session-0")
                );
                assert!(
                    out.stdout
                        .contains("last write-like call:  claude session-3")
                );
            } else {
                let out = failures(&ctx, input, 1, 2, true, None, EventWindow::default());
                assert_eq!(out.stdout.lines().count(), 2);
                assert!(out.stderr.contains("4 of 4 sessions"));
            }
            assert!(!alive.get());
        }
    }
}
