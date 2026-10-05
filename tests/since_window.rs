//! `--since` selects sessions by last recorded activity, and `failures`
//! scores only events inside the window. Runs the binary against a synthetic
//! store with a cleared environment, so no real store is read.
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

const NOW: &str = "2026-10-05T12:00:00Z";
const STALE: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const RESUMED: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const FAILING: &str = "aaaaaaaa-0000-4000-8000-000000000003";

fn user(ts: &str, text: &str) -> Value {
    json!({"type": "user", "cwd": "/w", "timestamp": ts, "message": {"content": text}})
}
fn tool_error(ts: &str, id: &str) -> Vec<Value> {
    vec![
        json!({"type": "assistant", "timestamp": ts, "message": {"model": "m", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "false"}}]}}),
        json!({"type": "user", "timestamp": ts, "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": format!("error {ts}"), "is_error": true}]}}),
    ]
}
fn write(home: &Path, id: &str, records: Vec<Value>, tail: &str) {
    let dir = home.join(".claude/projects/w");
    fs::create_dir_all(&dir).unwrap();
    let mut body = records
        .iter()
        .map(|r| r.to_string() + "\n")
        .collect::<String>();
    body.push_str(tail);
    fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
}

fn store() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    // Last real activity 8 Sep; a harness appended an undated line today,
    // so the file mtime is current.
    let mut stale = vec![user("2026-09-08T21:42:00Z", "old work")];
    stale.extend(tool_error("2026-09-08T22:00:00Z", "s1"));
    stale.push(json!({"type": "permission-mode", "permissionMode": "default"}));
    write(home, STALE, stale, "");
    // Errors on 25 Sep, resumed cleanly on 2 Oct.
    let mut resumed = vec![user("2026-09-25T21:00:00Z", "start")];
    resumed.extend(tool_error("2026-09-25T21:10:00Z", "r1"));
    resumed.extend(tool_error("2026-09-25T21:20:00Z", "r2"));
    resumed.push(user("2026-10-02T10:00:00Z", "carry on"));
    write(home, RESUMED, resumed, "");
    // Errors this week, and a final record cut off mid-write.
    let mut failing = vec![user("2026-10-03T09:00:00Z", "fix it")];
    failing.extend(tool_error("2026-10-03T09:10:00Z", "f1"));
    failing.extend(tool_error("2026-10-03T09:20:00Z", "f2"));
    write(
        home,
        FAILING,
        failing,
        r#"{"type":"user","message":{"content":"x\u12"#,
    );
    t
}

fn run(home: &Path, args: &[&str]) -> (Vec<Value>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agent-traces"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("AGENT_TRACES_NOW", NOW)
        .env("AGENT_TRACES_CACHE", home.join("cache"))
        .env("AGENT_TRACES_OUT", home.join("out"))
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let rows = if args.contains(&"--json") {
        stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    } else {
        vec![]
    };
    (rows, String::from_utf8(out.stderr).unwrap())
}

fn ids(rows: &[Value]) -> Vec<&str> {
    let mut ids = rows
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

#[test]
fn since_drops_a_session_whose_only_recent_change_is_undated() {
    let t = store();
    let (rows, _) = run(t.path(), &["ls", "--since", "7d", "--json"]);
    assert_eq!(ids(&rows), [RESUMED, FAILING]);
    let (rows, _) = run(
        t.path(),
        &[
            "failures",
            "--since",
            "7d",
            "--min",
            "1",
            "--all-events",
            "--json",
        ],
    );
    assert!(!ids(&rows).contains(&STALE));
}

#[test]
fn failures_score_only_events_inside_the_window() {
    let t = store();
    let (rows, stderr) = run(
        t.path(),
        &["failures", "--since", "7d", "--min", "1", "--json"],
    );
    assert_eq!(ids(&rows), [FAILING]);
    assert_eq!(rows[0]["tool_errors"], 2);
    assert!(stderr.contains("on events since"), "{stderr}");

    let (rows, stderr) = run(
        t.path(),
        &[
            "failures",
            "--since",
            "7d",
            "--min",
            "1",
            "--all-events",
            "--json",
        ],
    );
    assert_eq!(ids(&rows), [RESUMED, FAILING]);
    let resumed = rows.iter().find(|r| r["id"] == RESUMED).unwrap();
    assert_eq!(resumed["tool_errors"], 2);
    assert_eq!(resumed["samples"][0], "error 2026-09-25T21:10:00Z");
    assert!(stderr.contains("on all their events"), "{stderr}");
}

#[test]
fn a_cut_off_record_is_reported_with_its_store() {
    let t = store();
    let (_, stderr) = run(t.path(), &["failures", "--since", "7d", "--min", "1"]);
    let line = stderr
        .lines()
        .find(|l| l.contains("unterminated final record"))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(
        line.contains(&format!("{FAILING}.jsonl: line 6:")),
        "{line}"
    );
}
