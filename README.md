# jevgrep

`grep` (ripgrep-flavoured) where the pattern is a natural-language query judged by the
TypeSafe "System One" API (model `jev-latest`). It walks files like ripgrep, sends every
candidate line to jev together with a few lines of surrounding context, and prints the lines
jev says match — in grep's output format, with grep's exit codes.

## Install

```sh
cargo install --path .
```

## Setup

`jevgrep` reads `TYPESAFE_API_KEY` from the environment, falling back to a `.env` file in the
current directory. Copy `.env.example` to `.env` and fill in your key:

```sh
cp .env.example .env
```

The key is never logged.

## Examples

```sh
# Search a directory for TODOs about auth
jevgrep "TODOs that mention auth" src/

# Pipe input in
cat access.log | jevgrep "requests that look like a scraper"

# Show 2 lines of context around each match
jevgrep "error handling for network timeouts" -C 2 src/

# Preview scope and cost before running a big search
jevgrep "SQL queries missing a WHERE clause" --dry-run .
```

Before pointing `jevgrep` at a large tree, run it with `--dry-run` first: it walks the same
files, skips the same ignored/binary/hidden ones, and reports how many files and lines it
would see and about how many API requests that implies — with no network calls and no key
required.

## How it works

For each candidate line, `jevgrep` sends the line plus `--judge-context` lines of surrounding
text to jev and asks whether it matches your query. Blank lines are never sent (they're an
automatic non-match), and identical (line + context) is only ever asked once per run — later
occurrences reuse the first answer. Lines are batched (`--batch`) and multiple batches are
judged concurrently (`--concurrency`), but output is always printed in strict file order, then
line order, regardless of which batch's answer comes back first.

## Cost model

Measured on this repo's `src/` (judge-context 2, batch 16): 1,458 judged lines → 91 requests,
~250k input / ~43k output tokens, 5.5 s — about **170 input tokens per judged line**. The
default `--max-lines 2000` therefore caps a run at roughly 350k input tokens.

Roughly: `lines judged × (1 + 2 × judge-context)` lines of text ≈ tokens sent per run. With
the defaults (`--max-lines 2000`, `--batch 16`), a run is bounded at about 2000 lines ≈ 125
requests. Free lines (blank, or an exact cache hit) don't count against `--max-lines` and
don't cost a request. Use `--dry-run` to see the real numbers for your search before running
it, and `--max-line-length` to cap how much of a very long line gets sent.

## Bounds

`--max-files` (default 500) and `--max-lines` (default 2000) are hard stops, not soft limits:
files are counted as they're opened, lines as they're queued for jev (a blank line or a cache
hit doesn't count). Hitting either bound stops the walk immediately — `jevgrep` does not read
the rest of the tree first and truncate afterward. It prints whatever was already queued, then
a message like:

```
jevgrep: stopped at --max-lines 2000 (37 files not searched); raise the limit or narrow with -g
```

and exits with status 2, even though it may have printed real matches. Pass `0` to either flag
to disable it. `Ctrl-C` behaves the same way: it stops cleanly, printing what's already been
resolved (and `--stats`, if asked), and exits 2.

## Flags

### Search scope

| flag | default | meaning |
| --- | --- | --- |
| `-g, --glob <GLOB>` | | only search files matching GLOB (repeatable; `!GLOB` excludes) |
| `--hidden` | off | search hidden files/dirs |
| `--no-ignore` | off | don't respect `.gitignore`/`.ignore` |
| `--max-files <N>` | `500` | stop after opening N files (`0` = unlimited) |
| `--max-lines <N>` | `2000` | stop after sending N lines to jev (`0` = unlimited) |
| `--max-line-length <N>` | `1000` | lines longer than N bytes are truncated to N before judging (output prints the full line) |
| `--dry-run` | off | walk and count only; print files, lines, estimated requests to stderr; no API calls; exit 0 |

### Judging

| flag | default | meaning |
| --- | --- | --- |
| `-t, --threshold <P>` | `0.5` | match when `probabilities.yes >= P` |
| `-v, --invert-match` | off | print the lines that do **not** match |
| `--judge-context <N>` | `2` | lines before/after each line sent to jev as context (`0` = judge in isolation) |
| `-b, --batch <N>` | `16` | max lines per API request |
| `-j, --concurrency <N>` | `4` | max in-flight requests |
| `--no-cache` | off | disable the identical-(line+context) verdict cache |
| `--model <M>` | `jev-latest` | model id |
| `--timeout-ms <MS>` | `30000` | per-request timeout |
| `--api-url <URL>` | `https://api.typesafe.ai/v1/systemone` | endpoint override; also `JEVGREP_API_URL` env |
| `--strict` | off | exit 2 immediately on the first API error instead of warning and continuing |

### Output

| flag | default | meaning |
| --- | --- | --- |
| `-n, --line-number` | off | prefix with line number |
| `-H, --with-filename` | auto | always prefix with filename |
| `-h, --no-filename` | auto | never prefix with filename. Auto: filename shown iff more than one file is searched (any directory argument counts as "more than one") |
| `-l, --files-with-matches` | off | print only the names of files with at least one match |
| `-L, --files-without-match` | off | print only the names of files with no match |
| `-c, --count` | off | print `file:count` per file (or just `count` when filename is suppressed) |
| `-A, --after-context <N>` | `0` | print N lines after each match |
| `-B, --before-context <N>` | `0` | print N lines before each match |
| `-C, --context <N>` | `0` | both |
| `--show-score` | off | prefix each printed match with `p=0.97 ` |
| `--json` | off | one JSON object per match, no other stdout output |
| `--stats` | off | totals to stderr at exit |
| `--help` | | print help (`-h` is taken by `--no-filename`) |

Line format: `[p=0.97 ][file:][lineno:]text`. Context lines use `-` instead of `:` as the
separator (`file-lineno-text`), and non-adjacent groups are separated by a `--` line, exactly
like grep.

`--json` emits one object per match: `{"file":"src/a.rs","lineno":42,"line":"...","probability":0.97,"confidence":0.9}`.
Stdin is reported as `"file":"<stdin>"`. Context flags are ignored under `--json`, `-l`, `-L`, `-c`.

## Exit codes

Grep semantics: `0` at least one line matched, `1` no line matched, `2` error. "Error" covers
a missing key, an unreadable path named directly on the command line, any API failure (warned
or `--strict`), and hitting `--max-files`/`--max-lines` — even if real matches were printed
before the bound was hit. An unreadable file *discovered while walking a directory* is warned
about and skipped, like grep, and does not by itself cause exit 2.

## Development

```sh
cargo build --release
cargo clippy -- -D warnings
cargo fmt --check
cargo test
```

Tests run against a local mock server (`wiremock`); no real API calls are made and no real key
is needed (`tests/` sets `JEVGREP_API_URL` and a dummy `TYPESAFE_API_KEY` for every case).
