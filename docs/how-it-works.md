# How jevgrep works

The README covers what you need to use jevgrep. This page is the mechanics.

## One pipeline for files and streams

1. **Source.** Paths are walked ripgrep-style (`.gitignore`, hidden files and binaries
   skipped unless asked). Each file is read and split into lines. With no path and a
   piped stdin, stdin is read line by line as it arrives. Every event (file start, line,
   file end) gets a global sequence number.
2. **Batcher.** Lines are grouped into requests. A batch closes when it has `--batch`
   lines (16), or `--max-batch-bytes` (48KB) of text, or its oldest line has waited
   `--flush-ms` (200ms), whichever comes first. Batches never span files. Blank lines are
   never sent.

   This one rule is what makes streaming automatic. `cat big.log | jevgrep` fills batches
   by count in microseconds and the timer never fires. `tail -f app.log | jevgrep` at one
   line a second sends each line alone after 200ms. A burst gets packed. The batcher does
   not know or care whether its input is live.
3. **Judge.** One request carries the query and the batch as a text block, one
   `L03| <line>` row per line, with one [noul](https://docs.typesafe.ai/primitives/noul)
   question per row. noul is jev's bare yes/no primitive: *"Does line L03 match the
   query?"* returns P(yes). TypeSafe scores each question in isolation, so a verdict never
   depends on batch-mates. Up to `-j` requests (4) are in flight at once, about 300ms each.
4. **Emitter.** Results are reordered by sequence number and handed to a streaming
   printer, so output is always in input order regardless of which batch answered first.
   The printer handles grep's `-n`, `-H`, `-A/-B/-C` with `--` separators, `-l/-L/-c`
   (printed when a file ends), and `--json`.
5. **Errors.** HTTP 429 and 529 and transport errors are retried three times with
   backoff. A 401 stops the run with "rejected the key". Any other 4xx is reported with
   the API's own detail. Without `--strict` a failed batch is skipped, its lines are
   never printed, and the run exits 2 at the end.

## Judge-context

`--judge-context N` sends N neighbouring lines around each candidate. Because a batch is a
run of consecutive lines from one file, the neighbours of most candidates are already in
the block; only the batch edges need help. The batcher keeps the last N dispatched lines
of the current file and prepends them as context rows (`  ~| text`, no question), and
holds a batch open until N further lines arrive (or the file ends, or the flush timer
fires) so the trailing edge has context too. Those trailing lines become candidates in
the next batch.

It is off by default. On the fixture, the same batch scored a borderline line 0.83
without context and 0.56 with, and the context rows cost 26% more tokens. See
[accuracy.md](accuracy.md#the-context-experiment). It is there for inputs where a line
means little on its own, such as code.

## Long lines and the per-request ceiling

The API caps tokens per request (measured: about 22k tokens accepted, 45k rejected with
`max_tokens_exceeded`). `--max-line-length` (1000 bytes) clips judged text before it is
sent; the printed line is always the original. `--max-batch-bytes` closes a batch early
on bytes. A batch the API still rejects as too large is split in half and retried,
recursively, so one huge line never costs its batch-mates their verdict.

## Bounds

`--max-files` (500) and `--max-lines` (2000) are hard stops so a careless `jevgrep q /`
can't run up a bill. Hitting one stops reading, prints what was already judged, then a
message like:

```
jevgrep: stopped at --max-lines 2000 (37 files not searched); raise the limit or narrow with -g
```

and exits 2. Pass `0` to disable. On a stream the message says `(input not fully read)`.
`--dry-run` walks and counts without calling the API, so you can see what a search would
cost first.

## Caching

Identical lines are judged once per run (`--no-cache` disables this). On logs it rarely
fires because timestamps make nearly every line unique. On code it fires for repeated
lines like `}` and `use` statements. Both the verdict cache and the `--max-lines` dedup
set are capped at 10,000 entries so a stream that runs for days does not grow memory.

## Why one question per line

jev's docs also show a single `choice` question ranking all candidate lines at once. That
is cheaper still, but it is a ranking: probability mass is split across the lines, and on
the fixture five of nine real matches fell below uniform. A filter needs independent
verdicts. One `choice` question per line, the obvious alternative to noul, gave the same
verdicts at 1.7× the tokens.

## Signals and exit codes

Ctrl-C stops reading, dispatches whatever is buffered, prints in-flight results, and
exits 2 (a run cut short is not a clean run). A second Ctrl-C exits immediately with
status 130. Exit codes otherwise follow `grep`: `0` something matched, `1` nothing did,
`2` error.

## Full flag reference

Search scope:

| flag | default | what it does |
| --- | --- | --- |
| `-g, --glob <GLOB>` | | only search files matching GLOB (repeatable; `!GLOB` excludes) |
| `--hidden` | off | search hidden files and directories |
| `--no-ignore` | off | don't respect `.gitignore` / `.ignore` |
| `--max-files <N>` | `500` | stop after opening N files (`0` = unlimited) |
| `--max-lines <N>` | `2000` | stop after sending N lines to jev (`0` = unlimited) |
| `--max-line-length <N>` | `1000` | clip judged text to N bytes (output prints the full line) |
| `--dry-run` | off | walk and count only; no API calls |

Judging:

| flag | default | what it does |
| --- | --- | --- |
| `-t, --threshold <P>` | `0.5` | match when P(yes) ≥ P |
| `-v, --invert-match` | off | print the lines that do **not** match |
| `--judge-context <N>` | `0` | neighbouring lines sent as context |
| `-b, --batch <N>` | `16` | lines per API request |
| `--max-batch-bytes <B>` | `49152` | bytes of line text per request |
| `--flush-ms <MS>` | `200` | longest a line waits for its batch to fill |
| `-j, --concurrency <N>` | `4` | requests in flight at once |
| `--no-cache` | off | don't reuse verdicts for identical lines |
| `--model <M>` | `jev-latest` | model id |
| `--timeout-ms <MS>` | `30000` | per-request timeout |
| `--api-url <URL>` | TypeSafe's | endpoint override (also `JEVGREP_API_URL`); the tests use it |
| `--strict` | off | exit 2 on the first API error instead of skipping the batch |

Output:

| flag | default | what it does |
| --- | --- | --- |
| `-n, --line-number` | off | prefix with line number |
| `-H` / `-h` | auto | always / never prefix with filename (auto: shown when more than one file) |
| `-l` / `-L` | off | only file names with / without a match |
| `-c, --count` | off | `file:count` per file |
| `-A` / `-B` / `-C <N>` | `0` | context lines after / before / both |
| `--show-score` | off | prefix each match with `p=0.97 ` |
| `--json` | off | one object per match: `file`, `lineno`, `line`, `probability` |
| `--stats` | off | totals to stderr at exit |
| `--help` | | `-h` is taken by `--no-filename` |

## Config file

`jevgrep config set-key` writes `~/.config/jevgrep/config.toml` (or
`$XDG_CONFIG_HOME/jevgrep/config.toml`) with a single `api_key` field. The directory is
created 0700 and the file 0600. Precedence: `TYPESAFE_API_KEY` in the environment, then a
`.env` in the current directory, then the config file. A config file that exists but does
not parse is an error, not silently ignored.

## Tests

```
cargo test                                  # 44 integration tests, no network, no key
cargo clippy --all-targets -- -D warnings
```

The tests run the real binary against an in-process mock of the TypeSafe endpoint
([wiremock](https://docs.rs/wiremock)). Covered: filtering, thresholds, `-v`, the
filename rule, `-l/-L/-c`, context grouping and separators, `--json`, ordering under
out-of-order responses, judge-context rows at batch edges, dedup, blank lines never sent,
`--max-lines`/`--max-files` stop messages, globs, ignore rules, `--dry-run`, 429 retry,
401 abort, exit codes, stdin, a slow stdin producing matches before EOF, batches never
spanning files, Ctrl-C on a quiet stdin, long lines (byte budget, split-on-reject,
clipping, API error detail), and the config file.
