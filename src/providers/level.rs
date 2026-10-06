//! Level keyword highlighting for log lines that carry no ANSI color: finds
//! the first token such as `ERROR`, `[WARN]`, `level=info` or
//! `"level":"debug"` and marks it with a [`LogLevel`]. Provider-agnostic.

use std::ops::Range;

use crate::model::{LogLevel, LogSpan, LogStyle};

/// Adds a level span to `spans` (the SGR spans of `text`) unless the line is
/// already colored by the service itself.
#[must_use]
pub fn with_level(text: &str, spans: Vec<LogSpan>) -> Vec<LogSpan> {
    if spans.iter().any(|s| s.style.has_color()) {
        return spans;
    }
    let Some((range, level)) = find_level(text) else {
        return spans;
    };
    let start = text[..range.start].encode_utf16().count();
    let end = start + text[range].encode_utf16().count();
    overlay(spans, start, end, level)
}

/// Finds the first level keyword in `text`, as a byte range and its level.
///
/// Recognised forms:
/// - bare uppercase words: `INFO`, `[WARN]`, `ERROR:` (exact case, whole word);
/// - `level=…`, `lvl=…`, `severity=…` and their JSON / quoted variants
///   (`"level":"info"`), keys and values case-insensitive;
/// - the glog / klog prefix `I1005 12:00:00…` (`I`, `W`, `E`, `F` + `MMDD`).
#[must_use]
pub fn find_level(text: &str) -> Option<(Range<usize>, LogLevel)> {
    let b = text.as_bytes();
    if let Some(level) = glog(b) {
        return Some((0..1, level));
    }

    let mut i = 0;
    while i < b.len() {
        if !is_word(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        // Word boundaries are ASCII bytes, so these are char boundaries.
        let word = &text[start..i];
        if let Some(level) = bare(word) {
            return Some((start..i, level));
        }
        if is_key(word)
            && let Some(hit) = value_after(text, i)
        {
            return Some(hit);
        }
    }
    None
}

/// Non-ASCII bytes count as word characters so `É` never acts as a boundary.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn bare(word: &str) -> Option<LogLevel> {
    Some(match word {
        "DEBUG" | "TRACE" => LogLevel::Debug,
        "INFO" => LogLevel::Info,
        "WARN" | "WARNING" => LogLevel::Warn,
        "ERROR" | "ERR" | "FATAL" | "CRITICAL" | "PANIC" => LogLevel::Error,
        _ => return None,
    })
}

fn is_key(word: &str) -> bool {
    ["level", "lvl", "severity"]
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k))
}

fn value(word: &str) -> Option<LogLevel> {
    Some(match word.to_ascii_lowercase().as_str() {
        "debug" | "trace" => LogLevel::Debug,
        "info" | "notice" => LogLevel::Info,
        "warn" | "warning" => LogLevel::Warn,
        "error" | "err" | "fatal" | "critical" | "crit" | "panic" | "alert" | "emerg" => {
            LogLevel::Error
        }
        _ => return None,
    })
}

/// Parses `["']? \s* [=:] \s* ["']? word` after a key ending at `i`.
fn value_after(text: &str, mut i: usize) -> Option<(Range<usize>, LogLevel)> {
    let b = text.as_bytes();
    let at = |i: usize| b.get(i).copied();
    let skip_spaces = |mut i: usize| {
        while at(i) == Some(b' ') {
            i += 1;
        }
        i
    };

    if matches!(at(i), Some(b'"' | b'\'')) {
        i += 1;
    }
    i = skip_spaces(i);
    if !matches!(at(i), Some(b'=' | b':')) {
        return None;
    }
    i = skip_spaces(i + 1);
    if matches!(at(i), Some(b'"' | b'\'')) {
        i += 1;
    }
    let start = i;
    while at(i).is_some_and(is_word) {
        i += 1;
    }
    value(&text[start..i]).map(|level| (start..i, level))
}

/// `I1005 12:00:00.000000 …`: severity letter, month and day, space.
fn glog(b: &[u8]) -> Option<LogLevel> {
    if b.len() < 7 || !b[1..5].iter().all(u8::is_ascii_digit) || b[5] != b' ' {
        return None;
    }
    if !b[6].is_ascii_digit() {
        return None;
    }
    Some(match b[0] {
        b'I' => LogLevel::Info,
        b'W' => LogLevel::Warn,
        b'E' | b'F' => LogLevel::Error,
        _ => return None,
    })
}

/// Marks `start..end` with `level`, splitting any SGR spans (bold, …) it
/// crosses so the result stays sorted and non-overlapping.
fn overlay(spans: Vec<LogSpan>, start: usize, end: usize, level: LogLevel) -> Vec<LogSpan> {
    let mut out = Vec::with_capacity(spans.len() + 3);
    let gap = |from: usize, to: usize| LogSpan {
        start: from,
        end: to,
        style: LogStyle::default(),
        level: Some(level),
    };
    // Everything in start..cursor is already covered.
    let mut cursor = start;

    for s in spans {
        if s.end <= start || s.start >= end {
            if s.start >= end && cursor < end {
                out.push(gap(cursor, end));
                cursor = end;
            }
            out.push(s);
            continue;
        }
        if s.start < start {
            out.push(LogSpan {
                end: start,
                ..s.clone()
            });
        }
        let (from, to) = (s.start.max(start), s.end.min(end));
        if cursor < from {
            out.push(gap(cursor, from));
        }
        out.push(LogSpan {
            start: from,
            end: to,
            style: s.style,
            level: Some(level),
        });
        cursor = to;
        if s.end > end {
            out.push(LogSpan { start: end, ..s });
        }
    }
    if cursor < end {
        out.push(gap(cursor, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ansi;

    fn find(text: &str) -> Option<(&str, LogLevel)> {
        find_level(text).map(|(r, l)| (&text[r], l))
    }

    #[test]
    fn bare_uppercase_tokens() {
        use LogLevel::{Debug, Error, Info, Warn};
        assert_eq!(find("INFO server started"), Some(("INFO", Info)));
        assert_eq!(find("[WARN] disk 90%"), Some(("WARN", Warn)));
        assert_eq!(
            find("2026-10-05 12:00:01 WARNING x"),
            Some(("WARNING", Warn))
        );
        assert_eq!(find("ERROR: boom"), Some(("ERROR", Error)));
        assert_eq!(find("<ERR> x"), Some(("ERR", Error)));
        assert_eq!(find("FATAL x"), Some(("FATAL", Error)));
        assert_eq!(find("CRITICAL x"), Some(("CRITICAL", Error)));
        assert_eq!(find("PANIC x"), Some(("PANIC", Error)));
        assert_eq!(find("DEBUG x"), Some(("DEBUG", Debug)));
        assert_eq!(find("TRACE x"), Some(("TRACE", Debug)));
        assert_eq!(find("INFO:root:hello"), Some(("INFO", Info)));
    }

    #[test]
    fn only_the_first_token_counts() {
        assert_eq!(
            find("INFO retrying after ERROR"),
            Some(("INFO", LogLevel::Info))
        );
        assert_eq!(
            find(r#"level=info msg="ERROR in payload""#),
            Some(("info", LogLevel::Info))
        );
    }

    #[test]
    fn no_false_positives() {
        assert_eq!(find("information is power"), None);
        assert_eq!(
            find("an error occurred"),
            None,
            "bare tokens are uppercase only"
        );
        assert_eq!(find("Error: lowercase-ish"), None);
        assert_eq!(find("ERRORS: 0"), None);
        assert_eq!(find("ERR_CONNECTION_REFUSED"), None);
        assert_eq!(find("MY_INFO=1"), None);
        assert_eq!(find("ÉINFO"), None);
        assert_eq!(find("level=unknown"), None);
        assert_eq!(find("the level is high"), None);
        assert_eq!(find("I am here"), None);
        assert_eq!(find("E 1234 x"), None);
        assert_eq!(find(""), None);
    }

    #[test]
    fn key_value_forms() {
        use LogLevel::{Debug, Error, Info, Warn};
        assert_eq!(find("time=x level=error msg=y"), Some(("error", Error)));
        assert_eq!(find("lvl=WARN"), Some(("WARN", Warn)));
        assert_eq!(find(r#"level="debug" msg=x"#), Some(("debug", Debug)));
        assert_eq!(
            find(r#"{"ts":1,"level":"info","msg":"x"}"#),
            Some(("info", Info))
        );
        assert_eq!(find(r#"{"Level": "Warning"}"#), Some(("Warning", Warn)));
        assert_eq!(
            find(r#"{"severity":"CRITICAL"}"#),
            Some(("CRITICAL", Error))
        );
        assert_eq!(find("Level: notice"), Some(("notice", Info)));
        assert_eq!(find("level=fatal"), Some(("fatal", Error)));
        // A key without a level value does not stop the search.
        assert_eq!(find("level=7 then WARN"), Some(("WARN", Warn)));
    }

    #[test]
    fn glog_prefix() {
        assert_eq!(
            find("I1005 18:44:01.123456   1 main.go:10] up"),
            Some(("I", LogLevel::Info))
        );
        assert_eq!(find("W1005 18:44:01.1 x"), Some(("W", LogLevel::Warn)));
        assert_eq!(find("E1005 18:44:01.1 x"), Some(("E", LogLevel::Error)));
        assert_eq!(find("F1005 18:44:01.1 x"), Some(("F", LogLevel::Error)));
        assert_eq!(find("X1005 18:44:01.1 x"), None);
    }

    fn level_spans(line: &str) -> (String, Vec<LogSpan>) {
        let s = ansi::parse(line);
        let spans = with_level(&s.text, s.spans);
        (s.text, spans)
    }

    #[test]
    fn colored_lines_are_left_alone() {
        let (_, spans) = level_spans("\x1b[32mINFO\x1b[0m ok");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].level, None);
    }

    #[test]
    fn level_span_uses_utf16_offsets() {
        let (text, spans) = level_spans("🦀 é ERROR x");
        assert_eq!(
            spans,
            [LogSpan {
                start: 5,
                end: 10,
                style: LogStyle::default(),
                level: Some(LogLevel::Error),
            }]
        );
        let utf16: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(String::from_utf16_lossy(&utf16[5..10]), "ERROR");
    }

    #[test]
    fn level_splits_attribute_only_spans() {
        let bold = LogStyle {
            bold: true,
            ..LogStyle::default()
        };
        let span = |start, end, style, level| LogSpan {
            start,
            end,
            style,
            level,
        };
        let err = Some(LogLevel::Error);
        let plain = LogStyle::default();

        // Bold covers "[ERR" — the level token "ERR" is split out of it.
        let (_, spans) = level_spans("\x1b[1m[ERR\x1b[0m] x");
        assert_eq!(spans, [span(0, 1, bold, None), span(1, 4, bold, err)]);

        // Bold covers "RR] x" only: gap before, then the bold part.
        let (_, spans) = level_spans("[E\x1b[1mRR] x\x1b[0m");
        assert_eq!(
            spans,
            [
                span(1, 2, plain, err),
                span(2, 4, bold, err),
                span(4, 7, bold, None),
            ]
        );

        // Bold elsewhere, before and after the token.
        let (_, spans) = level_spans("\x1b[1ma\x1b[0m WARN \x1b[1mb\x1b[0m");
        assert_eq!(
            spans,
            [
                span(0, 1, bold, None),
                span(2, 6, plain, Some(LogLevel::Warn)),
                span(7, 8, bold, None),
            ]
        );
    }
}
