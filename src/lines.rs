//! Small text helpers shared by the source and the batcher: splitting raw
//! bytes into lines, and preparing a line's *judged* text (as opposed to the
//! raw text, which is always printed verbatim, untouched).

/// Strip a trailing `\r` from CRLF input for judging; the raw line (with the
/// `\r` still attached) is what gets printed.
pub fn strip_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

pub fn truncate_bytes(s: &str, max_len: usize) -> String {
    if max_len == 0 || s.len() <= max_len {
        return s.to_string();
    }
    // Cut at a byte boundary; lossy-decoding a mid-character cut is exactly
    // what `from_utf8_lossy` is for.
    String::from_utf8_lossy(&s.as_bytes()[..max_len]).into_owned()
}

pub fn judge_text(raw: &str, max_len: usize) -> String {
    truncate_bytes(strip_cr(raw), max_len)
}

/// Split raw file bytes into lines, stripping only the trailing `\n` (a
/// trailing `\r` from CRLF input is kept, matching "print the line as-is").
/// Non-UTF-8 bytes are lossily decoded per line.
pub fn split_lines(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(String::from_utf8_lossy(&bytes[start..]).into_owned());
    }
    out
}
