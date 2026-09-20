//! Live line source: walks files (via `walk.rs`) or streams stdin line by
//! line, emitting an ordered, seq-tagged event stream over a bounded
//! channel. This is what lets `tail -f app.log | jevgrep query` print
//! matches as they happen instead of only after EOF: stdin is read as it
//! arrives, not slurped whole before anything is judged.
//!
//! Every event gets a globally increasing `seq` at the moment it is
//! produced, so the batcher/emitter downstream can restore strict order
//! even though judging happens out of order across concurrent batches.
//!
//! The final `Stopped` notice (when `--max-files`/`--max-lines` trips) is
//! sent straight to the emitter, not relayed through the batcher: whoever
//! *first* claims the trip (via `StopCell::set_once`) is the one who sends
//! it, so it can never be lost to the race between "the batcher notices
//! `shutdown` and exits immediately" (needed so Ctrl-C on a stuck stdin
//! read still exits promptly) and "the source is still on its way to
//! reporting how it stopped".

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::Sender;

use crate::output::EmitItem;
use crate::walk::{self, FileArg};

#[derive(Debug, Clone)]
pub enum Event {
    FileStart { label: String },
    Line { lineno: u64, raw: String },
    FileEnd,
}

/// First-stop-wins record of which bound tripped. Whichever side (source,
/// for `--max-files`; batcher, for `--max-lines`) wins `set_once` is the
/// one responsible for sending the `Stopped` emit item itself.
#[derive(Default)]
pub struct StopCell(Mutex<Option<(&'static str, usize)>>);

impl StopCell {
    pub fn new() -> Self {
        StopCell(Mutex::new(None))
    }

    /// Returns `true` if this call is the one that recorded the reason
    /// (first stop wins) -- the caller should send the `Stopped` event only
    /// when this returns `true`, so it is never sent twice.
    pub fn set_once(&self, flag: &'static str, limit: usize) -> bool {
        let mut g = self.0.lock().unwrap();
        if g.is_some() {
            return false;
        }
        *g = Some((flag, limit));
        true
    }
}

fn strip_lf(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    line
}

fn decode(bytes: Vec<u8>) -> String {
    // Non-UTF-8 input bytes must not crash; lossily decode.
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn send(tx: &Sender<(u64, Event)>, seq: &AtomicU64, event: Event) -> Result<(), ()> {
    let n = seq.fetch_add(1, Ordering::SeqCst);
    tx.send((n, event)).await.map_err(|_| ())
}

async fn send_stopped(
    tx_emit: &Sender<(u64, EmitItem)>,
    seq: &AtomicU64,
    flag: &'static str,
    limit: usize,
    files_not_searched: Option<usize>,
) {
    let n = seq.fetch_add(1, Ordering::SeqCst);
    let _ = tx_emit
        .send((
            n,
            EmitItem::Stopped {
                flag,
                limit,
                files_not_searched,
            },
        ))
        .await;
}

/// Run every file argument in order (a mix of real files and `-`/no-path
/// stdin entries, as `walk::enumerate_files` already produces), stopping
/// early on `shutdown` or `--max-files`. `max_files == 0` means unlimited.
/// `had_error` is set for a warned-and-skipped unreadable file discovered
/// during a directory walk, or a hard error for an explicit unreadable
/// path. `remaining_files` is kept up to date (files strictly after the
/// one currently being read) so the batcher can give a best-effort
/// `files_not_searched` count if *it* is the one that trips `--max-lines`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    file_args: Vec<FileArg>,
    max_files: usize,
    tx: Sender<(u64, Event)>,
    tx_emit: Sender<(u64, EmitItem)>,
    seq: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    stop_cell: Arc<StopCell>,
    had_error: Arc<AtomicBool>,
    remaining_files: Arc<AtomicUsize>,
) {
    let total = file_args.len();
    let mut files_opened: usize = 0;

    for (idx, arg) in file_args.into_iter().enumerate() {
        if shutdown.load(Ordering::Relaxed) {
            // Someone else (the batcher, on --max-lines, or Ctrl-C) already
            // tripped -- or didn't need to send anything for. Just stop.
            return;
        }

        if max_files != 0 && files_opened >= max_files {
            let remaining = total - idx;
            if stop_cell.set_once("--max-files", max_files) {
                shutdown.store(true, Ordering::Relaxed);
                had_error.store(true, Ordering::Relaxed);
                send_stopped(&tx_emit, &seq, "--max-files", max_files, Some(remaining)).await;
            } else {
                shutdown.store(true, Ordering::Relaxed);
            }
            return;
        }
        remaining_files.store(total - idx - 1, Ordering::Relaxed);

        let explicit = matches!(arg, FileArg::Path { explicit: true, .. });
        let label = arg.label();

        match arg {
            FileArg::Stdin => {
                files_opened += 1;
                if run_stdin(&label, &tx, &seq, &shutdown).await.is_err() {
                    return;
                }
            }
            FileArg::Path { path, .. } => {
                let read_result = {
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || std::fs::read(&path)).await
                };
                let bytes = match read_result {
                    Ok(Ok(b)) => b,
                    Ok(Err(e)) => {
                        eprintln!("jevgrep: {}: {}", path.display(), e);
                        if explicit {
                            had_error.store(true, Ordering::Relaxed);
                            shutdown.store(true, Ordering::Relaxed);
                            return;
                        }
                        continue;
                    }
                    Err(_) => {
                        // The blocking task panicked; treat like an
                        // unreadable file rather than crashing the source.
                        eprintln!("jevgrep: {}: internal read error", path.display());
                        if explicit {
                            had_error.store(true, Ordering::Relaxed);
                            shutdown.store(true, Ordering::Relaxed);
                            return;
                        }
                        continue;
                    }
                };
                files_opened += 1;
                if walk::looks_binary(&bytes) {
                    continue;
                }

                if send(
                    &tx,
                    &seq,
                    Event::FileStart {
                        label: label.clone(),
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
                let lines = crate::lines::split_lines(&bytes);
                for (i, raw) in lines.into_iter().enumerate() {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let lineno = (i + 1) as u64;
                    if send(&tx, &seq, Event::Line { lineno, raw }).await.is_err() {
                        return;
                    }
                }
                if send(&tx, &seq, Event::FileEnd).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Stream stdin line by line as it arrives (a blocking read, driven async by
/// tokio's `io-std` support), emitting each line the moment it is read --
/// this is what makes a live pipe (`tail -f x | jevgrep q`) print matches
/// before the producer finishes, rather than only after EOF.
async fn run_stdin(
    label: &str,
    tx: &Sender<(u64, Event)>,
    seq: &Arc<AtomicU64>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), ()> {
    send(
        tx,
        seq,
        Event::FileStart {
            label: label.to_string(),
        },
    )
    .await?;

    let mut stdin = tokio::io::stdin();
    let mut lineno: u64 = 0;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let n = match stdin.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = pending.drain(..=pos).collect();
            let line = strip_lf(raw);
            lineno += 1;
            send(
                tx,
                seq,
                Event::Line {
                    lineno,
                    raw: decode(line),
                },
            )
            .await?;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }
    }

    // A final line with no trailing newline is still a line; don't drop it.
    if !pending.is_empty() {
        lineno += 1;
        let line = std::mem::take(&mut pending);
        let _ = send(
            tx,
            seq,
            Event::Line {
                lineno,
                raw: decode(line),
            },
        )
        .await;
    }

    send(tx, seq, Event::FileEnd).await
}
