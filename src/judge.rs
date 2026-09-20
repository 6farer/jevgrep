//! TypeSafe "System One" client: request shape, retry policy, and running
//! usage totals. The identical-line verdict cache lives in `lines.rs` (it
//! has to be resolved at plan time, before any batching happens); this
//! module only ever sees lines that need a real answer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lines::Verdict;

/// The outcome for one line within an otherwise-successful API response.
#[derive(Debug, Clone)]
pub enum LineOutcome {
    Verdict(Verdict),
    /// The response didn't include an answer for this line's question id.
    MissingAnswer,
}

/// A whole-request failure: transport error, or a non-2xx response after
/// retries are exhausted (or not attempted, for non-retryable statuses).
#[derive(Debug, Clone)]
pub struct BatchError {
    pub msg: String,
    /// The whole run should stop: retrying other batches cannot help
    /// (e.g. a rejected API key).
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

#[derive(Default, Debug, Clone, Copy)]
pub struct Usage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One line to ask jev about.
pub struct AskItem<'a> {
    pub key: &'a str,
    pub before: &'a [String],
    pub line: &'a str,
    pub after: &'a [String],
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
    lines: HashMap<String, LineEntry<'a>>,
}

#[derive(Serialize)]
struct LineEntry<'a> {
    before: &'a [String],
    line: &'a str,
    after: &'a [String],
}

#[derive(Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
    criteria: Criteria,
}

#[derive(Serialize)]
struct Criteria {
    yes: &'static str,
    no: &'static str,
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
    #[allow(dead_code)]
    #[serde(default)]
    choice: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    probabilities: HashMap<String, f64>,
}

#[derive(Deserialize, Default)]
struct UsageBody {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

pub struct Judge {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    model: String,
    timeout: Duration,
    usage: Mutex<Usage>,
}

impl Judge {
    pub fn new(api_url: String, api_key: String, model: String, timeout_ms: u64) -> Self {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        Judge {
            client,
            api_url,
            api_key,
            model,
            timeout: Duration::from_millis(timeout_ms),
            usage: Mutex::new(Usage::default()),
        }
    }

    pub fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }

    /// Ask jev about every line in `batch`. Returns one outcome per input
    /// key on success (even a per-line `MissingAnswer`); returns a single
    /// `BatchError` when the request itself failed.
    pub async fn judge_batch(
        &self,
        query: &str,
        batch: &[AskItem<'_>],
    ) -> Result<HashMap<String, LineOutcome>, BatchError> {
        let mut lines_map = HashMap::with_capacity(batch.len());
        let mut questions = HashMap::with_capacity(batch.len());
        let mut id_of: Vec<String> = Vec::with_capacity(batch.len());

        for (i, item) in batch.iter().enumerate() {
            let id = format!("l{i}");
            lines_map.insert(
                id.clone(),
                LineEntry {
                    before: item.before,
                    line: item.line,
                    after: item.after,
                },
            );
            questions.insert(
                id.clone(),
                Question {
                    kind: "choice",
                    instructions: format!(
                        "Does the line at state.lines.{id}.line match the query in state.query? \
                         Use state.lines.{id}.before and .after only as surrounding context; \
                         judge the line itself."
                    ),
                    criteria: Criteria {
                        yes: "matches",
                        no: "does not match",
                    },
                },
            );
            id_of.push(id);
        }

        let body = RequestBody {
            model: &self.model,
            state: StateBody {
                query,
                lines: lines_map,
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
                                return Err(BatchError::new(format!(
                                    "invalid response from TypeSafe API: {e}"
                                )));
                            }
                        };
                        {
                            let mut usage = self.usage.lock().unwrap();
                            usage.requests += 1;
                            usage.input_tokens += parsed.usage.input_tokens;
                            usage.output_tokens += parsed.usage.output_tokens;
                        }
                        let mut out = HashMap::with_capacity(batch.len());
                        for (i, item) in batch.iter().enumerate() {
                            let id = &id_of[i];
                            let outcome = match parsed.answers.get(id) {
                                Some(ans) => {
                                    let p = *ans.probabilities.get("yes").unwrap_or(&0.0);
                                    LineOutcome::Verdict(Verdict {
                                        probability: p,
                                        confidence: ans.confidence,
                                    })
                                }
                                None => LineOutcome::MissingAnswer,
                            };
                            out.insert(item.key.to_string(), outcome);
                        }
                        return Ok(out);
                    }

                    let code = status.as_u16();
                    if code == 401 {
                        return Err(BatchError::fatal(format!(
                            "TypeSafe API rejected the key (HTTP {code})"
                        )));
                    }

                    let body_text = resp.text().await.unwrap_or_default();
                    let snippet: String = body_text.chars().take(200).collect();
                    let msg = if snippet.is_empty() {
                        format!("TypeSafe API error (HTTP {code})")
                    } else {
                        format!("TypeSafe API error (HTTP {code}): {snippet}")
                    };

                    let retryable = code == 429 || code == 529;
                    if !retryable {
                        return Err(BatchError::new(msg));
                    }
                    last_err = Some(BatchError::new(msg));
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

        Err(last_err.unwrap_or_else(|| BatchError::new("TypeSafe API request failed")))
    }
}
