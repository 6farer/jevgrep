//! Shared test scaffolding: a dynamic wiremock responder that judges lines
//! by a simple rule (does the line contain a marker word?), and a
//! `jevgrep` command builder that never touches the real environment.
//!
//! The request body is jev's block-log shape: `state.log` is one text
//! block, one row per line -- `<id>| <text>` for a candidate (answered),
//! `  ~| <text>` for a context-only neighbour (no question, no answer).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub type ScoreFn = Box<dyn Fn(&str) -> f64 + Send + Sync>;

pub struct Scorer {
    pub calls: AtomicUsize,
    pub fail_first_n: usize,
    pub fail_status: u16,
    /// Requests whose raw JSON body contains this substring get delayed by
    /// `delay_ms` before responding (used to prove ordering survives an
    /// out-of-order batch completion).
    pub delay_marker: Option<String>,
    pub delay_ms: u64,
    pub score: ScoreFn,
    pub seen_bodies: Mutex<Vec<Value>>,
}

impl Scorer {
    pub fn matching(word: &str) -> Arc<Self> {
        let word = word.to_string();
        Arc::new(Scorer {
            calls: AtomicUsize::new(0),
            fail_first_n: 0,
            fail_status: 200,
            delay_marker: None,
            delay_ms: 0,
            score: Box::new(move |line: &str| {
                if line.to_uppercase().contains(&word.to_uppercase()) {
                    0.95
                } else {
                    0.05
                }
            }),
            seen_bodies: Mutex::new(Vec::new()),
        })
    }

    pub fn request_count(&self) -> usize {
        self.seen_bodies.lock().unwrap().len()
    }

    pub fn bodies(&self) -> Vec<Value> {
        self.seen_bodies.lock().unwrap().clone()
    }
}

/// Local newtype so we can implement the foreign `Respond` trait for a
/// shared `Arc<Scorer>` (the orphan rules forbid implementing a foreign
/// trait directly for `Arc<Scorer>`).
#[derive(Clone)]
pub struct SharedScorer(pub Arc<Scorer>);

impl Respond for SharedScorer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let this = &self.0;
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(json!({}));
        this.seen_bodies.lock().unwrap().push(body.clone());

        let n = this.calls.fetch_add(1, Ordering::SeqCst);
        if n < this.fail_first_n {
            return ResponseTemplate::new(this.fail_status);
        }

        let log = body["state"]["log"].as_str().unwrap_or("");
        let questions = body["questions"].as_object().cloned().unwrap_or_default();
        let mut answers = serde_json::Map::new();
        for row in log.split('\n') {
            if row.is_empty() {
                continue;
            }
            if let Some(text) = row.strip_prefix("  ~| ") {
                // Context-only row: no id, no question, no answer. Just
                // make sure nothing in the harness accidentally treats it
                // as askable.
                let _ = text;
                continue;
            }
            let (id, text) = row
                .split_once("| ")
                .expect("state.log row is not `<id>| <text>` or `  ~| <text>`");
            assert_eq!(
                questions[id]["type"], "noul",
                "expected a noul question for row {id}"
            );
            let p = (this.score)(text).clamp(0.0, 1.0);
            answers.insert(id.to_string(), json!({ "type": "noul", "noul": p }));
        }

        let resp = json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": { "input_tokens": answers.len() * 20, "output_tokens": answers.len() * 5 },
        });
        let mut template = ResponseTemplate::new(200).set_body_json(resp);
        if let Some(marker) = &this.delay_marker {
            let body_str = serde_json::to_string(&body).unwrap_or_default();
            if body_str.contains(marker.as_str()) {
                template = template.set_delay(std::time::Duration::from_millis(this.delay_ms));
            }
        }
        template
    }
}

pub async fn start(scorer: Arc<Scorer>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(SharedScorer(scorer))
        .mount(&server)
        .await;
    server
}

pub fn api_url(server: &MockServer) -> String {
    format!("{}/v1/systemone", server.uri())
}

/// A `jevgrep` command with a fake key, the mock server URL, and no access
/// to the real environment (in particular, never the crate root's real
/// `.env`/`TYPESAFE_API_KEY`). Callers must set `.current_dir(...)`.
pub fn base_cmd(server: &MockServer) -> Command {
    let mut cmd = Command::cargo_bin("jevgrep").unwrap();
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("TYPESAFE_API_KEY", "test");
    cmd.env("JEVGREP_API_URL", api_url(server));
    cmd
}

pub fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Parse a `state.log` block into its candidate rows (`(id, text)`, `id`
/// starting with `L`) and its context-only row texts (`  ~| <text>`, no
/// id), in the order they appear.
pub fn parse_log(log: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut context = Vec::new();
    for row in log.split('\n') {
        if row.is_empty() {
            continue;
        }
        if let Some(text) = row.strip_prefix("  ~| ") {
            context.push(text.to_string());
            continue;
        }
        if let Some((id, text)) = row.split_once("| ") {
            if id.starts_with('L') {
                candidates.push((id.to_string(), text.to_string()));
            }
        }
    }
    (candidates, context)
}
