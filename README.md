# jevgrep

`grep` where the pattern is a sentence.

jevgrep reads files, directories, stdin, or a live stream, sends every line to
[jev](https://typesafe.ai), TypeSafe's decision model, and prints the lines jev says match
your description. Same file, two different questions:

```
$ jevgrep --show-score "network connectivity problems" fixtures/network.log
p=0.85 Sep 19 08:00:01 mbp-eric kernel[0]: wifi0 deauth reason=3 (auth expired)
p=0.89 Sep 19 08:01:02 mbp-eric networkd[78]: en0 link state changed to DOWN
p=0.95 Sep 19 08:02:00 mbp-eric networkd[78]: connection to gateway 192.168.1.1 timed out
p=0.93 Sep 19 08:03:10 mbp-eric dhclient[234]: DHCPREQUEST failed, no lease obtained on en0
p=0.94 Sep 19 08:04:20 mbp-eric networkd[78]: no route to host 8.8.8.8
p=0.88 Sep 19 08:05:33 mbp-eric resolved[67]: DNS resolution failed for api.typesafe.ai
p=0.91 Sep 19 08:06:45 mbp-eric airportd[91]: en0 disassociated from SSID HomeNet, signal lost
p=0.81 Sep 19 08:09:00 mbp-eric vpnd[144]: PPP connection dropped, retrying handshake
p=0.96 Sep 19 08:10:10 mbp-eric networkd[78]: packet loss 42% detected on en0, link unstable

$ jevgrep --show-score "a scheduled or background job running" fixtures/network.log
p=0.87 Sep 19 08:00:15 mbp-eric cron[123]: (root) CMD (run-parts /etc/cron.hourly)
p=0.96 Sep 19 08:01:30 mbp-eric backupd[456]: starting scheduled Time Machine backup
p=0.52 Sep 19 08:06:05 mbp-eric mds[89]: Spotlight metadata index rebuild started
```

Nine network lines with no keyword in common, and not one of the eleven others. Each run
cost $0.00008.

It works the same on a stream. Lines are judged as they arrive, so this prints each error
within a second of it being written:

```
tail -f app.log | jevgrep "a database or storage failure"
```

## Try it

```
cargo install jevgrep
jevgrep config set-key               # prompts for your key from https://typesafe.ai, input hidden
tail -n 200 /var/log/install.log | jevgrep -t 0.7 "an install or update that failed"   # macOS
journalctl -n 200 | jevgrep -t 0.7 "something that failed"                            # Linux
jevgrep -n "error handling that swallows the error" src/                              # code
```

Two hundred log lines is about a tenth of a cent. To reproduce the example above:

```
curl -sO https://raw.githubusercontent.com/6farer/jevgrep/main/fixtures/network.log
jevgrep --show-score "network connectivity problems" network.log
```

The key goes in `~/.config/jevgrep/config.toml`, mode 0600, and is never printed.
`TYPESAFE_API_KEY` in the environment or a `.env` file overrides it, for CI and scripts.
`jevgrep config show` tells you which one is in effect.

**Every line you filter is sent to TypeSafe.** Don't point it at files or logs you can't
share with a third party.

## When it's worth it

jev costs about **$4 per million log lines**, so jevgrep is not a replacement for `grep`.
It's for the question `grep` can't answer: *"which of these lines is the thing I'm worried
about?"* when you don't know what that thing looks like yet.

- **Watching a stream.** `tail -f`, `kubectl logs -f`, `journalctl -f` into jevgrep is an
  alert for "something a human should look at", across services with no shared error
  format. One line a second, all day, is 35 cents.
- **Investigating a window.** "What went wrong in the last hour" over 50,000 lines is
  $0.20 and thirty seconds.
- **A second stage behind a cheap first one.** `journalctl -p err | jevgrep "a
  customer-visible failure, not a retry that succeeded"`. The coarse filter cuts volume
  100×; jev does the judgment.
- **Searching code for a concept.** "a lock held across an await", "error handling that
  swallows the error". Ripgrep-style walking: respects `.gitignore`, skips binaries,
  `-g` globs. A crate of 2,500 lines is half a cent.

Wrong tool for an unfiltered firehose: 1,000 lines a second is $350 a day. Sample or
pre-filter first.

## Usage

```
jevgrep [OPTIONS] <QUERY> [PATH]...
jevgrep config (set-key | show | path)
```

`QUERY` is plain English. Paths can be files or directories. No path reads stdin when it's
a pipe, otherwise searches `.`.

```
tail -f app.log | jevgrep "authentication failures"
kubectl logs -f api | jevgrep "a request that failed for a reason the client didn't cause"

jevgrep "a stack trace or panic" app.log app.log.1
jevgrep -n "a TODO that mentions auth" src/                    # line numbers, ripgrep-style walk
jevgrep -l "reads an environment variable" .                    # just the file names
jevgrep -C 2 "where the retry loop gives up" src/               # with context lines

jevgrep -v "routine, expected, nothing to act on" app.log      # the lines that DON'T match
jevgrep -t 0.8 "a database error" app.log                       # only confident verdicts

tail -f app.log | jevgrep -t 0.8 --json "an error a human should look at" \
  | while read -r m; do curl -s -X POST "$HOOK" -d "$m"; done   # alert, forever
```

Output is grep's: `[file:][lineno:]text`, `--json` for one object per match, exit `0`
matched, `1` nothing did, `2` error. Ctrl-C prints what's in flight and exits.

| flag | default | what it does |
| --- | --- | --- |
| `-t, --threshold <P>` | `0.5` | print when P(match) ≥ P |
| `-v, --invert-match` | off | print the lines that do **not** match |
| `-n, --line-number` | off | prefix with line number |
| `-l` / `-L` / `-c` | off | files with matches / without / match counts |
| `-A/-B/-C <N>` | `0` | context lines around each match |
| `-g, --glob <GLOB>` | | only search files matching GLOB (`!GLOB` excludes) |
| `--json` | off | one object per match: `file`, `lineno`, `line`, `probability` |
| `--show-score` | off | prefix each line with `p=0.97 ` |
| `--stats` | off | files / lines / requests / tokens to stderr at exit |
| `--dry-run` | off | walk and count only, no API calls; see what a search would cost |
| `--max-lines <N>` | `2000` | stop after sending N lines (`0` = unlimited); a safety net for big trees |

Batching, timing, judge-context, bounds and the endpoint override are in
[docs/how-it-works.md](docs/how-it-works.md#full-flag-reference).

## Cost and accuracy, briefly

jev is $0.042 per million input tokens, output free. Measured with `--stats`:

| input | tokens/line | cost |
| --- | --- | --- |
| syslog-style log lines | ~95 | $4 per million lines |
| Rust source | ~55 | $2.30 per million lines |
| `tail -f`, 1 line/s, all day | | $0.35 |

JSON logs cost 6–7× more than the same events as text. Render them first
(`pino-pretty`, `jq -r '.msg'`). Details: [docs/cost.md](docs/cost.md).

Strong matches are stable across runs; borderline ones (0.4–0.6) are noisy. Raise `-t` if
you see flicker. Sending neighbouring lines as context (`--judge-context`) is off by
default: on log lines it measurably lowers scores and adds a quarter more tokens.
Details: [docs/accuracy.md](docs/accuracy.md).

## Further reading

- [docs/how-it-works.md](docs/how-it-works.md): pipeline, batching by count or by time,
  why one question per line, long-line handling, bounds, full flags, config, tests.
- [docs/cost.md](docs/cost.md): every measurement, and the JSON-versus-text experiment.
- [docs/accuracy.md](docs/accuracy.md): the fixture, the context experiment, repeatability.

## Development

```
cargo test                                  # 44 integration tests, no network, no key
cargo clippy --all-targets -- -D warnings
```

## License

MIT — see [LICENSE](LICENSE).
