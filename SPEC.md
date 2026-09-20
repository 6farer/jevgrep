# jevgrep — spec

`jevgrep` is `grep` (ripgrep-flavoured) where the pattern is a natural-language query judged by
the TypeSafe "System One" API (model `jev-latest`). It walks files like ripgrep, sends every
candidate line to jev together with a few lines of surrounding context, and prints the lines
jev says match — in grep's output format, with grep's exit codes.

Sibling project: `../jevtail` (same API, streaming/`tail -f` semantics). jevgrep is a separate
crate; the API client is copied, not shared. Files, not streams: the input set is fully known
up front, so there is no flush timer and ordered output is trivial.

## 1. Verified API contract (tested live 2026-09-19 — do not guess)

`POST https://api.typesafe.ai/v1/systemone`
Headers: `Authorization: Bearer $TYPESAFE_API_KEY`, `Content-Type: application/json`

Request body (one question per line; `before`/`after` are the judge-context lines):

```json
{
  "model": "jev-latest",
  "state": {
    "query": "code that retries a failed HTTP request",
    "lines": {
      "l0": { "before": ["    for attempt in 1..=3 {", "        match client.post(url).send().await {"],
              "line":   "            Err(e) if attempt < 3 => sleep(backoff(attempt)).await,",
              "after":  ["            Err(e) => return Err(e.into()),", "        }"] },
      "l1": { "before": ["..."], "line": "...", "after": ["..."] }
    }
  },
  "questions": {
    "l0": { "type": "choice",
            "instructions": "Does the line at state.lines.l0.line match the query in state.query? Use state.lines.l0.before and .after only as surrounding context; judge the line itself.",
            "criteria": { "yes": "matches", "no": "does not match" } },
    "l1": { "...": "same shape, with l1 substituted" }
  }
}
```

Response body:

```json
{ "model": "jev-1.13.0",
  "answers": { "l0": { "type": "choice", "choice": "yes", "confidence": 0.87,
                       "probabilities": { "yes": 0.94, "no": 0.06 } },
               "l1": { "type": "choice", "choice": "no",  "confidence": 1.0,
                       "probabilities": { "yes": 0.0,  "no": 1.0 } } },
  "usage": { "input_tokens": 604, "output_tokens": 61 } }
```

Answers are keyed by question id; **assume nothing about ordering** — look answers up by id;
a missing id is an error for that line. `probabilities.yes` is the score; `confidence` is
passed through to `--json` only.

Retry policy: retry on HTTP 429 and 529 and on transport errors; 3 attempts total; backoff
`250ms * 2^(attempt-1)`; per-request timeout 30 s. HTTP 401 → `jevgrep: TypeSafe API rejected
the key (HTTP 401)`, no retry. Any other non-2xx → `jevgrep: TypeSafe API error (HTTP <code>)`
plus the first 200 bytes of the body, no retry.

## 2. CLI contract (fixed — tests are written against this)

```
jevgrep [OPTIONS] <QUERY> [PATH]...
```

* `<QUERY>` — natural-language description of the lines to find. Required, first positional.
* `[PATH]...` — files or directories. Directories are walked recursively. No path: if stdin
  is not a terminal, read stdin; otherwise search `.`. A path of `-` means stdin.

Options (grouped as `--help` should show them):

**Search scope**

| flag | default | meaning |
| --- | --- | --- |
| `-g, --glob <GLOB>` | | only search files matching GLOB (repeatable; `!GLOB` excludes). ripgrep semantics via the `globset`/`ignore` override builder |
| `--hidden` | off | search hidden files/dirs |
| `--no-ignore` | off | don't respect `.gitignore`/`.ignore` |
| `--max-files <N>` | `500` | stop after opening N files (`0` = unlimited) |
| `--max-lines <N>` | `2000` | stop after sending N lines to jev (`0` = unlimited) |
| `--max-line-length <N>` | `1000` | lines longer than N bytes are truncated to N before judging (output prints the full line) |
| `--dry-run` | off | walk and count only; print files, lines, estimated requests to stderr; no API calls; exit 0 |

**Judging**

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
| `--api-url <URL>` | `https://api.typesafe.ai/v1/systemone` | endpoint override; also `JEVGREP_API_URL` env. **Required for tests** |
| `--strict` | off | exit 2 immediately on the first API error instead of warning and continuing |

**Output** (grep semantics)

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
| `--show-score` | off | prefix each printed match with `p=0.97 ` (before file/line prefix) |
| `--json` | off | one JSON object per match, no other stdout output |
| `--stats` | off | totals to stderr at exit (see §3.8) |

Line format: `[p=0.97 ][file:][lineno:]text`. Context lines use `-` instead of `:` as the
separator (`file-lineno-text`), and non-adjacent groups are separated by a `--` line, exactly
like grep. Match lines are printed verbatim (lossy UTF-8), never truncated.

`--json` object: `{"file":"src/a.rs","lineno":42,"line":"...","probability":0.97,"confidence":0.9}`.
Stdin has `"file":"<stdin>"`. Context flags are ignored under `--json`, `-l`, `-L`, `-c`.

Key: read `TYPESAFE_API_KEY` from the environment, falling back to a `.env` file in the current
directory (`dotenvy`). Missing key → `jevgrep: TYPESAFE_API_KEY is not set (env or ./.env)`
on stderr, exit 2. Never log the key. `--dry-run` does not need a key.

Exit codes (grep semantics): `0` at least one line matched, `1` no line matched, `2` error.
"Error" includes: missing key, an unreadable path given on the command line, any API failure
(warned or strict), **and hitting `--max-files` or `--max-lines`** — a truncated search is not
a clean run, even if it printed matches. Unreadable files found during a directory walk are
warned about and skipped (like grep), and do not by themselves cause exit 2.

## 3. Behaviour requirements

1. **File walking = ripgrep defaults.** Use the `ignore` crate's `WalkBuilder`: respect
   `.gitignore`/`.ignore`/global gitignore, skip hidden files and dirs, skip binary files
   (NUL byte in the first 8 KiB — check per file before reading lines), follow no symlinks.
   Explicit file arguments are always searched even if ignored/hidden. Walk order: paths in
   argument order; within a directory, sorted by path (deterministic — `sort_by_file_path`).
2. **Strict output order.** File order, then line number, regardless of which API batch
   finished first. Implementation hint: build batches in order and use
   `futures::stream::iter(batches).map(judge).buffered(concurrency)` — it yields in order.
3. **Bounds are hard stops.** Count files at open time and lines at "queued for jev" time.
   When a bound is hit: stop reading, finish judging what is already queued, print those
   results, then print to stderr
   `jevgrep: stopped at --max-lines 2000 (N files not searched); raise the limit or narrow with -g`
   (or the `--max-files` variant), and exit 2. The remaining-files count is a best-effort
   count of unvisited walk entries; if unknown, say `some files not searched`.
4. **Free verdicts.** Never send to jev: blank/whitespace-only lines (verdict yes=0.0), and
   any line whose (truncated line + context) is already in the verdict cache. These lines
   still count toward output ordering and `-v`, but not toward `--max-lines`.
5. **Verdict cache.** Key = truncated line text + before + after context, per run, in memory,
   capped at 10k entries (drop oldest). Disabled by `--no-cache`.
6. **Context.** Judge-context (`--judge-context`) is what jev sees; output-context
   (`-A/-B/-C`) is what gets printed. They are independent. Judge-context never crosses file
   boundaries; the first line of a file has an empty `before`.
7. **Errors.** An API failure after retries: `jevgrep: <context>` on stderr, drop that batch
   (its lines are treated as "no verdict" — not printed, not counted as matches), continue,
   exit 2 at the end. With `--strict`, exit 2 immediately after printing what was already
   emitted. Bad key must produce the readable 401 message, never a panic or a raw JSON dump.
8. **Stats** (`--stats`, stderr, also printed on the bound-stop path):
   `jevgrep: 37 files, 1,204 lines (912 judged, 292 free), 14 matches, 57 requests, 61,340 in / 3,480 out tokens, 4.2s`.
9. **Dry run** prints per-file `lines  path` (one per line, sorted as walked) then a summary
   `N files, M lines, ~R requests` using `ceil(M / batch)`, and whether a bound would be hit.
10. **Ctrl-C** exits cleanly with what has been printed so far and `--stats` if asked; exit 2.
11. Non-UTF-8 input bytes must not crash; lossily decode. CRLF: strip a trailing `\r` for
    judging; print the line as-is.
12. Performance: a 500-file / 2000-line default run must not read more than needed — the
    reader stops at the bound, it does not read the whole tree and then truncate.

## 4. Layout

```
Cargo.toml
src/main.rs      — wiring, exit codes, Ctrl-C
src/cli.rs       — clap derive struct (three help groups as above)
src/walk.rs      — ignore::WalkBuilder wrapper, binary sniff, bound counters, stdin source
src/lines.rs     — per-file line reader producing (lineno, text, before, after) with judge-context
src/judge.rs     — TypeSafe client: request shape, retry, cache, usage totals
src/pipeline.rs  — batching + ordered concurrent judging
src/output.rs    — grep formatting (prefixes, -A/-B/-C groups, --, -l/-L/-c, --json, --stats)
tests/           — integration tests against a mock server (wiremock)
fixtures/        — small tree: text files, a binary, a .gitignore'd file, a hidden file
```

Rust 2021. `cargo build --release` warning-free, `cargo clippy -- -D warnings` clean,
`cargo fmt` applied.

```toml
[dependencies]
clap = { version = "4", features = ["derive", "env"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-std", "io-util", "fs", "time", "sync", "signal"] }
futures = "0.3"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
dotenvy = "0.15"
ignore = "0.4"
globset = "0.4"

[dev-dependencies]
wiremock = "0.6"
assert_cmd = "2"
predicates = "3"
tempfile = "3"
```

## 5. Definition of done

* `cargo build --release`, `cargo clippy -- -D warnings`, `cargo fmt --check`, `cargo test` all clean.
* Integration tests (mock server, no real API calls, no key needed — set `JEVGREP_API_URL`
  and a dummy `TYPESAFE_API_KEY`) cover: basic filtering with filename/lineno prefixes,
  auto filename rule (one file vs. directory), threshold, `-v`, `-l`/`-L`/`-c`, `-C 1` group
  formatting with `--`, `--json`, ordering under out-of-order batch completion (mock delays
  the first batch), judge-context contents in the request body (assert `before`/`after`),
  cache reuse (N identical lines → 1 request), blank lines never sent, `--max-lines` stop
  message + exit 2, `--max-files` stop + exit 2, `--dry-run` makes zero requests, gitignore
  and binary skipping, retry on 429, readable 401 message, exit codes 0/1/2, stdin input.
* README.md: what it is, install (`cargo install --path .`), examples (`jevgrep "TODOs that
  mention auth" src/`, piping, `-C`, `--dry-run` before a big search), every flag, the cost
  model (lines × context ≈ tokens; defaults bound a run to ~2000 lines ≈ 125 requests), and
  how the bounds behave.
