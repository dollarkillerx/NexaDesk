//! Pure format conversions between canonical content and platform content.

use std::borrow::Cow;

use crate::backend::{ClipError, Result};

/// The newline convention of a target platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`, the canonical form and the macOS convention.
    Lf,
    /// `\r\n`, the Windows convention.
    CrLf,
}

/// Rewrites newlines for `target`. Idempotent; a lone `\r` is left untouched in
/// both directions, so conversions round-trip.
pub fn normalize_newlines(text: &str, target: LineEnding) -> Cow<'_, str> {
    match target {
        LineEnding::Lf if text.contains("\r\n") => Cow::Owned(text.replace("\r\n", "\n")),
        LineEnding::Lf => Cow::Borrowed(text),
        LineEnding::CrLf => to_crlf(text),
    }
}

fn to_crlf(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let needs_rewrite =
        (0..bytes.len()).any(|i| bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r'));
    if !needs_rewrite {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut previous = '\0';
    for ch in text.chars() {
        if ch == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(ch);
        previous = ch;
    }
    Cow::Owned(out)
}

/// Converts text to its canonical form, the form that is hashed and sent.
pub fn canonical_text(text: &str) -> Cow<'_, str> {
    normalize_newlines(text, LineEnding::Lf)
}

const START_COMMENT: &str = "<!--StartFragment-->";
const END_COMMENT: &str = "<!--EndFragment-->";
const DOC_PREFIX: &str = "<html><body>\r\n";
const DOC_SUFFIX: &str = "\r\n</body></html>";

/// Wraps canonical HTML for the Windows `HTML Format` clipboard type. The input
/// is treated as the fragment. All offsets are byte offsets into the UTF-8
/// result, with a fixed width so the header length does not depend on them.
pub fn cf_html_wrap(fragment: &str) -> String {
    let header = |start_html: usize, end_html: usize, start_frag: usize, end_frag: usize| {
        format!(
            "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n"
        )
    };
    let start_html = header(0, 0, 0, 0).len();
    let start_frag = start_html + DOC_PREFIX.len() + START_COMMENT.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + END_COMMENT.len() + DOC_SUFFIX.len();

    let mut out = header(start_html, end_html, start_frag, end_frag);
    out.reserve(end_html - start_html);
    out.push_str(DOC_PREFIX);
    out.push_str(START_COMMENT);
    out.push_str(fragment);
    out.push_str(END_COMMENT);
    out.push_str(DOC_SUFFIX);
    out
}

/// Extracts the fragment from a CF_HTML payload. The header's byte offsets are
/// authoritative, so fragment markers inside the content cannot confuse it.
pub fn cf_html_unwrap(payload: &[u8]) -> Result<&str> {
    let mut start = None;
    let mut end = None;
    let mut cursor = 0;

    while let Some(line_len) = payload[cursor..].iter().position(|&b| b == b'\n') {
        let line = trim_cr(&payload[cursor..cursor + line_len]);
        let Some(colon) = header_colon(line) else {
            break;
        };
        let value = &line[colon + 1..];
        match &line[..colon] {
            b"StartFragment" => start = parse_offset(value),
            b"EndFragment" => end = parse_offset(value),
            _ => {}
        }
        cursor += line_len + 1;
    }

    let start = start.ok_or(ClipError::BadHtml("missing StartFragment"))?;
    let end = end.ok_or(ClipError::BadHtml("missing EndFragment"))?;
    if start < cursor || start > end || end > payload.len() {
        return Err(ClipError::BadHtml("fragment offsets out of range"));
    }
    std::str::from_utf8(&payload[start..end])
        .map_err(|_| ClipError::BadHtml("fragment is not UTF-8"))
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Position of the colon if `line` looks like `Key:value`.
fn header_colon(line: &[u8]) -> Option<usize> {
    let colon = line.iter().position(|&b| b == b':')?;
    let is_key = colon > 0 && line[..colon].iter().all(u8::is_ascii_alphabetic);
    is_key.then_some(colon)
}

fn parse_offset(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_convert_both_ways_and_are_idempotent() {
        assert_eq!(
            normalize_newlines("a\nb\r\nc", LineEnding::CrLf),
            "a\r\nb\r\nc"
        );
        assert_eq!(normalize_newlines("a\r\nb\nc", LineEnding::Lf), "a\nb\nc");
        let once = normalize_newlines("a\nb", LineEnding::CrLf).into_owned();
        assert_eq!(normalize_newlines(&once, LineEnding::CrLf), once);
    }

    #[test]
    fn newlines_keep_lone_cr_and_handle_edges() {
        assert_eq!(normalize_newlines("a\rb", LineEnding::Lf), "a\rb");
        assert_eq!(normalize_newlines("a\rb", LineEnding::CrLf), "a\rb");
        assert_eq!(normalize_newlines("\nx", LineEnding::CrLf), "\r\nx");
        assert_eq!(normalize_newlines("", LineEnding::CrLf), "");
    }

    #[test]
    fn newline_conversion_borrows_when_nothing_changes() {
        assert!(matches!(
            normalize_newlines("abc", LineEnding::Lf),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_newlines("a\r\n", LineEnding::CrLf),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn canonical_text_hashes_the_same_across_platforms() {
        let windows = canonical_text("one\r\ntwo");
        let mac = canonical_text("one\ntwo");
        assert_eq!(windows, mac);
    }

    #[test]
    fn cf_html_round_trips_multibyte_content() {
        let fragment = "<b>日本語 é</b>";
        let wrapped = cf_html_wrap(fragment);
        assert_eq!(cf_html_unwrap(wrapped.as_bytes()).unwrap(), fragment);
    }

    #[test]
    fn cf_html_offsets_are_byte_offsets() {
        let wrapped = cf_html_wrap("é");
        let header_value = |key: &str| -> usize {
            let line = wrapped.lines().find(|l| l.starts_with(key)).unwrap();
            line[key.len() + 1..].parse().unwrap()
        };
        let start = header_value("StartFragment");
        let end = header_value("EndFragment");
        assert_eq!(&wrapped.as_bytes()[start..end], "é".as_bytes());
        assert_eq!(header_value("EndHTML"), wrapped.len());
        assert!(wrapped[header_value("StartHTML")..].starts_with("<html>"));
    }

    #[test]
    fn cf_html_ignores_markers_inside_the_fragment() {
        let fragment = "a<!--EndFragment-->b";
        let wrapped = cf_html_wrap(fragment);
        assert_eq!(cf_html_unwrap(wrapped.as_bytes()).unwrap(), fragment);
    }

    #[test]
    fn cf_html_accepts_lf_headers_and_trailing_nul() {
        let body = "<!--StartFragment-->x<!--EndFragment-->\0";
        let header_len = "StartFragment:0000000000\nEndFragment:0000000000\n".len();
        let start = header_len + START_COMMENT.len();
        let payload = format!(
            "StartFragment:{start:010}\nEndFragment:{:010}\n{body}",
            start + 1
        );
        assert_eq!(cf_html_unwrap(payload.as_bytes()).unwrap(), "x");
    }

    #[test]
    fn cf_html_rejects_malformed_headers() {
        assert!(cf_html_unwrap(b"<html></html>").is_err());
        assert!(cf_html_unwrap(b"StartFragment:5\r\nEndFragment:3\r\n").is_err());
        assert!(cf_html_unwrap(b"StartFragment:40\r\nEndFragment:9999\r\n<a>").is_err());
        assert!(cf_html_unwrap(b"StartFragment:0\r\nEndFragment:1\r\n<a>").is_err());
    }
}
