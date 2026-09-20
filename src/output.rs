//! Streaming grep-style rendering: prefixes, `-A/-B/-C` grouping with `--`,
//! `-l/-L/-c`, `--json`, `--stats`, and the `--dry-run` summary.
//!
//! Unlike the old whole-file renderer, this one sees one line at a time, in
//! strict order (the caller reorders by sequence number before handing
//! events here), and never has the whole file in memory: `-B`/`-A` context
//! is a small ring/countdown instead of a range computed over the full
//! line list.

use std::collections::VecDeque;

use serde::Serialize;

/// One item in the ordered event stream, after batching/judging.
#[derive(Debug, Clone)]
pub enum EmitItem {
    FileStart {
        label: String,
    },
    FileEnd,
    /// `verdict`: `Some(p)` means the line passed the match/invert
    /// decision (a real match: printed with `:`, counted, `--json`-able).
    /// `None` covers everything else -- a judged non-match, a blank/free
    /// line under normal polarity, or a line that got no answer at all
    /// (batch error / missing answer: never printed as a match, never
    /// counted, but still a real physical line eligible as `-A`/`-B`
    /// context, exactly like the old whole-file renderer treated it).
    /// `judged`: false for a blank/free line (never sent to jev), true for
    /// everything else -- used only for `--stats`' judged/free split.
    Line {
        lineno: u64,
        raw: String,
        verdict: Option<f64>,
        judged: bool,
    },
    Stopped {
        flag: &'static str,
        limit: usize,
        files_not_searched: Option<usize>,
    },
    /// A placeholder for a `seq` that was allocated but deliberately never
    /// became real content (the line that tripped `--max-lines`, or a line
    /// read after the bound tripped but before the source noticed). The
    /// reorder buffer requires every allocated `seq` to eventually arrive
    /// here exactly once -- a `seq` with nothing ever sent for it would
    /// leave a permanent gap and stall every later event forever.
    Skip,
}

pub struct FormatParams {
    pub show_filename: bool,
    pub line_number: bool,
    pub show_score: bool,
    pub before_ctx: usize,
    pub after_ctx: usize,
    pub json: bool,
    pub files_with_matches: bool,
    pub files_without_match: bool,
    pub count: bool,
}

#[derive(Serialize)]
struct JsonLine<'a> {
    file: &'a str,
    lineno: u64,
    line: &'a str,
    probability: f64,
}

/// Streaming printer/accumulator: consumes `EmitItem`s strictly in order
/// and prints incrementally (or, for `-l/-L/-c`, at each `FileEnd`).
pub struct Printer {
    fmt: FormatParams,

    current_label: String,
    file_gen: u64,
    file_has_match: bool,
    file_match_count: u64,

    before_ring: VecDeque<(u64, String)>,
    after_remaining: usize,
    last_printed: Option<(u64, u64)>, // (file_gen, lineno)
    first_group: bool,

    pub files_seen: u64,
    pub lines_seen: u64,
    pub judged: u64,
    pub free: u64,
    pub had_stop: bool,
}

impl Printer {
    pub fn new(fmt: FormatParams) -> Self {
        Printer {
            fmt,
            current_label: String::new(),
            file_gen: 0,
            file_has_match: false,
            file_match_count: 0,
            before_ring: VecDeque::new(),
            after_remaining: 0,
            last_printed: None,
            first_group: true,
            files_seen: 0,
            lines_seen: 0,
            judged: 0,
            free: 0,
            had_stop: false,
        }
    }

    /// Handle one event; returns 1 if it was a counted match, else 0 (so
    /// the caller can accumulate `total_matches`).
    pub fn handle(&mut self, item: EmitItem) -> u64 {
        match item {
            EmitItem::Skip => 0,
            EmitItem::FileStart { label } => {
                self.current_label = label;
                self.file_gen += 1;
                self.file_has_match = false;
                self.file_match_count = 0;
                self.before_ring.clear();
                self.after_remaining = 0;
                self.files_seen += 1;
                0
            }
            EmitItem::FileEnd => {
                if self.fmt.files_with_matches {
                    if self.file_has_match {
                        println!("{}", self.current_label);
                    }
                } else if self.fmt.files_without_match {
                    if !self.file_has_match {
                        println!("{}", self.current_label);
                    }
                } else if self.fmt.count {
                    if self.fmt.show_filename {
                        println!("{}:{}", self.current_label, self.file_match_count);
                    } else {
                        println!("{}", self.file_match_count);
                    }
                }
                0
            }
            EmitItem::Stopped {
                flag,
                limit,
                files_not_searched,
            } => {
                self.had_stop = true;
                let detail = match files_not_searched {
                    Some(n) => format!("{n} files not searched"),
                    None => "input not fully read".to_string(),
                };
                eprintln!(
                    "jevgrep: stopped at {flag} {limit} ({detail}); raise the limit or narrow with -g"
                );
                0
            }
            EmitItem::Line {
                lineno,
                raw,
                verdict,
                judged,
            } => {
                self.lines_seen += 1;
                if judged {
                    self.judged += 1;
                } else {
                    self.free += 1;
                }
                let is_match = verdict.is_some();
                if is_match {
                    self.file_has_match = true;
                    self.file_match_count += 1;
                }

                if self.fmt.json {
                    if let Some(p) = verdict {
                        let j = JsonLine {
                            file: &self.current_label,
                            lineno,
                            line: &raw,
                            probability: p,
                        };
                        if let Ok(s) = serde_json::to_string(&j) {
                            println!("{s}");
                        }
                    }
                    return is_match as u64;
                }
                if self.fmt.files_with_matches || self.fmt.files_without_match || self.fmt.count {
                    return is_match as u64;
                }

                // Normal grep-style rendering with -A/-B/-C.
                if is_match {
                    self.flush_before(&raw, lineno);
                    self.print_line(lineno, &raw, true, verdict);
                    self.after_remaining = self.fmt.after_ctx;
                    self.before_ring.clear();
                } else if self.after_remaining > 0 {
                    self.print_line(lineno, &raw, false, None);
                    self.after_remaining -= 1;
                } else if self.fmt.before_ctx > 0 {
                    self.before_ring.push_back((lineno, raw));
                    if self.before_ring.len() > self.fmt.before_ctx {
                        self.before_ring.pop_front();
                    }
                }
                is_match as u64
            }
        }
    }

    fn flush_before(&mut self, _current_raw: &str, _current_lineno: u64) {
        let ring = std::mem::take(&mut self.before_ring);
        for (lineno, raw) in ring {
            self.print_line(lineno, &raw, false, None);
        }
    }

    fn print_line(&mut self, lineno: u64, raw: &str, is_match: bool, verdict: Option<f64>) {
        let contiguous =
            matches!(self.last_printed, Some((g, ln)) if g == self.file_gen && ln + 1 == lineno);
        if !self.first_group && !contiguous {
            println!("--");
        }
        self.first_group = false;

        let sep = if is_match { ':' } else { '-' };
        let mut out = String::new();
        if is_match && self.fmt.show_score {
            let p = verdict.unwrap_or(0.0);
            out.push_str(&format!("p={p:.2} "));
        }
        if self.fmt.show_filename {
            out.push_str(&self.current_label);
            out.push(sep);
        }
        if self.fmt.line_number {
            out.push_str(&lineno.to_string());
            out.push(sep);
        }
        out.push_str(raw);
        println!("{out}");

        self.last_printed = Some((self.file_gen, lineno));
    }

    /// Nothing to flush at run end: every printed line is emitted as soon
    /// as its file-relative position is known (context is never held past
    /// the point it's needed), so this is a no-op kept for symmetry/
    /// future use.
    pub fn finish(&mut self) {}
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

pub fn print_dry_run_line(count: usize, path: &str) {
    eprintln!("{count}\t{path}");
}

pub fn print_dry_run_summary(files: usize, lines: usize, requests: usize) {
    eprintln!("{files} files, {lines} lines, ~{requests} requests");
}
