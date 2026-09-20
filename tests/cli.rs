mod common;

use std::fs;

use predicates::prelude::*;
use tempfile::tempdir;

use common::{base_cmd, fixtures_dir, parse_log, start, Scorer};

// ---------------------------------------------------------------------
// Basic filtering, filename/lineno prefixes, auto-filename rule
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn basic_filtering_with_filename_and_lineno() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(fixtures_dir())
        .args(["MATCH lines", "a.txt", "-n", "-H"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "a.txt:2:a MATCH here\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_filename_suppressed_for_single_file() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(fixtures_dir())
        .args(["MATCH lines", "a.txt"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "a MATCH here\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_filename_shown_for_directory_argument() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("only.txt"), "a MATCH line\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["MATCH lines", "."])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    // A directory argument counts as "more than one file", even though
    // exactly one file was found inside it.
    assert!(stdout.contains("only.txt:"), "stdout was: {stdout:?}");
}

// ---------------------------------------------------------------------
// Threshold / invert-match
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn threshold_controls_the_cutoff() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a HIGH line\na LOW line\n").unwrap();

    let scorer = std::sync::Arc::new(Scorer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_first_n: 0,
        fail_status: 200,
        delay_marker: None,
        delay_ms: 0,
        score: Box::new(|line: &str| if line.contains("HIGH") { 0.6 } else { 0.3 }),
        seen_bodies: std::sync::Mutex::new(Vec::new()),
    });
    let server = start(scorer).await;

    // Default threshold 0.5: only the HIGH line matches.
    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "a HIGH line\n");

    // Threshold 0.7: nothing clears the bar.
    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "-t", "0.7"])
        .assert()
        .code(1)
        .stdout("");

    // Threshold 0.2: everything clears the bar.
    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "-t", "0.2"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "a HIGH line\na LOW line\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn invert_match_prints_the_opposite_lines() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a cat sat\na dog ran\n").unwrap();

    let scorer = Scorer::matching("cat");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["animal", "f.txt"])
        .assert()
        .code(0);
    assert_eq!(
        String::from_utf8(out.get_output().stdout.clone()).unwrap(),
        "a cat sat\n"
    );

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["animal", "f.txt", "-v"])
        .assert()
        .code(0);
    assert_eq!(
        String::from_utf8(out.get_output().stdout.clone()).unwrap(),
        "a dog ran\n"
    );
}

// ---------------------------------------------------------------------
// -l / -L / -c
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn files_with_without_matches_and_count() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f1.txt"), "a MATCH line\nplain\n").unwrap();
    fs::write(dir.path().join("f2.txt"), "nothing here\nstill nothing\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "."])
        .arg("-l")
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("f1.txt"));
    assert!(!stdout.contains("f2.txt"));

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "."])
        .arg("-L")
        .assert()
        .code(0); // f1.txt still has a real match elsewhere in the run
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("f2.txt"));
    assert!(!stdout.contains("f1.txt"));

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "."])
        .arg("-c")
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("f1.txt:1"));
    assert!(stdout.contains("f2.txt:0"));
}

// ---------------------------------------------------------------------
// -C context grouping with "--"
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn context_grouping_separates_with_double_dash() {
    let dir = tempdir().unwrap();
    let content = "l1\nl2\na MATCH one\nl4\nl5\nl6\na MATCH two\nl8\nl9\nl10\n";
    fs::write(dir.path().join("f.txt"), content).unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "-C", "1", "-n"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let expected = "\
2-l2
3:a MATCH one
4-l4
--
6-l6
7:a MATCH two
8-l8
";
    assert_eq!(stdout, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_separator_without_context_flags() {
    // grep only prints `--` between groups when -A/-B/-C is in effect.
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH\nx\ny\nb MATCH\n").unwrap();
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;
    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "a MATCH\nb MATCH\n");
}

// ---------------------------------------------------------------------
// --json
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn json_output_has_expected_fields() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH line\nplain\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "--json"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let line = stdout.trim_end();
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["file"], "f.txt");
    assert_eq!(v["lineno"], 1);
    assert_eq!(v["line"], "a MATCH line");
    assert!(v["probability"].as_f64().unwrap() >= 0.5);
    assert!(
        v.get("confidence").is_none(),
        "confidence was removed: {v:?}"
    );
}

// ---------------------------------------------------------------------
// Ordering under out-of-order batch completion
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn output_order_survives_a_delayed_first_batch() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "first ITEM\nsecond ITEM\n").unwrap();

    let scorer = std::sync::Arc::new(Scorer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_first_n: 0,
        fail_status: 200,
        delay_marker: Some("first ITEM".to_string()),
        delay_ms: 300,
        score: Box::new(|line: &str| if line.contains("ITEM") { 0.9 } else { 0.1 }),
        seen_bodies: std::sync::Mutex::new(Vec::new()),
    });
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "-b", "1", "-j", "2"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, "first ITEM\nsecond ITEM\n");
}

// ---------------------------------------------------------------------
// Judge-context contents in the request body
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn judge_context_is_sent_in_the_request() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();

    // With --judge-context 1 and --batch 2, a 5-line file dispatches in
    // three requests (lines 1-2, 3-4, 5), each carrying up to one leading
    // and one trailing `  ~| ` context row from its neighbours. The
    // second request (lines 3-4) has both: a before row (line 2) and an
    // after row (line 5).
    let scorer = Scorer::matching("NEVER-MATCHES");
    let server = start(scorer.clone()).await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args([
            "q",
            "f.txt",
            "--judge-context",
            "1",
            "--batch",
            "2",
            "-j",
            "1",
        ])
        .assert()
        .code(1); // nothing matches this query -> exit 1, not an error

    let bodies = scorer.bodies();
    assert!(
        bodies.len() >= 2,
        "expected at least 2 requests, got {}: {bodies:?}",
        bodies.len()
    );
    let second_log = bodies[1]["state"]["log"].as_str().unwrap_or("");
    let context_rows: Vec<&str> = second_log
        .lines()
        .filter(|l| l.starts_with("  ~| "))
        .collect();
    assert_eq!(
        context_rows.len(),
        2,
        "expected one before and one after context row in the second request, got: {second_log:?}"
    );

    // With --judge-context 0, no context rows ever appear, in any request.
    let scorer0 = Scorer::matching("NEVER-MATCHES");
    let server0 = start(scorer0.clone()).await;
    base_cmd(&server0)
        .current_dir(dir.path())
        .args([
            "q",
            "f.txt",
            "--judge-context",
            "0",
            "--batch",
            "2",
            "-j",
            "1",
        ])
        .assert()
        .code(1);
    for body in scorer0.bodies() {
        let log = body["state"]["log"].as_str().unwrap_or("");
        assert!(
            !log.contains("  ~| "),
            "no context rows expected with --judge-context 0: {log:?}"
        );
    }
}

// ---------------------------------------------------------------------
// Cache reuse: identical (line+context) lines collapse to one ask
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn identical_lines_are_deduped_before_asking() {
    let dir = tempdir().unwrap();
    let content = "dup MATCH\ndup MATCH\ndup MATCH\ndup MATCH\ndup MATCH\nother one\nother two\n";
    fs::write(dir.path().join("f.txt"), content).unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer.clone()).await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "--judge-context", "0"])
        .assert()
        .code(0);

    let bodies = scorer.bodies();
    assert_eq!(bodies.len(), 1, "expected everything to fit one request");
    let log = bodies[0]["state"]["log"].as_str().unwrap_or("");
    let (candidates, _context) = parse_log(log);
    // 5 duplicate "dup MATCH" + 2 distinct lines => 3 unique asks, not 7.
    assert_eq!(candidates.len(), 3, "log was: {log:?}");
}

// ---------------------------------------------------------------------
// Blank lines are never sent
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn blank_lines_are_never_sent_to_jev() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH\n\n   \nb MATCH\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer.clone()).await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "--judge-context", "0"])
        .assert()
        .code(0);

    let bodies = scorer.bodies();
    let mut total_candidates = 0usize;
    for body in &bodies {
        let log = body["state"]["log"].as_str().unwrap_or("");
        let (candidates, context) = parse_log(log);
        total_candidates += candidates.len();
        for (id, text) in &candidates {
            assert!(
                !text.trim().is_empty(),
                "blank candidate row {id} sent: {log:?}"
            );
        }
        for text in &context {
            assert!(!text.trim().is_empty(), "blank context row sent: {log:?}");
        }
    }
    assert_eq!(total_candidates, 2);
}

// ---------------------------------------------------------------------
// --max-lines / --max-files stop with exit 2
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn max_lines_stop_message_and_exit_2() {
    let dir = tempdir().unwrap();
    // Every queued line matches, so the truncated run still prints real
    // matches before the bound stops it -- exit 2 even so (spec: "a
    // truncated search is not a clean run, even if it printed matches").
    let content = "line a\nline b\nline c\nline d\nline e\n";
    fs::write(dir.path().join("f.txt"), content).unwrap();

    let scorer = Scorer::matching("line");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "--judge-context", "0", "--max-lines", "2"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "jevgrep: stopped at --max-lines 2",
        ));
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        !stdout.is_empty(),
        "matches already queued must still print"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn max_files_stop_message_and_exit_2() {
    let dir = tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(dir.path().join(name), "a MATCH line\n").unwrap();
    }

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", ".", "--max-files", "1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "jevgrep: stopped at --max-files 1",
        ));
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        !stdout.is_empty(),
        "the one opened file's match must still print"
    );
}

// ---------------------------------------------------------------------
// -g/--glob: whitelist, negation, and that it still descends into
// subdirectories (an easy thing for an override-based implementation to
// get wrong).
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn glob_include_and_exclude() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.rs"), "a MATCH in rust\n").unwrap();
    fs::write(dir.path().join("b.txt"), "a MATCH in text\n").unwrap();
    fs::write(dir.path().join("sub/c.rs"), "a MATCH nested\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", ".", "-g", "*.rs"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("a.rs"));
    assert!(
        stdout.contains("c.rs"),
        "glob must still descend into subdirs: {stdout:?}"
    );
    assert!(!stdout.contains("b.txt"));

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", ".", "-g", "!*.txt"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("a.rs"));
    assert!(!stdout.contains("b.txt"));
}

// ---------------------------------------------------------------------
// --stats and --show-score
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stats_and_show_score() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(fixtures_dir())
        .args(["MATCH lines", "a.txt", "--stats", "--show-score"])
        .assert()
        .code(0);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.starts_with("p=0.95 "), "stdout was: {stdout:?}");

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.starts_with("jevgrep: "));
    assert!(stderr.contains(" files, "));
    assert!(stderr.contains(" lines ("));
    assert!(stderr.contains(" judged, "));
    assert!(stderr.contains(" free), "));
    assert!(stderr.contains(" matches, "));
    assert!(stderr.contains(" requests, "));
    assert!(stderr.contains(" in / "));
    assert!(stderr.contains(" out tokens, "));
}

// ---------------------------------------------------------------------
// --dry-run makes zero requests
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_makes_zero_requests() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH\nplain\n").unwrap();

    let scorer = Scorer::matching("MATCH");
    let server = start(scorer.clone()).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "--dry-run"])
        .assert()
        .code(0);
    assert_eq!(out.get_output().stdout, Vec::<u8>::new());
    assert_eq!(scorer.request_count(), 0);

    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("requests"));
}

// ---------------------------------------------------------------------
// gitignore and binary skipping (uses the fixtures/ tree)
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn gitignore_hidden_and_binary_are_skipped_by_default() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(fixtures_dir())
        .args(["MATCH lines", "."])
        .assert();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("a.txt"));
    assert!(stdout.contains("b.txt"));
    assert!(!stdout.contains("ignored.txt"));
    assert!(!stdout.contains(".hidden:"));
    assert!(!stdout.contains("binary.bin"));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_ignore_and_hidden_flags_include_those_files() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let out = base_cmd(&server)
        .current_dir(fixtures_dir())
        .args(["MATCH lines", ".", "--no-ignore", "--hidden"])
        .assert();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("ignored.txt"));
    assert!(stdout.contains(".hidden:"));
    // Binary files are always skipped, ignore flags notwithstanding.
    assert!(!stdout.contains("binary.bin"));
}

// ---------------------------------------------------------------------
// Retry on 429, readable 401 message
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn retries_on_429_then_succeeds() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH line\n").unwrap();

    let scorer = std::sync::Arc::new(Scorer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_first_n: 1,
        fail_status: 429,
        delay_marker: None,
        delay_ms: 0,
        score: Box::new(|line: &str| if line.contains("MATCH") { 0.9 } else { 0.1 }),
        seen_bodies: std::sync::Mutex::new(Vec::new()),
    });
    let server = start(scorer.clone()).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt"])
        .assert()
        .code(0);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.is_empty(), "stderr was: {stderr:?}");
    assert_eq!(scorer.request_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn readable_401_message_no_retry() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "a MATCH line\n").unwrap();

    let scorer = std::sync::Arc::new(Scorer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_first_n: usize::MAX,
        fail_status: 401,
        delay_marker: None,
        delay_ms: 0,
        score: Box::new(|_: &str| 0.9),
        seen_bodies: std::sync::Mutex::new(Vec::new()),
    });
    let server = start(scorer.clone()).await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "jevgrep: TypeSafe API rejected the key (HTTP 401)",
        ));
    assert_eq!(scorer.request_count(), 1, "401 must not be retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_key_aborts_whole_run() {
    // 40 distinct lines at --batch 4 = 10 batches; a 401 must stop the run
    // after the first one, with the message printed once, not per batch.
    let dir = tempdir().unwrap();
    let body: String = (0..40).map(|i| format!("line number {i}\n")).collect();
    fs::write(dir.path().join("f.txt"), body).unwrap();

    let scorer = std::sync::Arc::new(Scorer {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_first_n: usize::MAX,
        fail_status: 401,
        delay_marker: None,
        delay_ms: 0,
        score: Box::new(|_: &str| 0.9),
        seen_bodies: std::sync::Mutex::new(Vec::new()),
    });
    let server = start(scorer.clone()).await;

    let out = base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt", "-b", "4", "-j", "1"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("rejected the key").count(),
        1,
        "401 message must be printed exactly once, got:\n{stderr}"
    );
    assert!(
        scorer.request_count() <= 2,
        "401 must abort the run (got {} requests)",
        scorer.request_count()
    );
}

// ---------------------------------------------------------------------
// Exit codes 0 / 1 / 2, missing key, stdin
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn exit_code_1_when_nothing_matches() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "nothing interesting\n").unwrap();

    let scorer = Scorer::matching("NEVER-PRESENT-MARKER");
    let server = start(scorer).await;

    base_cmd(&server)
        .current_dir(dir.path())
        .args(["q", "f.txt"])
        .assert()
        .code(1)
        .stdout("");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_api_key_exits_2_with_exact_message() {
    let dir = tempdir().unwrap();
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let mut cmd = base_cmd(&server);
    cmd.env_remove("TYPESAFE_API_KEY");
    cmd.current_dir(dir.path()).args(["q", "."]);
    cmd.assert().code(2).stderr(
        "jevgrep: missing API key: run `jevgrep config set-key`, or set TYPESAFE_API_KEY \
             (environment or .env)\n",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_from_stdin_when_no_path_given() {
    let scorer = Scorer::matching("MATCH");
    let server = start(scorer).await;

    let mut cmd = base_cmd(&server);
    let assert = cmd
        .current_dir(std::env::temp_dir())
        .args(["q", "--json"])
        .write_stdin("a MATCH line\nplain line\n")
        .assert()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(v["file"], "<stdin>");
    assert_eq!(v["line"], "a MATCH line");
}
