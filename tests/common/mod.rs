//! Shared test scaffolding: a dynamic wiremock responder that judges lines
//! by a simple rule (does the line contain a marker word?), and a
//! `jevgrep` command builder that never touches the real environment.
//!
//! The request body is jev's block-log shape: `state.log` is one text
//! block, one row per line -- `<id>| <text>` for a candidate (answered),
//! `  ~| <text>` for a context-only neighbour (no question, no answer).

#![allow(dead_code)]

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

/// A running `jevgrep` child with piped stdin/stdout/stderr, for tests that
/// write to stdin incrementally and need to observe stdout *before* the
/// process exits (proving matches print as they happen, not only at EOF),
/// or that need to signal the process directly. Always killed and reaped
/// on drop, including during test-failure unwinding, so a broken test can
/// never leak a child process.
pub struct StreamChild {
    pub child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout_buf: Arc<Mutex<String>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl StreamChild {
    /// Spawn `jevgrep <args...>` wired to the mock server with a fake key,
    /// isolated from the real environment exactly like [`base_cmd`].
    pub fn spawn(server: &MockServer, args: &[&str], cwd: &std::path::Path) -> Self {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_jevgrep"));
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("TYPESAFE_API_KEY", "test");
        cmd.env("JEVGREP_API_URL", api_url(server));
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().expect("spawn jevgrep");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let buf_clone = stdout_buf.clone();
        let reader_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut stdout = stdout;
            let mut chunk = [0u8; 4096];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&chunk[..n]).into_owned();
                        buf_clone.lock().unwrap().push_str(&s);
                    }
                }
            }
        });
        // Drain stderr too so the child can never block writing to it (a
        // full, unread pipe buffer would otherwise stall the process
        // indefinitely, which would look exactly like a hang to the test).
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut stderr = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        StreamChild {
            child,
            stdin,
            stdout_buf,
            reader_thread: Some(reader_thread),
            stderr_thread: Some(stderr_thread),
        }
    }

    /// Write `s` to the child's stdin and flush immediately.
    pub fn write_stdin(&mut self, s: &str) {
        use std::io::Write;
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(s.as_bytes()).expect("write to child stdin");
            stdin.flush().expect("flush child stdin");
        }
    }

    /// Close stdin (send EOF) by dropping the write handle.
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Everything written to stdout so far, regardless of whether the
    /// child has exited yet.
    pub fn stdout_so_far(&self) -> String {
        self.stdout_buf.lock().unwrap().clone()
    }

    /// Poll until the child exits or `timeout` passes, returning its exit
    /// code if it exited in time.
    pub fn wait_timeout(&mut self, timeout: std::time::Duration) -> Option<i32> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for StreamChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
    }
}
