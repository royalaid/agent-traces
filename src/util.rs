use crate::model::{Event, Session};
use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
pub fn opt_s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
pub fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    if v.is_null() {
        return None;
    }
    if let Some(mut n) = v.as_f64() {
        if n > 1e12 {
            n /= 1000.;
        }
        let mut sec = n.floor() as i64;
        let mut micros = ((n - n.floor()) * 1e6).round() as u32;
        if micros >= 1_000_000 {
            sec = sec.checked_add(1)?;
            micros = 0;
        }
        return Utc.timestamp_opt(sec, micros * 1000).single();
    }
    let text = v.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().all(|c| c.is_ascii_digit() || c == '.') && (10..=17).contains(&text.len()) {
        return text
            .parse::<f64>()
            .ok()
            .and_then(|n| parse_ts(&serde_json::json!(n)));
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|d| d.and_utc())
        })
        .or_else(|| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc())
        })
}
pub fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(
        if dt.timestamp_subsec_micros() == 0 {
            chrono::SecondsFormat::Secs
        } else {
            chrono::SecondsFormat::Micros
        },
        false,
    )
}
pub fn parse_since(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if text.len() > 1 {
        let (number, unit) = text.split_at(text.char_indices().last().map(|(i, _)| i).unwrap_or(0));
        if let Ok(n) = number.parse::<i64>() {
            let scale = match unit {
                "m" => 60,
                "h" => 3600,
                "d" => 86400,
                "w" => 604800,
                _ => 0,
            };
            if scale > 0 {
                return now
                    .checked_sub_signed(
                        chrono::Duration::try_seconds(
                            n.checked_mul(scale)
                                .ok_or_else(|| anyhow::anyhow!("time overflow"))?,
                        )
                        .ok_or_else(|| anyhow::anyhow!("time out of range"))?,
                    )
                    .ok_or_else(|| anyhow::anyhow!("time out of range"));
            }
        }
    }
    if text.len() == 10
        && let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d")
    {
        if utc_requested() {
            return Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
        }
        if let Some(dt) = d
            .and_hms_opt(0, 0, 0)
            .and_then(|d| Local.from_local_datetime(&d).earliest())
        {
            return Ok(dt.with_timezone(&Utc));
        }
    }
    if let Some(d) = parse_ts(&Value::String(text.into())) {
        return Ok(d);
    }
    bail!("cannot parse time {text:?}; use 7d, 36h, 90m, or 2026-09-01")
}
pub fn one_line(text: &str, n: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!(
            "{}…",
            t.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}
pub fn clip(text: &str, n: usize) -> String {
    let t = text.trim();
    let len = t.chars().count();
    if len <= n {
        t.into()
    } else {
        format!(
            "{}\n[… {} more chars]",
            t.chars().take(n).collect::<String>().trim_end(),
            len - n
        )
    }
}
pub fn label(row: &Session) -> String {
    for k in ["t3_title", "title"] {
        if !s(row, k).is_empty() {
            return s(row, k).into();
        }
    }
    let prompt = s(row, "first_prompt");
    if prompt.is_empty() {
        "(untitled)".into()
    } else {
        one_line(prompt, 90)
    }
}
pub fn event_text(e: &Event) -> String {
    if e.role == "tool" {
        format!(
            "{} {}",
            e.name.as_deref().unwrap_or(""),
            e.input
                .as_ref()
                .map(|v| v
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| python_json(v)))
                .unwrap_or_else(|| "null".into())
        )
    } else {
        e.text.clone()
    }
}
pub fn python_json(v: &Value) -> String {
    // Python json.dumps separators, preserving insertion order.
    match v {
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    serde_json::to_string(k).unwrap_or_default(),
                    python_json(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        _ => v.to_string(),
    }
}
pub fn redact(text: &str) -> String {
    use std::sync::LazyLock;
    static P: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
        [
            r"sk-(?:ant-)?[A-Za-z0-9_\-]{20,}",
            r"gh[pousr]_[A-Za-z0-9]{30,}",
            r"glpat[-][A-Za-z0-9_\-]{20,}",
            r"xox[abprs]-[A-Za-z0-9\-]{10,}",
            r"AKIA[0-9A-Z]{16}",
            r"(?i)(bearer\s+)[A-Za-z0-9._\-]{20,}",
            r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        ]
        .into_iter()
        .map(|p| regex::Regex::new(p).expect("static regex"))
        .collect()
    });
    let mut out = text.to_owned();
    for p in P.iter() {
        out = p
            .replace_all(&out, |c: &regex::Captures<'_>| {
                format!("{}<redacted>", c.get(1).map_or("", |m| m.as_str()))
            })
            .into_owned();
    }
    out
}
pub fn utc_requested() -> bool {
    std::env::var("TZ").is_ok_and(|s| matches!(s.as_str(), "UTC" | "UTC0" | "GMT" | "GMT0"))
}
pub fn fmt_ts(dt: Option<DateTime<Utc>>) -> String {
    dt.map(|d| {
        if utc_requested() {
            d.format("%Y-%m-%d %H:%M").to_string()
        } else {
            d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
        }
    })
    .unwrap_or_else(|| "????-??-?? ??:??".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_unicode_and_huge_relative_dates_return_errors() {
        let now = Utc::now();
        for text in ["昨日", "é", "99999999999999999d"] {
            assert!(parse_since(text, now).is_err());
        }
    }
    #[test]
    fn epoch_milliseconds_round_to_python_microseconds() {
        let d = parse_ts(&serde_json::json!(1790330400172_i64)).unwrap();
        assert_eq!(d.timestamp_subsec_micros(), 172000);
    }
}
