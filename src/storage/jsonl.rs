//! Streaming, permissive JSON Lines readers shared by store adapters.
use crate::context::Context;
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

// Python's json.loads(strict=False) accepts literal control bytes inside strings.
pub fn decode(raw: &[u8]) -> serde_json::Result<Value> {
    if let Ok(value) = serde_json::from_slice(raw) {
        return Ok(value);
    }
    let text = String::from_utf8_lossy(raw);
    let mut clean = String::with_capacity(text.len());
    let (mut quoted, mut escaped) = (false, false);
    for c in text.chars() {
        if quoted && !escaped && (c as u32) < 32 {
            clean.push_str(&format!("\\u{:04x}", c as u32));
            continue;
        }
        clean.push(c);
        if escaped {
            escaped = false;
        } else if c == '\\' && quoted {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        }
    }
    serde_json::from_str(&clean)
}

pub fn records<'a>(
    ctx: &'a Context,
    path: &Path,
    start: usize,
    needles: Option<&[String]>,
) -> Box<dyn Iterator<Item = (usize, Value)> + 'a> {
    ctx.protect_source(path);
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            ctx.diagnostic("store_unreadable", None, Some(path), e.to_string());
            return Box::new(std::iter::empty());
        }
    };
    let path = path.to_owned();
    let needles = needles
        .unwrap_or_default()
        .iter()
        .map(|n| n.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut index = 0;
    let mut finished = false;
    Box::new(std::iter::from_fn(move || {
        while !finished {
            raw.clear();
            let i = index;
            index += 1;
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    finished = true;
                    ctx.diagnostic("store_unreadable", None, Some(&path), e.to_string());
                    return None;
                }
            }
            if i < start {
                continue;
            }
            if !needles.is_empty() {
                let lower = raw.to_ascii_lowercase();
                if !needles
                    .iter()
                    .any(|n| n.is_empty() || lower.windows(n.len()).any(|w| w == n))
                {
                    ctx.skipped.set(ctx.skipped.get() + 1);
                    continue;
                }
            }
            match decode(&raw) {
                Ok(v) => return Some((i, v)),
                // A live session's last record can be read mid-write.
                Err(e) if raw.last() != Some(&b'\n') => ctx.diagnostic(
                    "malformed_record",
                    None,
                    Some(&path),
                    format!(
                        "line {}: unterminated final record skipped (file may still be written): {e}",
                        i + 1
                    ),
                ),
                Err(e) => ctx.diagnostic(
                    "malformed_record",
                    None,
                    Some(&path),
                    format!("line {}: {e}", i + 1),
                ),
            }
        }
        None
    }))
}

pub fn tail_lines(ctx: &Context, path: &Path, nbytes: usize) -> Vec<Value> {
    ctx.protect_source(path);
    let data = (|| -> std::io::Result<(u64, Vec<u8>)> {
        let mut f = File::open(path)?;
        let size = f.metadata()?.len();
        f.seek(SeekFrom::Start(size.saturating_sub(nbytes as u64)))?;
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        Ok((size, b))
    })();
    let (size, data) = match data {
        Ok(v) => v,
        Err(e) => {
            ctx.diagnostic("store_unreadable", None, Some(path), e.to_string());
            return vec![];
        }
    };
    String::from_utf8_lossy(&data)
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            if size > nbytes as u64 && i == 0 {
                return None;
            }
            match decode(l.as_bytes()) {
                Ok(v) => Some(v),
                Err(e) => {
                    ctx.diagnostic(
                        "malformed_record",
                        None,
                        Some(path),
                        format!("tail line {}: {e}", i + 1),
                    );
                    None
                }
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permissive_and_skipped() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x");
        std::fs::write(&p, b"broken\n{\"s\":\"A\x01\xff\"}\n{\"s\":\"needle\"}\n").unwrap();
        let ctx = Context::from_env();
        let v = records(&ctx, &p, 0, None).collect::<Vec<_>>();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, 1);
        assert_eq!(v[0].1["s"], "A\u{1}\u{fffd}");
        assert_eq!(ctx.diagnostics.borrow().len(), 1);
        let v = records(&ctx, &p, 0, Some(&["needle".into()])).collect::<Vec<_>>();
        assert_eq!(v[0].0, 2);
        assert_eq!(ctx.skipped.get(), 2);
    }
    #[test]
    fn an_unterminated_last_record_is_named_as_such() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("live.jsonl");
        // A writer stopped mid-record, partway into a \u escape.
        std::fs::write(&p, b"{\"a\":1}\nbroken\n{\"s\":\"x\\u12").unwrap();
        let ctx = Context::from_env();
        assert_eq!(records(&ctx, &p, 0, None).count(), 1);
        let d = ctx.diagnostics.borrow();
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|d| d.code == "malformed_record"));
        assert!(!d[0].message.contains("unterminated"));
        assert!(
            d[1].message
                .starts_with("line 3: unterminated final record")
        );
    }
    #[test]
    fn tail_drops_partial_first_line() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x");
        std::fs::write(&p, b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();
        assert_eq!(
            tail_lines(&Context::from_env(), &p, 12),
            vec![serde_json::json!({"a":3})]
        );
    }
    #[test]
    fn nonfinite_numbers_are_reported_as_malformed_not_coerced() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("numbers.jsonl");
        std::fs::write(&p,b"{\"x\":NaN}\n{\"x\":Infinity}\n{\"x\":-Infinity}\n{\"x\":\"NaN Infinity -Infinity\"}\n").unwrap();
        let ctx = Context::from_env();
        let rows = records(&ctx, &p, 0, None).collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 3);
        assert_eq!(rows[0].1["x"], "NaN Infinity -Infinity");
        let diagnostics = ctx.diagnostics.borrow();
        assert_eq!(diagnostics.len(), 3);
        assert!(diagnostics.iter().all(|d| d.code == "malformed_record"));
    }
}
