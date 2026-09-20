//! Ordered file enumeration: ripgrep-style ignore rules, explicit-argument
//! overrides, and the auto-filename ("multi") rule.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use globset::{Glob, GlobSetBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// One thing to search: an explicit file, a file discovered while walking a
/// directory, or stdin.
#[derive(Debug, Clone)]
pub enum FileArg {
    Stdin,
    /// `explicit` is true for a file named directly on the command line
    /// (never filtered by ignore rules; an open failure is a hard error).
    /// It is false for a file discovered while walking a directory
    /// argument (ignore rules apply; an open failure is a warning).
    Path {
        path: PathBuf,
        explicit: bool,
    },
}

impl FileArg {
    pub fn label(&self) -> String {
        match self {
            FileArg::Stdin => "<stdin>".to_string(),
            FileArg::Path { path, .. } => path.display().to_string(),
        }
    }
}

/// Build the glob override set for `-g/--glob`, ripgrep semantics: a plain
/// glob whitelists, `!glob` excludes.
fn build_overrides(root: &Path, globs: &[String]) -> Result<ignore::overrides::Override> {
    let mut builder = OverrideBuilder::new(root);
    for g in globs {
        builder
            .add(g)
            .map_err(|e| anyhow!("invalid --glob {g:?}: {e}"))?;
    }
    builder
        .build()
        .map_err(|e| anyhow!("invalid --glob pattern: {e}"))
}

/// Build a validated globset purely to reject bad `-g` patterns up front
/// with a clear error, independent of the `ignore` crate's own diagnostics.
pub fn validate_globs(globs: &[String]) -> Result<()> {
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        let pat = g.strip_prefix('!').unwrap_or(g);
        let glob = Glob::new(pat).map_err(|e| anyhow!("invalid --glob {g:?}: {e}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| anyhow!("invalid --glob pattern: {e}"))?;
    Ok(())
}

/// Determine whether stdin should be used as the (only) input when no PATH
/// arguments were given.
pub fn stdin_is_input() -> bool {
    !std::io::stdin().is_terminal()
}

/// Whether filenames should be shown by default (before `-H`/`-h`
/// override): true iff more than one file is searched, or any argument is a
/// directory. This is computed from the CLI arguments themselves (not the
/// number of files actually found).
pub fn compute_multi(paths: &[String]) -> Result<bool> {
    if paths.is_empty() {
        // No PATH: stdin (single source) or default directory "." (multi).
        return Ok(!stdin_is_input());
    }
    if paths.len() > 1 {
        return Ok(true);
    }
    let p = &paths[0];
    if p == "-" {
        return Ok(false);
    }
    let meta = std::fs::metadata(p);
    match meta {
        Ok(m) => Ok(m.is_dir()),
        // Unreadable path: treated as non-directory here; the real error is
        // raised later when we actually try to open it.
        Err(_) => Ok(false),
    }
}

/// Enumerate, in order, every file that should be searched, honoring
/// ignore/hidden/glob rules for directory arguments and always including
/// explicit file arguments regardless of those rules.
pub fn enumerate_files(
    paths: &[String],
    hidden: bool,
    no_ignore: bool,
    globs: &[String],
) -> Result<Vec<FileArg>> {
    if paths.is_empty() {
        if stdin_is_input() {
            return Ok(vec![FileArg::Stdin]);
        }
        return enumerate_dir(Path::new("."), hidden, no_ignore, globs);
    }

    let mut out = Vec::new();
    for p in paths {
        if p == "-" {
            out.push(FileArg::Stdin);
            continue;
        }
        let path = PathBuf::from(p);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| anyhow!("{}: {}", path.display(), e))?;
        if meta.is_dir() {
            out.extend(enumerate_dir(&path, hidden, no_ignore, globs)?);
        } else {
            out.push(FileArg::Path {
                path,
                explicit: true,
            });
        }
    }
    Ok(out)
}

fn enumerate_dir(
    root: &Path,
    hidden: bool,
    no_ignore: bool,
    globs: &[String],
) -> Result<Vec<FileArg>> {
    let overrides = build_overrides(root, globs)?;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!hidden)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_global(!no_ignore)
        .git_exclude(!no_ignore)
        .parents(!no_ignore)
        .require_git(false)
        .follow_links(false)
        .overrides(overrides)
        .sort_by_file_path(|a, b| a.cmp(b));

    let mut out = Vec::new();
    for entry in builder.build() {
        match entry {
            Ok(entry) => {
                let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
                if is_file {
                    out.push(FileArg::Path {
                        path: entry.into_path(),
                        explicit: false,
                    });
                }
            }
            Err(e) => {
                eprintln!("jevgrep: {e}");
            }
        }
    }
    Ok(out)
}

/// First 8KiB of `bytes` contains a NUL: treated as a binary file, like
/// ripgrep's default binary detection.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(8192);
    bytes[..n].contains(&0)
}
