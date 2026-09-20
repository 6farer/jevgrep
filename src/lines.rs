//! The "plan pass": walk the file list (already enumerated by `walk.rs`),
//! read each file, and classify every line as `Free` (blank, no API call
//! needed), `Dup` (identical line+context already queued this run, will
//! resolve from the same verdict), or `Ask` (needs an API call). This is
//! also where `--max-files`/`--max-lines` are enforced, and it is the only
//! place that touches file content, so stopping here is what keeps a
//! bound-hit run from reading more than it needs to.

use std::collections::{HashSet, VecDeque};
use std::io::Read;

use anyhow::{anyhow, Result};

use crate::walk::{looks_binary, FileArg};

const CACHE_CAP: usize = 10_000;

#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    pub probability: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub enum RecordKind {
    Free(Verdict),
    Dup {
        key: String,
    },
    Ask {
        key: String,
        before: Vec<String>,
        line: String,
        after: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct LineRecord {
    pub file_idx: usize,
    pub lineno: u64,
    /// The full line as read (minus the trailing `\n`), never truncated.
    /// Printed verbatim.
    pub raw: String,
    pub kind: RecordKind,
}

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncKind {
    MaxFiles,
    MaxLines,
}

pub struct PlanResult {
    pub files: Vec<FileMeta>,
    pub records: Vec<LineRecord>,
    pub free_count: u64,
    /// `(which bound, exact number of files not searched)`.
    pub truncated: Option<(TruncKind, usize)>,
}

/// Small insertion-ordered set capped at `CACHE_CAP`, dropping the oldest
/// entry once full. This is the "verdict cache" key-presence half; the
/// value half (actual verdicts) is filled in later by the judge pass.
struct SeenKeys {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenKeys {
    fn new() -> Self {
        SeenKeys {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.set.contains(key)
    }

    fn insert(&mut self, key: String) {
        if self.set.contains(&key) {
            return;
        }
        if self.order.len() >= CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
    }
}

fn strip_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

fn truncate_bytes(s: &str, max_len: usize) -> String {
    if max_len == 0 || s.len() <= max_len {
        return s.to_string();
    }
    // Cut at a byte boundary; lossy-decoding a mid-character cut is exactly
    // what `from_utf8_lossy` is for.
    String::from_utf8_lossy(&s.as_bytes()[..max_len]).into_owned()
}

fn judge_text(raw: &str, max_len: usize) -> String {
    truncate_bytes(strip_cr(raw), max_len)
}

fn cache_key(before: &[String], line: &str, after: &[String]) -> String {
    // serde_json gives us an unambiguous, escaped encoding for free.
    serde_json::to_string(&(before, line, after)).unwrap_or_default()
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

pub fn read_file_arg(arg: &FileArg) -> Result<Vec<u8>> {
    match arg {
        FileArg::Stdin => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| anyhow!("stdin: {e}"))?;
            Ok(buf)
        }
        FileArg::Path { path, .. } => {
            std::fs::read(path).map_err(|e| anyhow!("{}: {}", path.display(), e))
        }
    }
}

pub struct PlanParams {
    pub judge_context: usize,
    pub max_files: usize,
    pub max_lines: usize,
    pub max_line_length: usize,
    pub no_cache: bool,
}

/// Run the plan pass. Returns an error only for an explicit CLI file
/// argument that could not be opened (the spec's "unreadable path given on
/// the command line" case); everything discovered during a directory walk
/// that turns out to be unreadable is a warning printed to stderr instead.
pub fn plan(file_args: Vec<FileArg>, params: &PlanParams) -> Result<PlanResult> {
    let mut files = Vec::new();
    let mut records = Vec::new();
    let mut files_opened: usize = 0;
    let mut free_count: u64 = 0;
    let mut queued_count: u64 = 0;
    let mut seen = SeenKeys::new();

    let mut truncated: Option<(TruncKind, usize)> = None;

    let total = file_args.len();
    'outer: for (idx, arg) in file_args.into_iter().enumerate() {
        if params.max_files != 0 && files_opened >= params.max_files {
            truncated = Some((TruncKind::MaxFiles, total - idx));
            break;
        }

        let explicit = matches!(arg, FileArg::Path { explicit: true, .. });
        let bytes = match read_file_arg(&arg) {
            Ok(b) => b,
            Err(e) => {
                if explicit {
                    return Err(e);
                }
                eprintln!("jevgrep: {e}");
                continue;
            }
        };
        files_opened += 1;

        if looks_binary(&bytes) {
            continue;
        }

        let file_idx = files.len();
        files.push(FileMeta { label: arg.label() });

        let file_lines = split_lines(&bytes);
        let n = file_lines.len();
        let ctx = params.judge_context;

        for i in 0..n {
            let lineno = (i + 1) as u64;
            let raw = file_lines[i].clone();

            let before_start = i.saturating_sub(ctx);
            let after_end = (i + 1 + ctx).min(n);
            let judge_before: Vec<String> = file_lines[before_start..i]
                .iter()
                .map(|l| judge_text(l, params.max_line_length))
                .collect();
            let judge_after: Vec<String> = file_lines[i + 1..after_end]
                .iter()
                .map(|l| judge_text(l, params.max_line_length))
                .collect();
            let judge_line = judge_text(&raw, params.max_line_length);

            if judge_line.trim().is_empty() {
                records.push(LineRecord {
                    file_idx,
                    lineno,
                    raw,
                    kind: RecordKind::Free(Verdict {
                        probability: 0.0,
                        confidence: 1.0,
                    }),
                });
                free_count += 1;
                continue;
            }

            // Under --no-cache every occurrence gets its own key (prefixed
            // with its record index), so two identical lines never share a
            // verdict — not even by accident when one batch fails.
            let key = if params.no_cache {
                format!("#{}:{}", records.len(), judge_line)
            } else {
                cache_key(&judge_before, &judge_line, &judge_after)
            };
            let is_dup = !params.no_cache && seen.contains(&key);

            if is_dup {
                records.push(LineRecord {
                    file_idx,
                    lineno,
                    raw,
                    kind: RecordKind::Dup { key },
                });
                continue;
            }

            if params.max_lines != 0 && queued_count >= params.max_lines as u64 {
                let remaining_after_this_file = total - idx - 1;
                truncated = Some((TruncKind::MaxLines, remaining_after_this_file));
                break 'outer;
            }

            if !params.no_cache {
                seen.insert(key.clone());
            }
            queued_count += 1;
            records.push(LineRecord {
                file_idx,
                lineno,
                raw,
                kind: RecordKind::Ask {
                    key,
                    before: judge_before,
                    line: judge_line,
                    after: judge_after,
                },
            });
        }
    }

    Ok(PlanResult {
        files,
        records,
        free_count,
        truncated,
    })
}
