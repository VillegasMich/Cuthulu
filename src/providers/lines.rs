//! Turning raw log chunks into clean [`LogLine`]s: line splitting, timestamp
//! extraction and ANSI escape removal. Provider-agnostic.

use std::collections::VecDeque;

use crate::model::{LogLine, LogStream};

/// A line longer than this is emitted in pieces instead of buffered forever.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Reassembles lines from arbitrary chunks, one buffer per stream.
#[derive(Debug, Default)]
pub struct LineSplitter {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Whether each line starts with an RFC 3339 timestamp and a space.
    timestamps: bool,
}

impl LineSplitter {
    #[must_use]
    pub fn new(timestamps: bool) -> Self {
        Self {
            timestamps,
            ..Self::default()
        }
    }

    /// Adds a chunk and moves every completed line into `out`.
    pub fn push(&mut self, stream: LogStream, chunk: &[u8], out: &mut VecDeque<LogLine>) {
        let timestamps = self.timestamps;
        let buf = self.buf(stream);
        buf.extend_from_slice(chunk);

        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|&b| b == b'\n') {
            out.push_back(parse_line(stream, &buf[start..start + pos], timestamps));
            start += pos + 1;
        }
        buf.drain(..start);

        if buf.len() > MAX_LINE_BYTES {
            let line = parse_line(stream, buf, timestamps);
            buf.clear();
            out.push_back(line);
        }
    }

    /// Emits whatever is left without a trailing newline.
    pub fn flush(&mut self, out: &mut VecDeque<LogLine>) {
        let timestamps = self.timestamps;
        for stream in [LogStream::Stdout, LogStream::Stderr] {
            let buf = self.buf(stream);
            if !buf.is_empty() {
                let line = parse_line(stream, buf, timestamps);
                buf.clear();
                out.push_back(line);
            }
        }
    }

    fn buf(&mut self, stream: LogStream) -> &mut Vec<u8> {
        match stream {
            LogStream::Stdout => &mut self.stdout,
            LogStream::Stderr => &mut self.stderr,
        }
    }
}

fn parse_line(stream: LogStream, raw: &[u8], timestamps: bool) -> LogLine {
    let raw = String::from_utf8_lossy(raw);
    let raw = raw.strip_suffix('\r').unwrap_or(&raw);

    let (ts, text) = match raw.split_once(' ') {
        Some((ts, text)) if timestamps && looks_like_timestamp(ts) => (Some(ts.to_owned()), text),
        _ => (None, raw),
    };

    LogLine {
        ts,
        stream,
        text: strip_ansi(text),
    }
}

fn looks_like_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 20 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[10] == b'T'
}

/// Removes ANSI escape sequences (colors, cursor movement, OSC titles).
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: ESC [ params… final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … terminated by BEL or ESC \
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-byte sequences such as ESC ( B: drop the next char too.
            Some('(' | ')') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(s: &mut LineSplitter, stream: LogStream, chunks: &[&str]) -> Vec<LogLine> {
        let mut out = VecDeque::new();
        for c in chunks {
            s.push(stream, c.as_bytes(), &mut out);
        }
        out.into()
    }

    #[test]
    fn splits_across_chunks() {
        let mut s = LineSplitter::new(false);
        let lines = collect(&mut s, LogStream::Stdout, &["ab", "c\nde", "f\n", "tail"]);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["abc", "def"]);

        let mut rest = VecDeque::new();
        s.flush(&mut rest);
        assert_eq!(rest[0].text, "tail");
    }

    #[test]
    fn keeps_streams_separate() {
        let mut s = LineSplitter::new(false);
        let mut out = VecDeque::new();
        s.push(LogStream::Stdout, b"out-", &mut out);
        s.push(LogStream::Stderr, b"err\n", &mut out);
        s.push(LogStream::Stdout, b"line\n", &mut out);
        assert_eq!(out[0].text, "err");
        assert_eq!(out[0].stream, LogStream::Stderr);
        assert_eq!(out[1].text, "out-line");
    }

    #[test]
    fn extracts_timestamp() {
        let mut s = LineSplitter::new(true);
        let lines = collect(
            &mut s,
            LogStream::Stdout,
            &["2026-10-05T18:44:01.123456789Z listening on :80\r\n"],
        );
        assert_eq!(
            lines[0].ts.as_deref(),
            Some("2026-10-05T18:44:01.123456789Z")
        );
        assert_eq!(lines[0].text, "listening on :80");
    }

    #[test]
    fn no_timestamp_when_disabled_or_absent() {
        let mut s = LineSplitter::new(true);
        let lines = collect(&mut s, LogStream::Stdout, &["plain line\n"]);
        assert_eq!(lines[0].ts, None);
        assert_eq!(lines[0].text, "plain line");
    }

    #[test]
    fn caps_runaway_lines() {
        let mut s = LineSplitter::new(false);
        let big = "x".repeat(MAX_LINE_BYTES + 1);
        let lines = collect(&mut s, LogStream::Stdout, &[&big]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(strip_ansi("\x1b(Bok"), "ok");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }
}
