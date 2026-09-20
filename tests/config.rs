//! Tests for the `jevgrep config ...` subcommand and config-file-backed API
//! key resolution.
//!
//! Every test is isolated from the real environment: cleared env (so the
//! repo's real `.env`/`TYPESAFE_API_KEY` never leaks in), `current_dir`
//! into a tempdir, and `XDG_CONFIG_HOME`/`HOME` pointed into the tempdir (so
//! the developer's real `~/.config/jevgrep` is never touched).

mod common;

use common::{api_url, start, Scorer};

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;

/// A fresh `jevgrep` command isolated into `tmp`: cleared env, current dir
/// set to `tmp`, and `XDG_CONFIG_HOME`/`HOME` pointed into `tmp`. Callers
/// add `TYPESAFE_API_KEY`/`--api-url` themselves where a test needs them.
fn isolated_cmd(tmp: &Path) -> Command {
    let mut cmd = Command::cargo_bin("jevgrep").unwrap();
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.current_dir(tmp)
        .env("XDG_CONFIG_HOME", tmp.join("cfg"))
        .env("HOME", tmp);
    cmd
}

fn config_file_path(tmp: &Path) -> std::path::PathBuf {
    tmp.join("cfg").join("jevgrep").join("config.toml")
}

#[tokio::test(flavor = "multi_thread")]
async fn set_key_from_arg_writes_0600_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = isolated_cmd(tmp.path());
    cmd.args(["config", "set-key", "abc123secretKEY"]);

    let assert = cmd.assert().code(0);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

    let config_path = config_file_path(tmp.path());
    assert!(
        stderr.contains(&config_path.display().to_string()),
        "stderr should mention the config path: {stderr:?}"
    );

    let contents = std::fs::read_to_string(&config_path).expect("read config file");
    assert!(contents.contains(r#"api_key = "abc123secretKEY""#));

    #[cfg(unix)]
    {
        let perms = std::fs::metadata(&config_path)
            .expect("stat config file")
            .permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_key_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = isolated_cmd(tmp.path());
    cmd.args(["config", "set-key"]).write_stdin("piped-key\n");

    cmd.assert().code(0);

    let config_path = config_file_path(tmp.path());
    let contents = std::fs::read_to_string(&config_path).expect("read config file");
    assert!(contents.contains(r#"api_key = "piped-key""#));
}

#[tokio::test(flavor = "multi_thread")]
async fn run_uses_config_key() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("some.log"), "alpha line\nbeta line\n").unwrap();

    let scorer = Scorer::matching("alpha");
    let server = start(scorer).await;

    let mut set_cmd = isolated_cmd(tmp.path());
    set_cmd.args(["config", "set-key", "abc123secretKEY"]);
    set_cmd.assert().code(0);

    let mut run_cmd = isolated_cmd(tmp.path());
    run_cmd
        .arg("--api-url")
        .arg(api_url(&server))
        .arg("anything")
        .arg("some.log");
    run_cmd.assert().code(0);

    let received = server.received_requests().await.expect("recording enabled");
    assert!(!received.is_empty(), "expected at least one request");
    let auth = received[0]
        .headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .expect("header is valid utf8");
    assert_eq!(auth, "Bearer abc123secretKEY");
}

#[tokio::test(flavor = "multi_thread")]
async fn env_key_overrides_config() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("some.log"), "alpha line\n").unwrap();

    let scorer = Scorer::matching("alpha");
    let server = start(scorer).await;

    let mut set_cmd = isolated_cmd(tmp.path());
    set_cmd.args(["config", "set-key", "config-key"]);
    set_cmd.assert().code(0);

    let mut run_cmd = isolated_cmd(tmp.path());
    run_cmd
        .env("TYPESAFE_API_KEY", "env-key")
        .arg("--api-url")
        .arg(api_url(&server))
        .arg("anything")
        .arg("some.log");
    run_cmd.assert().code(0);

    let received = server.received_requests().await.expect("recording enabled");
    let auth = received[0]
        .headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .expect("header is valid utf8");
    assert_eq!(auth, "Bearer env-key");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_key_mentions_config_command() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("some.log"), "anything at all\n").unwrap();

    let scorer = Scorer::matching("anything");
    let server = start(scorer).await;

    let mut cmd = isolated_cmd(tmp.path());
    cmd.arg("--api-url")
        .arg(api_url(&server))
        .arg("anything")
        .arg("some.log");

    cmd.assert()
        .code(2)
        .stderr(predicates::str::contains("jevgrep config set-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn show_masks_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut set_cmd = isolated_cmd(tmp.path());
    set_cmd.args(["config", "set-key", "abcdefghijklmnopqrstuvwxyz"]);
    set_cmd.assert().code(0);

    let mut show_cmd = isolated_cmd(tmp.path());
    show_cmd.args(["config", "show"]);
    let assert = show_cmd.assert().code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("abcd\u{2026}wxyz"),
        "expected masked key in output: {stdout:?}"
    );
    assert!(
        !stdout.contains("abcdefghijklmnopqrstuvwxyz"),
        "full key must not appear in `config show` output: {stdout:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_config_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg").join("jevgrep");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(&cfg_path, "api_key = [").expect("write malformed config");

    std::fs::write(tmp.path().join("some.log"), "anything at all\n").unwrap();
    let scorer = Scorer::matching("anything");
    let server = start(scorer).await;

    let mut cmd = isolated_cmd(tmp.path());
    cmd.arg("--api-url")
        .arg(api_url(&server))
        .arg("anything")
        .arg("some.log");

    cmd.assert()
        .code(2)
        .stderr(predicates::str::contains(cfg_path.display().to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn double_dash_config_is_a_query() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.log"), "alpha\nbeta\n").unwrap();

    let scorer = Scorer::matching("alpha");
    let server = start(scorer).await;

    let mut cmd = isolated_cmd(tmp.path());
    cmd.env("TYPESAFE_API_KEY", "test-key")
        .arg("--api-url")
        .arg(api_url(&server))
        .arg("--")
        .arg("config")
        .arg("config.log");

    cmd.assert().code(0);

    let received = server.received_requests().await.expect("recording enabled");
    assert!(
        !received.is_empty(),
        "`jevgrep -- config <file>` should run as a filter, not the config subcommand"
    );
}
