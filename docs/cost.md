# Cost

jev is priced at **$0.042 per million input tokens; output tokens are free**
([typesafe.ai](https://typesafe.ai),
[announcement](https://typesafe.ai/blog/introducing-system-one-models-and-jev)).
Each line costs its own text plus a short per-line question. Everything below was
measured with `--stats` on real input.

## Measurements

| input | lines | tokens/line | cost |
| --- | --- | --- | --- |
| `fixtures/network.log` (syslog) | 20 | 97 | $0.00008 |
| `tail -n 200 /var/log/install.log` (macOS) | 200 | 92 | $0.0008 |
| this crate's `src/` (Rust) | 2,512 judged | 54 | $0.0057 |
| same, `--judge-context 2` | 2,512 judged | 65 | $0.0068 |
| Vercel runtime logs, production | 559 | 75 | $0.0018 |
| pino JSON, Azure Container Apps, production | 10,000 | 561 | $0.24 |

The last two were measured with jevtail, jevgrep's predecessor, which used the same
request shape.

Rule of thumb for plain text logs: **about 90 tokens per line, about $4 per million
lines.** Code is cheaper, about 55 tokens per line, because lines are short.

| volume | cost |
| --- | --- |
| 1,000 log lines | $0.004 |
| 100,000 log lines | $0.40 |
| 1,000,000 log lines | $4 |
| `tail -f`, 1 line/s, all day | $0.35/day |
| `tail -f`, 10 lines/s, all day | $3.50/day |
| `tail -f`, 1,000 lines/s, all day | $350/day |

That last row is why jevgrep is the wrong tool for an unfiltered firehose. Sample or
pre-filter first (`journalctl -p err | jevgrep ...`, `grep -v healthcheck | jevgrep ...`).

## JSON logs: render them first

Structured JSON lines (pino, bunyan, zap) are 6–7× more expensive than the same events
rendered as text, and the extra tokens are `pid`, `hostname`, `time` and framework
metadata that carry no signal. On the same 10,000 Azure lines:

| form | tokens/line | 10k lines | matches at p ≥ 0.8 |
| --- | --- | --- | --- |
| raw pino JSON | 561 | $0.24 | 527 |
| `time [LEVEL] msg url status` | 76 | $0.03 | 588 |

The rendered form found the same matches plus a few that the JSON noise had buried. So:
`... | pino-pretty | jevgrep "..."`, or `jq -r '"\(.levelName) \(.msg)"'`, or whatever
your logger's text renderer is.

## What the redesign saved

jevgrep's first version sent each line with two lines before and two after as separate
copies, and measured 170 input tokens per judged line on this crate's `src/`. The current
design sends a batch as one contiguous block and asks a yes/no question per row, at 54
tokens per line: 3.1× cheaper, and more accurate on logs (see
[accuracy.md](accuracy.md)).

## What doesn't help much

- The identical-line cache: nearly every real log line is unique because of its
  timestamp. It does fire on code (`}`, `use` lines, blank-adjacent boilerplate).
- Smaller batches: batching amortizes the HTTP request, not the tokens. A line costs the
  same alone or in a batch of 16.
