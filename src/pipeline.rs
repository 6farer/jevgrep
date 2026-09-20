//! Channels, bounded concurrency, and ordered emit -- the streaming
//! rewrite. Flow: `source::run` -> (bounded channel) -> the batcher (this
//! module: size/bytes/flush triggers, file-boundary batches, block-form
//! judge-context via overlapping windows) -> (bounded semaphore) -> judge
//! workers -> (channel) -> ordered-emit (`output.rs`, reorders by sequence
//! number and prints incrementally).
//!
//! Judge-context, block form: a batch is a run of consecutive rows (blank
//! and candidate) from one file. Once `--batch` candidates (or
//! `--max-batch-bytes` of candidate text) have accumulated, the batch is
//! closed as soon as `--judge-context` further rows are available (or
//! `FileEnd`/the flush timer forces it early) -- those trailing rows go
//! into *this* request purely as `  ~| ` context, and are *not* removed
//! from consideration: they remain buffered and become real candidates
//! (with their own before-context taken from this same overlap) in the
//! *next* request. This is what gives every candidate correct before/after
//! context using only a single sliding window, without ever re-reading a
//! file.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{mpsc, Notify, Semaphore};

use crate::cli::Cli;
use crate::judge::{Judge, LineOutcome, Row};
use crate::output::{self, EmitItem, Printer};
use crate::source::{self, Event, StopCell};
use crate::walk;

pub struct RunOutcome {
    pub matched_any: bool,
    pub had_error: bool,
}

fn build_file_args(cli: &Cli) -> Result<Vec<walk::FileArg>> {
    walk::validate_globs(&cli.glob)?;
    walk::enumerate_files(&cli.paths, cli.hidden, cli.no_ignore, &cli.glob)
}

/// One buffered row of the sliding window: every line read (blank or
/// candidate) from the current file, in order.
struct PendingRow {
    seq: u64,
    lineno: u64,
    raw: String,
    judge_text: String,
    is_blank: bool,
    /// Already sent to jev as a real candidate in an earlier dispatch.
    /// Retained rows with `asked == true` only ever appear as `  ~| `
    /// context text from here on.
    asked: bool,
}

/// Shared handles needed by the batcher/dispatch/worker stages.
struct Ctx {
    judge: Arc<Judge>,
    query: Arc<String>,
    threshold: f64,
    invert_match: bool,
    batch_size: usize,
    max_batch_bytes: usize,
    flush_ms: u64,
    judge_context: usize,
    max_line_length: usize,
    no_cache: bool,
    strict: bool,
    max_lines: usize,
    tx_emit: mpsc::Sender<(u64, EmitItem)>,
    had_error: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    semaphore: Arc<Semaphore>,
    stop_cell: Arc<StopCell>,
    remaining_files: Arc<AtomicUsize>,
    seq: Arc<AtomicU64>,
}

pub async fn run(cli: &Cli, judge: Arc<Judge>, shutdown: Arc<AtomicBool>) -> Result<RunOutcome> {
    let start = Instant::now();
    let query = cli
        .query
        .clone()
        .expect("query presence validated in main before pipeline::run is called");

    let multi = walk::compute_multi(&cli.paths)?;
    let file_args = build_file_args(cli)?;

    let show_filename = if cli.with_filename {
        true
    } else if cli.no_filename {
        false
    } else {
        multi
    };
    let (before_ctx, after_ctx) = cli.output_context();
    let fmt_params = output::FormatParams {
        show_filename,
        line_number: cli.line_number,
        show_score: cli.show_score,
        before_ctx,
        after_ctx,
        json: cli.json,
        files_with_matches: cli.files_with_matches,
        files_without_match: cli.files_without_match,
        count: cli.count,
    };

    let shutdown_notify = Arc::new(Notify::new());
    let had_error = Arc::new(AtomicBool::new(false));
    let stop_cell = Arc::new(StopCell::new());

    // First Ctrl-C: stop accepting new input, drain in-flight work. Second
    // Ctrl-C: give up on draining and exit now (a blocking stdin read can't
    // otherwise be interrupted).
    {
        let shutdown = shutdown.clone();
        let shutdown_notify = shutdown_notify.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown.store(true, Ordering::Relaxed);
                shutdown_notify.notify_one();
            }
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = std::io::Write::flush(&mut std::io::stdout());
                std::process::exit(130);
            }
        });
    }

    let line_channel_cap = (cli.batch.max(1) * cli.concurrency.max(1) * 2).max(16);
    let (tx_lines, rx_lines) = mpsc::channel::<(u64, Event)>(line_channel_cap);
    let emit_channel_cap = (cli.batch.max(1) * cli.concurrency.max(1) * 4).max(64);
    let (tx_emit, mut rx_emit) = mpsc::channel::<(u64, EmitItem)>(emit_channel_cap);

    let seq = Arc::new(AtomicU64::new(0));
    let remaining_files = Arc::new(AtomicUsize::new(0));
    let source_handle = {
        let shutdown = shutdown.clone();
        let had_error = had_error.clone();
        let stop_cell = stop_cell.clone();
        let max_files = cli.max_files;
        let seq = seq.clone();
        let tx_emit = tx_emit.clone();
        let remaining_files = remaining_files.clone();
        tokio::spawn(async move {
            source::run(
                file_args,
                max_files,
                tx_lines,
                tx_emit,
                seq,
                shutdown,
                stop_cell,
                had_error,
                remaining_files,
            )
            .await;
        })
    };

    let ctx = Arc::new(Ctx {
        judge: judge.clone(),
        query: Arc::new(query),
        threshold: cli.threshold,
        invert_match: cli.invert_match,
        batch_size: cli.batch.max(1),
        max_batch_bytes: cli.max_batch_bytes.max(1),
        flush_ms: cli.flush_ms,
        judge_context: cli.judge_context,
        max_line_length: cli.max_line_length,
        no_cache: cli.no_cache,
        strict: cli.strict,
        max_lines: cli.max_lines,
        tx_emit,
        had_error: had_error.clone(),
        shutdown: shutdown.clone(),
        shutdown_notify,
        semaphore: Arc::new(Semaphore::new(cli.concurrency.max(1))),
        stop_cell,
        remaining_files,
        seq,
    });

    let batcher_handle = tokio::spawn(run_batcher(rx_lines, ctx));

    let mut next_expected: u64 = 0;
    let mut pending: BTreeMap<u64, EmitItem> = BTreeMap::new();
    let mut printer = Printer::new(fmt_params);
    let mut total_matches: u64 = 0;

    while let Some((seq, item)) = rx_emit.recv().await {
        pending.insert(seq, item);
        while let Some(item) = pending.remove(&next_expected) {
            total_matches += printer.handle(item);
            next_expected += 1;
        }
    }

    source_handle.abort();
    let _ = source_handle.await;
    let _ = batcher_handle.await;

    printer.finish();

    if cli.stats {
        let usage = judge.usage();
        output::print_stats(
            printer.files_seen,
            printer.lines_seen,
            printer.judged,
            printer.free,
            total_matches,
            usage.requests,
            usage.input_tokens,
            usage.output_tokens,
            start.elapsed().as_secs_f64(),
        );
    }

    Ok(RunOutcome {
        matched_any: total_matches > 0,
        had_error: had_error.load(Ordering::Relaxed),
    })
}

/// Per-file sliding-window state.
struct FileWindow {
    buffer: Vec<PendingRow>,
}

impl FileWindow {
    fn new() -> Self {
        FileWindow { buffer: Vec::new() }
    }

    /// Rows still eligible to become a real candidate: not blank, and not
    /// already asked in an earlier dispatch (a row that was asked can
    /// still be sitting in the buffer, retained purely to serve as a
    /// context row for its neighbours).
    fn candidate_count(&self) -> usize {
        self.buffer
            .iter()
            .filter(|r| !r.is_blank && !r.asked)
            .count()
    }

    fn candidate_bytes(&self) -> usize {
        self.buffer
            .iter()
            .filter(|r| !r.is_blank && !r.asked)
            .map(|r| r.judge_text.len())
            .sum()
    }

    fn ready_to_close(&self, batch_size: usize, max_bytes: usize) -> bool {
        let cc = self.candidate_count();
        cc > 0 && (cc >= batch_size || self.candidate_bytes() >= max_bytes)
    }

    /// Returns `(cand_end, cut)`: `buffer[..cand_end]` (excluding rows
    /// already `asked`) are the rows that become real candidates in this
    /// dispatch; `buffer[cand_end..cut]` are extra rows included purely as
    /// trailing context. `None` means "not ready yet" (only when `force`
    /// is false).
    fn compute_cut(
        &self,
        batch_size: usize,
        judge_context: usize,
        force: bool,
    ) -> Option<(usize, usize)> {
        let cc = self.candidate_count();
        if cc == 0 {
            return None;
        }
        let want = cc.min(batch_size);
        let mut seen = 0usize;
        let mut cand_end = None;
        for (i, r) in self.buffer.iter().enumerate() {
            if !r.is_blank && !r.asked {
                seen += 1;
                if seen == want {
                    cand_end = Some(i + 1);
                    break;
                }
            }
        }
        let cand_end = cand_end?;
        let available = self.buffer.len() - cand_end;
        if available < judge_context && !force {
            return None;
        }
        let cut = (cand_end + judge_context).min(self.buffer.len());
        Some((cand_end, cut))
    }
}

async fn run_batcher(mut rx_lines: mpsc::Receiver<(u64, Event)>, ctx: Arc<Ctx>) {
    let mut window = FileWindow::new();
    let mut deadline: Option<Instant> = None;
    let mut seen_texts: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queued_lines: u64 = 0;
    // Set only when *this batcher* decides to stop accepting new
    // candidates (an in-progress --max-lines trip). Deliberately separate
    // from `ctx.shutdown`: that flag can already be true because of a
    // completely unrelated, earlier-in-the-file-list --max-files trip (set
    // by the source) or a fatal/strict worker error -- neither of those
    // means "drop the line currently in hand", which may well be one that
    // was legitimately queued before the trip.
    let mut max_lines_tripped = false;
    let flush_after = Duration::from_millis(ctx.flush_ms.max(1));

    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;

            maybe = rx_lines.recv() => {
                match maybe {
                    Some((seq, event)) => {
                        match event {
                            Event::FileStart { label, .. } => {
                                // A file boundary always starts a fresh window.
                                close_and_dispatch(&mut window, &ctx, true).await;
                                deadline = None;
                                if ctx.tx_emit.send((seq, EmitItem::FileStart { label })).await.is_err() {
                                    return;
                                }
                            }
                            Event::FileEnd { .. } => {
                                close_and_dispatch(&mut window, &ctx, true).await;
                                deadline = None;
                                if ctx.tx_emit.send((seq, EmitItem::FileEnd)).await.is_err() {
                                    return;
                                }
                            }
                            Event::Line { lineno, raw, .. } => {
                                if max_lines_tripped {
                                    // This batcher already decided to stop
                                    // accepting new candidates. The channel
                                    // is still drained normally by the rest
                                    // of this loop so that already-buffered
                                    // FileStart/FileEnd events for files
                                    // still in flight are never lost. This
                                    // seq was already allocated by the
                                    // source, so something must still be
                                    // sent for it or the reorder buffer
                                    // downstream would stall forever on the
                                    // gap.
                                    if ctx.tx_emit.send((seq, EmitItem::Skip)).await.is_err() {
                                        return;
                                    }
                                    continue;
                                }

                                let judge_text = crate::lines::judge_text(&raw, ctx.max_line_length);
                                let is_blank = judge_text.trim().is_empty();

                                if is_blank {
                                    // Free verdict: known instantly, no API
                                    // call, no queueing toward --max-lines.
                                    let is_match = if ctx.invert_match { 0.0 < ctx.threshold } else { 0.0 >= ctx.threshold };
                                    let verdict = if is_match { Some(0.0) } else { None };
                                    if ctx.tx_emit.send((seq, EmitItem::Line { lineno, raw: raw.clone(), verdict, judged: false })).await.is_err() {
                                        return;
                                    }
                                } else {
                                    // Approximate "dups excluded" for
                                    // --max-lines counting: identical judge
                                    // text (ignoring context) only counts
                                    // once. The judge's own cache/dedup
                                    // still keys on the richer (before,
                                    // line, after) signature at dispatch
                                    // time; this is only for the bound.
                                    let is_new_for_bound = ctx.no_cache || seen_texts.insert(judge_text.clone());
                                    if is_new_for_bound && ctx.max_lines != 0 && queued_lines >= ctx.max_lines as u64 {
                                        max_lines_tripped = true;
                                        if ctx.stop_cell.set_once("--max-lines", ctx.max_lines) {
                                            ctx.shutdown.store(true, Ordering::Relaxed);
                                            ctx.had_error.store(true, Ordering::Relaxed);
                                            let remaining = ctx.remaining_files.load(Ordering::Relaxed);
                                            let n = ctx.seq.fetch_add(1, Ordering::SeqCst);
                                            let _ = ctx.tx_emit.send((n, EmitItem::Stopped {
                                                flag: "--max-lines",
                                                limit: ctx.max_lines,
                                                files_not_searched: Some(remaining),
                                            })).await;
                                        } else {
                                            ctx.shutdown.store(true, Ordering::Relaxed);
                                        }
                                        // This line itself is dropped
                                        // (never queued, never shown),
                                        // matching the old plan pass. Flush
                                        // whatever was already properly
                                        // queued before it, then accept
                                        // nothing further for this file.
                                        close_and_dispatch(&mut window, &ctx, true).await;
                                        deadline = None;
                                        if ctx.tx_emit.send((seq, EmitItem::Skip)).await.is_err() {
                                            return;
                                        }
                                        continue;
                                    }
                                    if is_new_for_bound {
                                        queued_lines += 1;
                                    }
                                }

                                if window.buffer.is_empty() {
                                    deadline = Some(Instant::now() + flush_after);
                                }
                                window.buffer.push(PendingRow { seq, lineno, raw, judge_text, is_blank, asked: false });
                                if is_blank && window.candidate_count() == 0
                                    && window.buffer.len() > ctx.judge_context
                                {
                                    // Cap leading blank runs before any
                                    // candidate has arrived, so an
                                    // arbitrarily long quiet stretch
                                    // doesn't grow the window forever (and,
                                    // with --judge-context 0, ensures no
                                    // stray context row ever survives to
                                    // be sent).
                                    let drop_n = window.buffer.len() - ctx.judge_context;
                                    window.buffer.drain(0..drop_n);
                                }

                                while window.ready_to_close(ctx.batch_size, ctx.max_batch_bytes) {
                                    let Some((cand_end, cut)) = window.compute_cut(ctx.batch_size, ctx.judge_context, false) else { break };
                                    dispatch_window(&mut window, &ctx, cand_end, cut).await;
                                    deadline = if window.buffer.is_empty() { None } else { Some(Instant::now() + flush_after) };
                                }
                            }
                        }
                    }
                    None => {
                        close_and_dispatch(&mut window, &ctx, true).await;
                        break;
                    }
                }
            }

            _ = sleep, if deadline.is_some() => {
                close_and_dispatch(&mut window, &ctx, false).await;
                deadline = if window.buffer.is_empty() { None } else { Some(Instant::now() + flush_after) };
            }

            // Ctrl-C (or a --strict/fatal worker error) explicitly wakes us
            // here to stop waiting on the channel right now, since the
            // source may be stuck in an uninterruptible blocking read (a
            // quiet stdin). --max-files/--max-lines don't notify: they let
            // the channel drain and close naturally, which is what lets
            // every already-buffered file event (and the direct-sent
            // `Stopped` notice) survive instead of racing this exit.
            _ = ctx.shutdown_notify.notified() => {
                close_and_dispatch(&mut window, &ctx, true).await;
                break;
            }
        }
    }
}

/// Force-close whatever is buffered (used for FileStart/FileEnd/EOF/
/// shutdown). `clear_after` additionally drops any leftover rows -- used
/// whenever the window will not continue (file boundary, shutdown): a mid-
/// batch flush-timer close instead keeps the sliding-window overlap.
async fn close_and_dispatch(window: &mut FileWindow, ctx: &Arc<Ctx>, clear_after: bool) {
    while let Some((cand_end, cut)) = window.compute_cut(ctx.batch_size, ctx.judge_context, true) {
        dispatch_window(window, ctx, cand_end, cut).await;
    }
    if clear_after {
        window.buffer.clear();
    }
}

/// Dispatch `window.buffer[..cut]` as one request: `[..cand_end]` are real
/// candidates (asked, with a cache key computed from their neighbours
/// *within this slice*), `[cand_end..cut]` are trailing context only.
/// `window.buffer[cand_end..]` is retained (the overlap that lets the next
/// request's leading rows double as before-context, and lets any
/// non-blank rows among them become real candidates next time).
async fn dispatch_window(window: &mut FileWindow, ctx: &Arc<Ctx>, cand_end: usize, cut: usize) {
    let slice = &window.buffer[..cut];

    let mut rows: Vec<Row> = Vec::with_capacity(cut);
    let mut targets: Vec<(u64, u64, String)> = Vec::new(); // (seq, lineno, raw) per candidate, in row order

    for (i, r) in slice.iter().enumerate() {
        if !r.is_blank && !r.asked && i < cand_end {
            let before: Vec<&str> = slice[i.saturating_sub(ctx.judge_context)..i]
                .iter()
                .map(|p| p.judge_text.as_str())
                .collect();
            let after_end = (i + 1 + ctx.judge_context).min(slice.len());
            let after: Vec<&str> = slice[i + 1..after_end]
                .iter()
                .map(|p| p.judge_text.as_str())
                .collect();
            let cache_key = if ctx.no_cache {
                None
            } else {
                Some(serde_json::to_string(&(&before, &r.judge_text, &after)).unwrap_or_default())
            };
            rows.push(Row {
                text: r.judge_text.clone(),
                candidate: true,
                cache_key,
            });
            targets.push((r.seq, r.lineno, r.raw.clone()));
        } else {
            rows.push(Row {
                text: r.judge_text.clone(),
                candidate: false,
                cache_key: None,
            });
        }
    }

    if targets.is_empty() {
        return; // pure-context slice; nothing to ask (shouldn't normally happen)
    }

    // Mark the rows just asked so they're never re-asked if they remain in
    // the buffer purely for context. Then drop everything more than
    // `judge_context` rows before the cut point -- close enough to serve
    // no further purpose -- and retain the rest (which may include some
    // now-`asked` rows, kept only for their text) as the start of the next
    // window.
    for r in window.buffer[..cand_end].iter_mut() {
        if !r.is_blank {
            r.asked = true;
        }
    }
    let drain_start = cand_end.saturating_sub(ctx.judge_context);
    window.buffer.drain(0..drain_start);

    let permit = match ctx.semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return,
    };
    let ctx = ctx.clone();
    let query = ctx.query.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let outcomes = ctx.judge.judge_rows(&query, &rows).await;
        let mut printed_batch_err = false;

        for (pos, (seq, lineno, raw)) in targets.into_iter().enumerate() {
            let item = match outcomes.get(&pos) {
                Some(Ok(LineOutcome::Verdict(v))) => {
                    let is_match = if ctx.invert_match {
                        v.probability < ctx.threshold
                    } else {
                        v.probability >= ctx.threshold
                    };
                    EmitItem::Line {
                        lineno,
                        raw,
                        verdict: if is_match { Some(v.probability) } else { None },
                        judged: true,
                    }
                }
                Some(Ok(LineOutcome::MissingAnswer)) => {
                    eprintln!("jevgrep: no answer from TypeSafe API for line {lineno}");
                    ctx.had_error.store(true, Ordering::Relaxed);
                    if ctx.strict {
                        ctx.shutdown.store(true, Ordering::Relaxed);
                        ctx.shutdown_notify.notify_one();
                    }
                    EmitItem::Line {
                        lineno,
                        raw,
                        verdict: None,
                        judged: true,
                    }
                }
                Some(Err(e)) => {
                    if !printed_batch_err {
                        eprintln!("jevgrep: {e}");
                        printed_batch_err = true;
                    }
                    ctx.had_error.store(true, Ordering::Relaxed);
                    if ctx.strict || e.fatal {
                        ctx.shutdown.store(true, Ordering::Relaxed);
                        ctx.shutdown_notify.notify_one();
                    }
                    EmitItem::Line {
                        lineno,
                        raw,
                        verdict: None,
                        judged: true,
                    }
                }
                None => EmitItem::Line {
                    lineno,
                    raw,
                    verdict: None,
                    judged: true,
                },
            };
            if ctx.tx_emit.send((seq, item)).await.is_err() {
                break;
            }
        }
    });
}

/// `--dry-run`: walk and count only, no API calls. For a path search this
/// mirrors the old bound preview; reading stdin under `--dry-run` reads all
/// of stdin (there is nothing to "preview" about a live pipe) and counts.
pub fn dry_run(cli: &Cli) -> Result<()> {
    let file_args = build_file_args(cli)?;
    let batch = cli.batch.max(1);

    let mut total_files = 0usize;
    let mut total_lines = 0usize;
    let mut would_stop: Option<(&'static str, usize)> = None;

    for arg in &file_args {
        let bytes = match arg {
            walk::FileArg::Stdin => {
                use std::io::Read;
                let mut buf = Vec::new();
                match std::io::stdin().read_to_end(&mut buf) {
                    Ok(_) => buf,
                    Err(e) => {
                        eprintln!("jevgrep: stdin: {e}");
                        continue;
                    }
                }
            }
            walk::FileArg::Path { path, .. } => match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("jevgrep: {}: {}", path.display(), e);
                    continue;
                }
            },
        };
        if walk::looks_binary(&bytes) {
            continue;
        }
        let n = crate::lines::split_lines(&bytes).len();
        total_files += 1;
        total_lines += n;
        output::print_dry_run_line(n, &arg.label());

        if would_stop.is_none() {
            if cli.max_files != 0 && total_files > cli.max_files {
                would_stop = Some(("--max-files", cli.max_files));
            } else if cli.max_lines != 0 && total_lines > cli.max_lines {
                would_stop = Some(("--max-lines", cli.max_lines));
            }
        }
    }

    let requests = total_lines.div_ceil(batch);
    output::print_dry_run_summary(total_files, total_lines, requests);
    if let Some((flag, limit)) = would_stop {
        eprintln!("jevgrep: would stop at {flag} {limit} (bound would be hit)");
    }

    Ok(())
}
