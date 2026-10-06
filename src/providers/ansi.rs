//! ANSI escape handling for log text: SGR sequences (colors, bold, …) become
//! [`LogSpan`]s over the plain text; every other escape sequence (cursor
//! movement, OSC titles, charset switches) is removed. Provider-agnostic.

use crate::model::{LogSpan, LogStyle};

/// Plain text plus its styled ranges (UTF-16 offsets, see [`LogSpan`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Styled {
    pub text: String,
    pub spans: Vec<LogSpan>,
}

/// Splits `s` into plain text and SGR style spans.
#[must_use]
pub fn parse(s: &str) -> Styled {
    let mut out = Builder::default();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: ESC [ params… final byte in @..~; only `m` (SGR) is kept.
            Some('[') => {
                let mut params = String::new();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        if c == 'm' {
                            let mut style = out.style;
                            apply_sgr(&mut style, &params);
                            out.set_style(style);
                        }
                        break;
                    }
                    params.push(c);
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
    out.finish()
}

/// Removes every ANSI escape sequence, colors included.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    parse(s).text
}

#[derive(Default)]
struct Builder {
    text: String,
    spans: Vec<LogSpan>,
    style: LogStyle,
    /// UTF-16 offset where the current style started.
    run_start: usize,
    /// UTF-16 length of `text`.
    pos: usize,
}

impl Builder {
    fn push(&mut self, c: char) {
        self.text.push(c);
        self.pos += c.len_utf16();
    }

    fn set_style(&mut self, style: LogStyle) {
        if style != self.style {
            self.close_run();
            self.style = style;
        }
    }

    fn close_run(&mut self) {
        if self.pos > self.run_start && self.style != LogStyle::default() {
            match self.spans.last_mut() {
                Some(last) if last.end == self.run_start && last.style == self.style => {
                    last.end = self.pos;
                }
                _ => self.spans.push(LogSpan {
                    start: self.run_start,
                    end: self.pos,
                    style: self.style,
                    level: None,
                }),
            }
        }
        self.run_start = self.pos;
    }

    fn finish(mut self) -> Styled {
        self.close_run();
        Styled {
            text: self.text,
            spans: self.spans,
        }
    }
}

/// Applies one SGR parameter string (the part between `ESC [` and `m`).
fn apply_sgr(style: &mut LogStyle, params: &str) {
    // Private or malformed sequences (e.g. `ESC [ ? … m`) are ignored.
    if !params
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b';' || b == b':')
    {
        return;
    }
    let mut it = params.split(';');
    while let Some(param) = it.next() {
        let mut sub = param.split(':');
        // An empty parameter means 0 (reset), as in `ESC [ m` or `ESC [ ; 1 m`.
        let code = match sub.next().unwrap_or_default() {
            "" => 0,
            c => match c.parse::<u16>() {
                Ok(code) => code,
                Err(_) => continue,
            },
        };
        match code {
            0 => *style = LogStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => (style.bold, style.dim) = (false, false),
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(index(code - 30)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(index(code - 40)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(index(code - 90 + 8)),
            100..=107 => style.bg = Some(index(code - 100 + 8)),
            38 | 48 => {
                // `38:5:n` / `38:2::r:g:b` keep their arguments in sub-parameters,
                // `38;5;n` / `38;2;r;g;b` in the following parameters.
                let color = if param.contains(':') {
                    extended_colon(&sub.collect::<Vec<_>>())
                } else {
                    extended_semicolon(&mut it)
                };
                if let Some(color) = color {
                    if code == 38 {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `code` is always below 16 here; the fallback is never hit.
fn index(code: u16) -> u8 {
    u8::try_from(code).unwrap_or(7)
}

fn extended_semicolon<'a>(it: &mut impl Iterator<Item = &'a str>) -> Option<u8> {
    match it.next()? {
        "5" => Some(from_256(it.next()?.parse().ok()?)),
        "2" => {
            let (r, g, b) = (it.next()?, it.next()?, it.next()?);
            Some(nearest(r.parse().ok()?, g.parse().ok()?, b.parse().ok()?))
        }
        _ => None,
    }
}

fn extended_colon(args: &[&str]) -> Option<u8> {
    match args {
        ["5", n] => Some(from_256(n.parse().ok()?)),
        // An optional color-space id may precede r:g:b.
        ["2", .., r, g, b] if args.len() <= 5 => {
            Some(nearest(r.parse().ok()?, g.parse().ok()?, b.parse().ok()?))
        }
        _ => None,
    }
}

/// Maps a 256-color palette index to the nearest of the 16 base colors.
fn from_256(n: u8) -> u8 {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match n {
        0..=15 => n,
        16..=231 => {
            let i = usize::from(n - 16);
            nearest(CUBE[i / 36], CUBE[i / 6 % 6], CUBE[i % 6])
        }
        _ => {
            let v = 8 + 10 * (n - 232);
            nearest(v, v, v)
        }
    }
}

/// Reference RGB values of the 16 base colors (xterm defaults). Only used to
/// pick the nearest index; the rendered colors come from the CSS theme.
const BASE16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn nearest(r: u8, g: u8, b: u8) -> u8 {
    let dist = |(br, bg, bb): (u8, u8, u8)| {
        let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).pow(2);
        d(r, br) + d(g, bg) + d(b, bb)
    };
    (0u8..)
        .zip(BASE16)
        .min_by_key(|&(_, c)| dist(c))
        .map_or(7, |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize, style: LogStyle) -> LogSpan {
        LogSpan {
            start,
            end,
            style,
            level: None,
        }
    }

    fn fg(c: u8) -> LogStyle {
        LogStyle {
            fg: Some(c),
            ..LogStyle::default()
        }
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\after"), "after");
        assert_eq!(strip_ansi("\x1b(Bok"), "ok");
        assert_eq!(strip_ansi("\x1b[2K\x1b[1Gcursor"), "cursor");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn plain_text_has_no_spans() {
        assert_eq!(parse("hello").spans, []);
        // Non-SGR CSI and OSC sequences are dropped without styling anything.
        assert_eq!(parse("\x1b[2J\x1b]0;t\x07x").spans, []);
    }

    #[test]
    fn basic_colors_and_reset() {
        let s = parse("\x1b[32mINFO\x1b[0m ok");
        assert_eq!(s.text, "INFO ok");
        assert_eq!(s.spans, [span(0, 4, fg(2))]);

        let s = parse("a\x1b[1;31mERROR\x1b[m b");
        let bold_red = LogStyle {
            bold: true,
            ..fg(1)
        };
        assert_eq!(s.spans, [span(1, 6, bold_red)]);
    }

    #[test]
    fn attributes_combine_and_turn_off() {
        let s = parse("\x1b[1mb\x1b[2md\x1b[22;3mi\x1b[4mu\x1b[23;24mn");
        assert_eq!(s.text, "bdiun");
        let st = |bold, dim, italic, underline| LogStyle {
            bold,
            dim,
            italic,
            underline,
            ..LogStyle::default()
        };
        assert_eq!(
            s.spans,
            [
                span(0, 1, st(true, false, false, false)),
                span(1, 2, st(true, true, false, false)),
                span(2, 3, st(false, false, true, false)),
                span(3, 4, st(false, false, true, true)),
            ]
        );
    }

    #[test]
    fn bright_and_background_colors() {
        let s = parse("\x1b[91;44mx\x1b[39mY\x1b[49mz\x1b[103mw\x1b[0m");
        assert_eq!(s.text, "xYzw");
        assert_eq!(
            s.spans,
            [
                span(
                    0,
                    1,
                    LogStyle {
                        fg: Some(9),
                        bg: Some(4),
                        ..LogStyle::default()
                    }
                ),
                span(
                    1,
                    2,
                    LogStyle {
                        bg: Some(4),
                        ..LogStyle::default()
                    }
                ),
                span(
                    3,
                    4,
                    LogStyle {
                        bg: Some(11),
                        ..LogStyle::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn style_runs_without_text_between_merge() {
        // red, then a blue that styles nothing, then red again: one span.
        let s = parse("\x1b[31ma\x1b[34m\x1b[31mb\x1b[0m");
        assert_eq!(s.spans, [span(0, 2, fg(1))]);
        // A trailing style without reset runs to the end of the line.
        assert_eq!(parse("x\x1b[33my").spans, [span(1, 2, fg(3))]);
    }

    #[test]
    fn extended_colors_map_to_nearest_base() {
        assert_eq!(parse("\x1b[38;5;196mx").spans, [span(0, 1, fg(9))]);
        assert_eq!(parse("\x1b[38;5;3mx").spans, [span(0, 1, fg(3))]);
        assert_eq!(parse("\x1b[38;5;244mx").spans, [span(0, 1, fg(8))]);
        assert_eq!(parse("\x1b[38;2;0;200;0mx").spans, [span(0, 1, fg(2))]);
        assert_eq!(parse("\x1b[38:2::255:165:0mx").spans, [span(0, 1, fg(3))]);
        assert_eq!(parse("\x1b[38:5:63mx").spans, [span(0, 1, fg(12))]);
        let bg = parse("\x1b[48;5;232;1mx").spans;
        assert_eq!(bg[0].style.bg, Some(0));
        assert!(bg[0].style.bold, "parameters after 48;5;n still apply");
        // Out of range or truncated: ignored.
        assert_eq!(parse("\x1b[38;5;300mx").spans, []);
        assert_eq!(parse("\x1b[38;2;1mx").spans, []);
    }

    #[test]
    fn private_sgr_is_ignored() {
        assert_eq!(parse("\x1b[?1mx").spans, []);
        assert_eq!(parse("\x1b[>4;2mx").spans, []);
    }

    #[test]
    fn offsets_are_utf16_code_units() {
        // é: 2 bytes, 1 UTF-16 unit. 🦀: 4 bytes, 2 UTF-16 units.
        let s = parse("é🦀\x1b[31mrød🦀\x1b[0m!");
        assert_eq!(s.text, "é🦀rød🦀!");
        assert_eq!(s.spans, [span(3, 8, fg(1))]);
        let utf16: Vec<u16> = s.text.encode_utf16().collect();
        assert_eq!(String::from_utf16_lossy(&utf16[3..8]), "rød🦀");
    }
}
