//! grep-style rendering: prefixes, `-A/-B/-C` grouping with `--`, `-l/-L/-c`,
//! `--json`, `--stats`, and the `--dry-run` summary.

use serde::Serialize;

/// A file's fully-resolved lines, ready to format.
pub struct FileLines<'a> {
    pub file_idx: usize,
    pub label: &'a str,
    /// Index 0 is line 1.
    pub raw_by_lineno: &'a [String],
    /// Lines that passed the match decision (threshold, possibly inverted),
    /// sorted ascending by line number.
    pub matched: &'a [(u64, f64)],
}

pub struct FormatParams {
    pub show_filename: bool,
    pub line_number: bool,
    pub show_score: bool,
    pub before_ctx: usize,
    pub after_ctx: usize,
}

/// Tracks output-ordering state across files so `--` separators are printed
/// correctly even at file boundaries.
#[derive(Default)]
pub struct Printer {
    last: Option<(usize, u64)>,
    first_group: bool,
}

impl Printer {
    pub fn new() -> Self {
        Printer {
            last: None,
            first_group: true,
        }
    }

    /// Print every matched line in this file, with `-A/-B/-C` context,
    /// merging overlapping/adjacent windows and separating non-contiguous
    /// groups with `--`.
    pub fn print_file(&mut self, f: &FileLines, p: &FormatParams) {
        if f.matched.is_empty() {
            return;
        }
        let n = f.raw_by_lineno.len() as u64;
        let matched_set: std::collections::HashSet<u64> =
            f.matched.iter().map(|(ln, _)| *ln).collect();
        let prob_by_line: std::collections::HashMap<u64, f64> = f.matched.iter().cloned().collect();

        // Expand each match into a context range, then merge.
        let mut ranges: Vec<(u64, u64)> = f
            .matched
            .iter()
            .map(|(ln, _)| {
                let start = ln.saturating_sub(p.before_ctx as u64).max(1);
                let end = (ln + p.after_ctx as u64).min(n);
                (start, end)
            })
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut() {
                if start <= last.1 + 1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }

        for (start, end) in merged {
            let contiguous =
                matches!(self.last, Some((fi, ln)) if fi == f.file_idx && ln + 1 == start);
            if !self.first_group && !contiguous {
                println!("--");
            }
            self.first_group = false;

            for lineno in start..=end {
                let text = &f.raw_by_lineno[(lineno - 1) as usize];
                let is_match = matched_set.contains(&lineno);
                let sep = if is_match { ':' } else { '-' };

                let mut out = String::new();
                if is_match && p.show_score {
                    let prob = prob_by_line.get(&lineno).copied().unwrap_or(0.0);
                    out.push_str(&format!("p={prob:.2} "));
                }
                if p.show_filename {
                    out.push_str(f.label);
                    out.push(sep);
                }
                if p.line_number {
                    out.push_str(&lineno.to_string());
                    out.push(sep);
                }
                out.push_str(text);
                println!("{out}");
            }
            self.last = Some((f.file_idx, end));
        }
    }
}

pub fn file_has_match(f: &FileLines) -> bool {
    !f.matched.is_empty()
}

pub fn print_files_with_matches(f: &FileLines) {
    if file_has_match(f) {
        println!("{}", f.label);
    }
}

pub fn print_files_without_match(f: &FileLines) {
    if !file_has_match(f) {
        println!("{}", f.label);
    }
}

pub fn print_count(f: &FileLines, show_filename: bool) {
    let count = f.matched.len();
    if show_filename {
        println!("{}:{}", f.label, count);
    } else {
        println!("{count}");
    }
}

#[derive(Serialize)]
struct JsonLine<'a> {
    file: &'a str,
    lineno: u64,
    line: &'a str,
    probability: f64,
    confidence: f64,
}

pub fn print_json(f: &FileLines, confidence_by_line: &std::collections::HashMap<u64, f64>) {
    for &(lineno, probability) in f.matched {
        let text = &f.raw_by_lineno[(lineno - 1) as usize];
        let confidence = confidence_by_line.get(&lineno).copied().unwrap_or(0.0);
        let j = JsonLine {
            file: f.label,
            lineno,
            line: text,
            probability,
            confidence,
        };
        if let Ok(s) = serde_json::to_string(&j) {
            println!("{s}");
        }
    }
}

/// Insert thousands separators, e.g. 61340 -> "61,340".
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn print_stats(
    files: u64,
    lines: u64,
    judged: u64,
    free: u64,
    matches: u64,
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    elapsed_secs: f64,
) {
    eprintln!(
        "jevgrep: {} files, {} lines ({} judged, {} free), {} matches, {} requests, {} in / {} out tokens, {:.1}s",
        grouped(files),
        grouped(lines),
        grouped(judged),
        grouped(free),
        grouped(matches),
        grouped(requests),
        grouped(input_tokens),
        grouped(output_tokens),
        elapsed_secs,
    );
}

pub fn print_stop_message(flag: &str, limit: usize, remaining: usize) {
    eprintln!(
        "jevgrep: stopped at {flag} {limit} ({remaining} files not searched); raise the limit or narrow with -g"
    );
}

pub fn print_dry_run_line(count: usize, path: &str) {
    eprintln!("{count}\t{path}");
}

pub fn print_dry_run_summary(files: usize, lines: usize, requests: usize) {
    eprintln!("{files} files, {lines} lines, ~{requests} requests");
}
