# Accuracy

## The fixture

`fixtures/network.log.expected.json` records which 9 of the fixture's 20 lines a person
calls "network connectivity problems", and why each one qualifies. jevgrep picks exactly
those 9 on repeated runs, and `-v` returns exactly the other 11. Scores on the matches run
0.80 to 0.96; the highest non-match is well under 0.5.

This is one sample and one query, not a benchmark. It is in the repo so you can check
that the tool and your key work, and so the tests have a realistic body to mock.

## The context experiment

While porting, one fixture line ("PPP connection dropped, retrying handshake") fell from
0.81 to 0.45 and stopped matching. It was the first candidate of the second batch, and the
only thing that had changed was that two neighbouring lines now preceded it as context
rows. The same batch was sent to the API by hand in four forms, three times each:

| request form | P(match) for that line | input tokens |
| --- | --- | --- |
| context rows + "neighbours are context only" instruction | 0.58, 0.59, 0.52 | 616 |
| context rows + plain instruction | 0.55, 0.62, 0.59 | 556 |
| no context rows + "neighbours are context only" instruction | 0.82, 0.85, 0.83 | 549 |
| no context rows + plain instruction | 0.77, 0.78, 0.81 | 489 |

The instruction wording is noise. The context rows are what move the score, by about
0.25, and they cost 26% more tokens. The neighbours in question were a log-rotation line
and a clock-sync line: on a log, adjacent lines are usually unrelated events, and jev
weighs them anyway. So `--judge-context` defaults to 0. On this crate's own source, the
default also dropped a false-positive match on a lone `}`.

## Repeatability

Strong matches are stable across runs (the fixture's 0.9+ lines move by a hundredth or
two). Borderline verdicts are noisy, roughly ±0.1: a line at 0.55 on one run can be 0.45
on the next. On 10,000 production log lines, rerunning the same input changed about 2% of
printed matches at the default threshold and about 0% at `-t 0.8`.

If you see lines flickering in and out between runs, raise `-t`. `--show-score` prints
what jev actually thought, which is usually enough to pick a threshold for your query.

## Why not a ranking

jev's docs show one `choice` question ranking all candidate lines at once. Probability
mass is then split across the lines, so a line's score depends on what else is in the
batch. On the fixture, five of the nine real matches fell below uniform. jevgrep asks an
independent yes/no per line instead.
