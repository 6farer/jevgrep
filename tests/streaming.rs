//! Streaming-specific behaviour that the plan-then-batch pipeline never had
//! to prove: matches appearing before EOF on a live stdin, batches never
//! spanning a file boundary, the sliding-window judge-context handoff
//! across requests, Ctrl-C on a quiet stdin, and (ported from jevtail)
//! byte-budget/large-line handling.

mod common;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::tempdir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use common::{base_cmd, parse_log, start, Scorer, StreamChild};

// ---------------------------------------------------------------------
// a. Matches on a slow/live stdin appear before EOF.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn slow_stdin_matches_appear_before_eof() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let mut child = StreamChild::spawn(&server, &["q"], &std::env::temp_dir());
    child.write_stdin("one MATCH\n");

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let so_far = child.stdout_so_far();
    assert!(
        so_far.contains("one MATCH"),
        "expected the match to print well before EOF, stdout so far: {so_far:?}"
    );

    child.write_stdin("two\n");
    child.close_stdin();

    let code = child.wait_timeout(Duration::from_secs(5));
    assert_eq!(code, Some(0), "expected exit 0");
    assert_eq!(child.stdout_so_far(), "one MATCH\n");
}

// ---------------------------------------------------------------------
// b. A batch never mixes lines from two different files.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn batches_never_span_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a one\na two\na three\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b one\nb two\nb three\n").unwrap();

    let scorer = Scorer::matching("NEVER-MATCHES");
    let server = start(scorer.clone()).await;

    let _ = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", ".", "--batch", "16", "--judge-context", "0"])
        .assert();

    let bodies = scorer.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "expected exactly one request per file: {bodies:?}"
    );
    for body in &bodies {
        let log = body["state"]["log"].as_str().unwrap_or("");
        let (candidates, _context) = parse_log(log);
        assert!(!candidates.is_empty(), "request had no candidates: {log:?}");
        let from_a = candidates
            .iter()
            .filter(|(_, t)| t.starts_with("a "))
            .count();
        let from_b = candidates
            .iter()
            .filter(|(_, t)| t.starts_with("b "))
            .count();
        assert!(
            from_a == 0 || from_b == 0,
            "request mixed lines from both files: {log:?}"
        );
    }
}

// ---------------------------------------------------------------------
// c. The sliding window hands trailing context to the next request as its
//    leading context, and every line ends up a candidate exactly once.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn after_context_is_held_then_reused() {
    let dir = tempdir().unwrap();
    let lines: Vec<String> = (1..=10).map(|i| format!("item{i}")).collect();
    fs::write(dir.path().join("f.txt"), lines.join("\n") + "\n").unwrap();

    let scorer = Scorer::matching("NEVER-MATCHES");
    let server = start(scorer.clone()).await;

    // -j 1 forces strictly sequential dispatch so the requests arrive at
    // the mock in window order; with the default concurrency, the two
    // requests can race onto the wire in either order.
    let _ = base_cmd(&server)
        .current_dir(dir.path())
        .args([
            "q",
            "f.txt",
            "--judge-context",
            "2",
            "--batch",
            "4",
            "--flush-ms",
            "100",
            "-j",
            "1",
        ])
        .assert();

    let bodies = scorer.bodies();
    assert!(
        bodies.len() >= 2,
        "expected at least 2 requests, got {}: {bodies:?}",
        bodies.len()
    );

    let (cand0, ctx0) = parse_log(bodies[0]["state"]["log"].as_str().unwrap_or(""));
    let cand0_texts: Vec<&str> = cand0.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        cand0_texts,
        vec!["item1", "item2", "item3", "item4"],
        "first request candidates"
    );
    assert_eq!(
        ctx0,
        vec!["item5", "item6"],
        "first request trailing context"
    );

    let (cand1, ctx1) = parse_log(bodies[1]["state"]["log"].as_str().unwrap_or(""));
    let cand1_texts: Vec<&str> = cand1.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        cand1_texts,
        vec!["item5", "item6", "item7", "item8"],
        "second request candidates"
    );
    assert_eq!(
        ctx1,
        vec!["item3", "item4", "item9", "item10"],
        "second request context (leading then trailing)"
    );

    // Every line 1-10 must be a candidate in exactly one request, across
    // however many requests the run actually took.
    let mut all_candidates: Vec<String> = Vec::new();
    for body in &bodies {
        let (cands, _) = parse_log(body["state"]["log"].as_str().unwrap_or(""));
        all_candidates.extend(cands.into_iter().map(|(_, t)| t));
    }
    let mut seen_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for c in &all_candidates {
        *seen_counts.entry(c.clone()).or_insert(0) += 1;
    }
    let expected: std::collections::HashSet<String> =
        (1..=10).map(|i| format!("item{i}")).collect();
    let actual: std::collections::HashSet<String> = seen_counts.keys().cloned().collect();
    assert_eq!(
        actual, expected,
        "every line 1-10 must appear as a candidate somewhere"
    );
    for (text, count) in &seen_counts {
        assert_eq!(
            *count, 1,
            "{text} was a candidate in {count} requests, expected exactly 1"
        );
    }
}

// ---------------------------------------------------------------------
// d. Ctrl-C on a quiet, held-open stdin still exits promptly.
// ---------------------------------------------------------------------

#[tokio::test]
async fn sigint_on_quiet_stdin_exits_2() {
    let scorer = Scorer::matching("x");
    let server = start(scorer).await;

    let mut child = StreamChild::spawn(&server, &["q"], &std::env::temp_dir());
    // stdin stays open (piped, never closed) but nothing is written --
    // exactly the "quiet producer" case a blocking stdin read can't
    // otherwise escape. 700ms (matching ../jevtail/tests/signals.rs) gives
    // the child's tokio runtime and Ctrl-C listener time to actually start
    // under a loaded/sandboxed test host; a shorter, tighter delay here was
    // observed to flake by racing signal delivery against that startup.
    std::thread::sleep(Duration::from_millis(700));

    let status = std::process::Command::new("kill")
        .arg("-INT")
        .arg(child.child.id().to_string())
        .status()
        .expect("run kill(1)");
    assert!(status.success(), "kill -INT failed");

    let code = child.wait_timeout(Duration::from_secs(2));
    assert_eq!(
        code,
        Some(2),
        "expected exit 2 within 2s of SIGINT on quiet stdin"
    );
}

// ---------------------------------------------------------------------
// e. Large lines (ported from ../jevtail/tests/large_lines.rs): the API
// caps tokens per request and answers `400
// {"detail":{"error_type":"max_tokens_exceeded"}}` when a batch is over
// it; real logs carry 16KB stack dumps that hit this. jevgrep must (a)
// budget batches by bytes, (b) split a rejected batch rather than
// dropping it, (c) clip a single huge line rather than fail it, and (d)
// put the API's error detail on stderr for any other 4xx.
//
// --judge-context 0 keeps context rows from confusing the byte
// accounting, and --max-line-length 0 disables jevgrep's own pre-judge
// truncation (default 1000 bytes) so these multi-KB lines actually reach
// the judge's own clipping logic, which is what these tests are about.
// ---------------------------------------------------------------------

/// Scores like `common::Scorer`, but rejects any request whose `state.log`
/// is over `max_log_bytes` the way the real API does, and records the
/// largest single candidate row it was ever sent.
struct CappedResponder {
    max_log_bytes: usize,
    rejected: Arc<AtomicUsize>,
    largest_row: Arc<AtomicUsize>,
}

impl Respond for CappedResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(json!({}));
        let log = body["state"]["log"].as_str().unwrap_or("");
        if log.len() > self.max_log_bytes {
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return ResponseTemplate::new(400)
                .set_body_json(json!({"detail": {"error_type": "max_tokens_exceeded"}}));
        }
        let mut answers = serde_json::Map::new();
        for row in log.split('\n') {
            if row.is_empty() || row.starts_with("  ~| ") {
                continue; // blank, or a context row: no question to answer
            }
            let Some((id, text)) = row.split_once("| ") else {
                continue;
            };
            self.largest_row.fetch_max(text.len(), Ordering::SeqCst);
            let p = if text.contains("CRITICAL") {
                0.95
            } else {
                0.05
            };
            answers.insert(id.to_string(), json!({"type": "noul", "noul": p}));
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
    }
}

async fn mount_capped(
    server: &MockServer,
    max_log_bytes: usize,
) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let rejected = Arc::new(AtomicUsize::new(0));
    let largest_row = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(CappedResponder {
            max_log_bytes,
            rejected: rejected.clone(),
            largest_row: largest_row.clone(),
        })
        .mount(server)
        .await;
    (rejected, largest_row)
}

/// Sixteen 3KB lines is 48KB: over a 20KB cap, so a count-only batcher
/// would be rejected. The byte budget must keep every request under the
/// cap with no rejections at all, and every CRITICAL line must still be
/// judged.
#[tokio::test(flavor = "multi_thread")]
async fn byte_budget_keeps_requests_under_the_cap() {
    let dir = tempdir().unwrap();
    let filler = "x".repeat(3000);
    let lines: Vec<String> = (0..16)
        .map(|i| {
            if i % 5 == 0 {
                format!("CRITICAL {i} {filler}")
            } else {
                format!("fine {i} {filler}")
            }
        })
        .collect();
    fs::write(dir.path().join("big.log"), lines.join("\n") + "\n").unwrap();

    let server = MockServer::start().await;
    let (rejected, _largest_row) = mount_capped(&server, 20 * 1024).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args([
            "q",
            "big.log",
            "--max-batch-bytes",
            "16384",
            "--judge-context",
            "0",
            "--max-line-length",
            "0",
            "--json",
        ])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let matched = stdout.lines().filter(|l| !l.trim().is_empty()).count();

    assert_eq!(
        rejected.load(Ordering::SeqCst),
        0,
        "no request may exceed the cap"
    );
    assert_eq!(matched, 4, "all four CRITICAL lines judged: {stdout}");
}

/// With the byte budget set too high, the API rejects the batch. jevgrep
/// must split and retry rather than drop the batch, and must not report an
/// error for lines that were ultimately judged.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_batch_is_split_not_dropped() {
    let dir = tempdir().unwrap();
    let filler = "y".repeat(3000);
    let lines: Vec<String> = (0..8)
        .map(|i| {
            if i == 3 {
                format!("CRITICAL {filler}")
            } else {
                format!("fine {i} {filler}")
            }
        })
        .collect();
    fs::write(dir.path().join("big.log"), lines.join("\n") + "\n").unwrap();

    let server = MockServer::start().await;
    let (rejected, _largest_row) = mount_capped(&server, 7 * 1024).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args([
            "q",
            "big.log",
            "--max-batch-bytes",
            "1000000",
            "--judge-context",
            "0",
            "--max-line-length",
            "0",
        ])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();

    assert!(
        rejected.load(Ordering::SeqCst) >= 1,
        "the first batch should have been rejected"
    );
    assert!(
        stdout.contains("CRITICAL"),
        "the matching line must still be found: {stdout}"
    );
    assert!(
        !stderr.contains("HTTP 400"),
        "a split-and-retried batch is not an error: {stderr}"
    );
}

/// A single 20KB line can't be split. It is sent clipped to the per-line
/// limit and judged on its prefix, but printed in full (raw text is never
/// altered).
#[tokio::test(flavor = "multi_thread")]
async fn single_huge_line_is_clipped_not_failed() {
    let dir = tempdir().unwrap();
    let huge = format!("CRITICAL {}", "z".repeat(20 * 1024));
    fs::write(dir.path().join("huge.log"), format!("{huge}\nfine\n")).unwrap();

    let server = MockServer::start().await;
    let (_rejected, largest_row) = mount_capped(&server, 12 * 1024).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "huge.log", "--max-line-length", "0"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.starts_with("CRITICAL "),
        "huge line judged and printed in full: {}",
        stdout.len()
    );
    assert_eq!(
        stdout.trim_end().len(),
        huge.len(),
        "output must be the original, unclipped line"
    );
    assert!(
        largest_row.load(Ordering::SeqCst) <= 8 * 1024,
        "the API must only ever see the clipped prefix"
    );
}

/// Any other 4xx puts the API's own detail on stderr, not a bare status.
#[tokio::test(flavor = "multi_thread")]
async fn other_4xx_reports_api_detail() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.log"), "one line\n").unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(422).set_body_json(json!({"detail": "model not available"})),
        )
        .mount(&server)
        .await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "a.log"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("HTTP 422"))
        .stderr(predicate::str::contains("model not available"));
}
