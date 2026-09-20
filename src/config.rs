//! Local config file (`<config_dir>/config.toml`) holding the TypeSafe API
//! key, so users can run one command instead of always setting
//! `TYPESAFE_API_KEY`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Directory holding jevgrep's config file: `$XDG_CONFIG_HOME/jevgrep` if
/// `XDG_CONFIG_HOME` is set and non-empty, else `$HOME/.config/jevgrep`
/// (matching tools like `gh`/`cargo` rather than macOS's
/// `Library/Application Support`). On Windows, falls back to
/// `%APPDATA%\jevgrep` when `HOME` is absent.
pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("jevgrep");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".config").join("jevgrep");
        }
    }
    if cfg!(windows) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("jevgrep");
            }
        }
    }
    // Last-resort fallback so config_dir() always returns something usable
    // even in a stripped-down environment with neither HOME nor APPDATA.
    PathBuf::from(".jevgrep")
}

/// Path to the config file itself.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Load the config file. `Ok(None)` if it doesn't exist; a parse error names
/// the path rather than being silently ignored.
pub fn load() -> anyhow::Result<Option<Config>> {
    let path = config_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("parsing config file {}", path.display()))?;
    Ok(Some(config))
}

/// Save `key` (trimmed) as the config file's `api_key`, creating the config
/// directory and file if needed. Returns the path written.
pub fn save_api_key(key: &str) -> anyhow::Result<PathBuf> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API key must not be empty");
    }

    let dir = config_dir();
    create_dir_all_secure(&dir)?;

    let mut config = load()?.unwrap_or_default();
    config.api_key = Some(key.to_string());

    let path = dir.join("config.toml");
    let toml_str = toml::to_string_pretty(&config).context("serializing config")?;
    write_file_secure(&path, &toml_str)?;

    Ok(path)
}

/// Mask a secret for display: first 4 and last 4 characters with `…`
/// between when long enough to do so meaningfully, else `****`.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 12 {
        let first: String = chars[..4].iter().collect();
        let last: String = chars[chars.len() - 4..].iter().collect();
        format!("{first}…{last}")
    } else {
        "****".to_string()
    }
}

#[cfg(unix)]
fn create_dir_all_secure(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", dir.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_dir_all_secure(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_file_secure(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    // `mode()` on OpenOptions only applies at file *creation*, so an
    // already-existing (possibly world-readable) file needs its
    // permissions fixed explicitly too.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_file_secure(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
