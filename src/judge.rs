//! TypeSafe "System One" client: request batching shape, retry, the
//! identical-(line+context) verdict cache, and running usage totals.
//!
//! The batch goes in as one text block ("log"), one row per line, `L00| `
//! for a candidate row (the ones jev actually answers a yes/no question
//! about) and `  ~| ` for a context-only row. Identical candidates (same
//! `cache_key`, computed by the caller from the line's judged neighbourhood)
//! are judged once per run: within a single `judge_rows` call (an in-batch
//! duplicate collapses to one question) and across calls (once judged, a
//! repeat is served from the cache with no API call at all).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const CACHE_CAP: usize = 10_000;

/// jev's P(line matches the query).
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    pub probability: f64,
}

/// The outcome for one line within an otherwise-successful API response.
#[derive(Debug, Clone)]
pub enum LineOutcome {
    Verdict(Verdict),
    /// The response didn't include an answer for this line's question id.
    MissingAnswer,
}

/// A whole-request failure: transport error, non-2xx after retries, or a
/// fatal auth failure. Every candidate row in the batch gets this outcome.
/// `fatal` means retrying other batches cannot help (a rejected API key) --
/// the whole run should stop even without `--strict`.
#[derive(Debug, Clone)]
pub struct BatchError {
    pub msg: String,
    pub fatal: bool,
}

impl BatchError {
    fn new(msg: impl Into<String>) -> Self {
        BatchError {
            msg: msg.into(),
            fatal: false,
        }
    }

    fn fatal(msg: impl Into<String>) -> Self {
        BatchError {
            msg: msg.into(),
            fatal: true,
        }
    }
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

/// The API's per-request token ceiling was hit. Handled by splitting the
/// batch, so it never reaches the caller unless a single line is too big.
#[derive(Debug)]
enum AskError {
    TooLarge,
    Other(BatchError),
}

/// Longest line text sent to the API, in bytes. A line over this is judged
/// on its prefix: real logs carry the occasional 16KB stack dump, and the
/// first few KB is what decides whether it matches.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

fn clip(text: &str) -> &str {
    if text.len() <= MAX_LINE_BYTES {
        return text;
    }
    let mut end = MAX_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Usage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One row of the batch as the caller (the batcher, in `pipeline.rs`) sees
/// it: either a candidate to be judged, or a context-only neighbour.
pub struct Row {
    pub text: String,
    pub candidate: bool,
    /// Identical-neighbourhood dedup/cache key for a candidate row. Ignored
    /// for context rows. `None` on a candidate disables dedup/caching for
    /// that row (treated as always-unique).
    pub cache_key: Option<String>,
}

/// One row as it will actually be sent: a context row (no question), or a
/// candidate row that still needs asking (already deduped: only the first
/// occurrence of a repeated key becomes an `Entry::Candidate`).
#[derive(Clone)]
enum Entry {
    Context(String),
    Candidate { text: String, key: String },
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    state: StateBody<'a>,
    questions: HashMap<String, Question>,
}

#[derive(Serialize)]
struct StateBody<'a> {
    query: &'a str,
    log: String,
}

/// A `noul` is jev's bare yes/no: it returns P(yes) with no option list to
/// pay for, which is exactly what "does this line match?" needs.
#[derive(Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
}

#[derive(Deserialize)]
struct ResponseBody {
    #[allow(dead_code)]
    #[serde(default)]
    model: String,
    answers: HashMap<String, Answer>,
    #[serde(default)]
    usage: UsageBody,
}

#[derive(Deserialize)]
struct Answer {
    #[allow(dead_code)]
    #[serde(rename = "type", default)]
    kind: String,
    /// P(yes). A noul answer carries nothing else worth keeping.
    noul: f64,
}

#[derive(Deserialize, Default)]
struct UsageBody {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

struct Cache {
    map: HashMap<String, Verdict>,
    order: VecDeque<String>,
}

impl Cache {
    fn new() -> Self {
        Cache {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &str) -> Option<Verdict> {
        self.map.get(key).copied()
    }

    fn insert(&mut self, key: String, verdict: Verdict) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.order.len() >= CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, verdict);
    }
}

pub struct Judge {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    model: String,
    timeout: Duration,
    no_cache: bool,
    cache: Mutex<Cache>,
    usage: Mutex<Usage>,
}

impl Judge {
    pub fn new(
        api_url: String,
        api_key: String,
        model: String,
        timeout_ms: u64,
        no_cache: bool,
    ) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Judge {
            client,
            api_url,
            api_key,
            model,
            timeout: Duration::from_millis(timeout_ms),
            no_cache,
            cache: Mutex::new(Cache::new()),
            usage: Mutex::new(Usage::default()),
        }
    }

    pub fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }

    /// Judge a batch of rows against `query`. Returns one outcome per
    /// *candidate* row index in `rows` (context rows never appear in the
    /// result map). Candidates sharing an identical `cache_key` -- within
    /// this call, or already cached from an earlier one -- are only ever
    /// sent to the API once; every original index gets a clone of that
    /// key's outcome. Only fails a given index (as `Err(BatchError)`) when
    /// the underlying request for its key's group actually failed.
    pub async fn judge_rows(
        &self,
        query: &str,
        rows: &[Row],
    ) -> HashMap<usize, Result<LineOutcome, BatchError>> {
        let mut results = HashMap::new();

        // Per-row dedup key (candidates only; `None` for context rows).
        let keys: Vec<Option<String>> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if !r.candidate {
                    return None;
                }
                Some(
                    r.cache_key
                        .clone()
                        .unwrap_or_else(|| format!("__nokey__{i}")),
                )
            })
            .collect();

        // Group candidate row indices by key, first-seen order.
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, key) in keys.iter().enumerate() {
            let Some(key) = key else { continue };
            if !groups.contains_key(key) {
                order.push(key.clone());
            }
            groups.entry(key.clone()).or_default().push(idx);
        }

        if order.is_empty() {
            return results; // no candidates in this batch at all
        }

        let to_ask: Vec<String> = if self.no_cache {
            order
        } else {
            let cache = self.cache.lock().unwrap();
            let mut to_ask = Vec::with_capacity(order.len());
            for key in order {
                match cache.get(&key) {
                    Some(v) => {
                        for idx in &groups[&key] {
                            results.insert(*idx, Ok(LineOutcome::Verdict(v)));
                        }
                    }
                    None => to_ask.push(key),
                }
            }
            to_ask
        };

        if to_ask.is_empty() {
            return results;
        }
        let to_ask_set: HashSet<&str> = to_ask.iter().map(String::as_str).collect();

        // Build the ordered entries actually sent: every context row, plus
        // exactly one representative per to-ask candidate group (its first
        // occurrence in `rows`). Cache hits and later duplicates are
        // omitted entirely -- their outcome is already known or will be
        // copied from the representative.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut entries: Vec<Entry> = Vec::with_capacity(rows.len());
        for (idx, row) in rows.iter().enumerate() {
            match &keys[idx] {
                None => entries.push(Entry::Context(row.text.clone())),
                Some(key) => {
                    if !to_ask_set.contains(key.as_str()) {
                        continue;
                    }
                    if !seen.insert(key.as_str()) {
                        continue;
                    }
                    entries.push(Entry::Candidate {
                        text: row.text.clone(),
                        key: key.clone(),
                    });
                }
            }
        }

        match self.ask_split(query, &entries).await {
            Ok(answers) => {
                if !self.no_cache {
                    let mut cache = self.cache.lock().unwrap();
                    for key in &to_ask {
                        if let Some(LineOutcome::Verdict(v)) = answers.get(key) {
                            cache.insert(key.clone(), *v);
                        }
                    }
                }
                for key in &to_ask {
                    let outcome = answers
                        .get(key)
                        .cloned()
                        .unwrap_or(LineOutcome::MissingAnswer);
                    for idx in &groups[key] {
                        results.insert(*idx, Ok(outcome.clone()));
                    }
                }
            }
            Err(e) => {
                for key in &to_ask {
                    for idx in &groups[key] {
                        results.insert(*idx, Err(e.clone()));
                    }
                }
            }
        }

        results
    }

    /// `ask`, but when the API says the request is over its token ceiling,
    /// halve the entries and try each half, down to a single entry. A
    /// split keeps each half's context rows (they're just whichever entries
    /// land in that half). A single candidate is sent clipped to
    /// `MAX_LINE_BYTES`; if even that is too big it gets a per-key error
    /// rather than failing its batch-mates.
    async fn ask_split(
        &self,
        query: &str,
        entries: &[Entry],
    ) -> Result<HashMap<String, LineOutcome>, BatchError> {
        if !entries.iter().any(|e| matches!(e, Entry::Candidate { .. })) {
            return Ok(HashMap::new());
        }
        match self.ask(query, entries).await {
            Ok(a) => return Ok(a),
            Err(AskError::Other(e)) => return Err(e),
            Err(AskError::TooLarge) => {}
        }
        if entries.len() == 1 {
            return Err(BatchError::new(format!(
                "line too large for the TypeSafe API even after clipping to {MAX_LINE_BYTES} bytes"
            )));
        }
        let mid = entries.len() / 2;
        let left = Box::pin(self.ask_split(query, &entries[..mid])).await?;
        let right = Box::pin(self.ask_split(query, &entries[mid..])).await?;
        let mut out = left;
        out.extend(right);
        Ok(out)
    }

    /// Send one API request covering `entries`. Retries on 429/529 and
    /// transport errors, 3 attempts total, backoff `250ms * 2^(n-1)`.
    async fn ask(
        &self,
        query: &str,
        entries: &[Entry],
    ) -> Result<HashMap<String, LineOutcome>, AskError> {
        let mut rows: Vec<String> = Vec::with_capacity(entries.len());
        let mut questions = HashMap::new();
        let mut id_of_key: Vec<(String, String)> = Vec::new();
        let mut n = 0usize;

        for entry in entries {
            match entry {
                Entry::Context(text) => {
                    rows.push(format!("  ~| {}", clip(text)));
                }
                Entry::Candidate { text, key } => {
                    let id = format!("L{n:02}");
                    n += 1;
                    rows.push(format!("{id}| {}", clip(text)));
                    questions.insert(
                        id.clone(),
                        Question {
                            kind: "noul",
                            instructions: format!(
                                "Does line {id} match the query? Neighbouring lines are \
                                 context only; judge {id} itself."
                            ),
                        },
                    );
                    id_of_key.push((key.clone(), id));
                }
            }
        }

        let body = RequestBody {
            model: &self.model,
            state: StateBody {
                query,
                log: rows.join("\n"),
            },
            questions,
        };

        let mut last_err: Option<BatchError> = None;
        for attempt in 1..=3u32 {
            let send_result = self
                .client
                .post(&self.api_url)
                .bearer_auth(&self.api_key)
                .timeout(self.timeout)
                .json(&body)
                .send()
                .await;

            match send_result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let parsed: ResponseBody = match resp.json().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(AskError::Other(BatchError::new(format!(
                                    "invalid response from TypeSafe API: {e}"
                                ))));
                            }
                        };
                        {
                            let mut usage = self.usage.lock().unwrap();
                            usage.requests += 1;
                            usage.input_tokens += parsed.usage.input_tokens;
                            usage.output_tokens += parsed.usage.output_tokens;
                        }
                        let mut out = HashMap::with_capacity(id_of_key.len());
                        for (key, id) in &id_of_key {
                            let outcome = match parsed.answers.get(id) {
                                Some(ans) => LineOutcome::Verdict(Verdict {
                                    probability: ans.noul,
                                }),
                                None => LineOutcome::MissingAnswer,
                            };
                            out.insert(key.clone(), outcome);
                        }
                        return Ok(out);
                    } else if status.as_u16() == 401 {
                        return Err(AskError::Other(BatchError::fatal(format!(
                            "TypeSafe API rejected the key (HTTP {})",
                            status.as_u16()
                        ))));
                    } else if status.as_u16() == 429 || status.as_u16() == 529 {
                        last_err = Some(BatchError::new(format!(
                            "TypeSafe API error (HTTP {})",
                            status.as_u16()
                        )));
                    } else {
                        // The body is the useful part of a 4xx: e.g.
                        // {"detail":{"error_type":"max_tokens_exceeded"}}.
                        let body = resp.text().await.unwrap_or_default();
                        if body.contains("max_tokens_exceeded") {
                            return Err(AskError::TooLarge);
                        }
                        let detail = api_error_detail(&body);
                        return Err(AskError::Other(BatchError::new(format!(
                            "TypeSafe API error (HTTP {}){}",
                            status.as_u16(),
                            detail
                        ))));
                    }
                }
                Err(e) => {
                    last_err = Some(BatchError::new(format!(
                        "request to TypeSafe API failed: {e}"
                    )));
                }
            }

            if attempt < 3 {
                let backoff_ms = 250u64 * (1u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
        }

        Err(AskError::Other(last_err.unwrap_or_else(|| {
            BatchError::new("TypeSafe API request failed")
        })))
    }
}

/// A short, single-line rendering of an API error body for stderr: the
/// `detail` field when there is one, else the first 200 chars of the body.
fn api_error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let detail = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| v.get("detail").cloned())
        .map(|d| match d {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_else(|| trimmed.chars().take(200).collect());
    format!(": {}", detail.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_char_boundaries() {
        let s = "é".repeat(MAX_LINE_BYTES); // 2 bytes each; boundary falls mid-char
        let c = clip(&s);
        assert!(c.len() <= MAX_LINE_BYTES);
        assert!(s.is_char_boundary(c.len()));
        assert_eq!(clip("short"), "short");
    }

    #[test]
    fn error_detail_prefers_detail_field() {
        assert_eq!(
            api_error_detail(r#"{"detail":{"error_type":"max_tokens_exceeded"}}"#),
            r#": {"error_type":"max_tokens_exceeded"}"#
        );
        assert_eq!(api_error_detail("plain text"), ": plain text");
        assert_eq!(api_error_detail("  "), "");
    }
}
