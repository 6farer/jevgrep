//! Wires the plan pass (`lines::plan`) to the judge pass (batched, ordered,
//! concurrent calls to `Judge`) and to output. Batches are built from the
//! *whole* run's `Ask` records (not per file), so a batch can span files;
//! output is still emitted strictly in file order by tracking, for each
//! file, the last batch its lines depend on and only emitting once that
//! batch has resolved.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::stream::{self, StreamExt};

use crate::cli::Cli;
use crate::judge::{AskItem, Judge, LineOutcome};
use crate::lines::{self, FileMeta, PlanParams, RecordKind, TruncKind, Verdict};
use crate::output::{self, FileLines, FormatParams, Printer};
use crate::walk;

pub struct RunOutcome {
    pub matched_any: bool,
    pub had_error: bool,
}

fn plan_params(cli: &Cli) -> PlanParams {
    PlanParams {
        judge_context: cli.judge_context,
        max_files: cli.max_files,
        max_lines: cli.max_lines,
        max_line_length: cli.max_line_length,
        no_cache: cli.no_cache,
    }
}

fn build_file_args(cli: &Cli) -> Result<Vec<walk::FileArg>> {
    walk::validate_globs(&cli.glob)?;
    walk::enumerate_files(&cli.paths, cli.hidden, cli.no_ignore, &cli.glob)
}

pub async fn run(cli: &Cli, judge: Arc<Judge>, shutdown: Arc<AtomicBool>) -> Result<RunOutcome> {
    let start = Instant::now();
    let multi = walk::compute_multi(&cli.paths)?;
    let file_args = build_file_args(cli)?;
    let plan = lines::plan(file_args, &plan_params(cli))?;

    let show_filename = if cli.with_filename {
        true
    } else if cli.no_filename {
        false
    } else {
        multi
    };
    let (before_ctx, after_ctx) = cli.output_context();
    let fmt_params = FormatParams {
        show_filename,
        line_number: cli.line_number,
        show_score: cli.show_score,
        before_ctx,
        after_ctx,
    };

    // Contiguous [start, end) record ranges per file, since `plan.records`
    // is built file-by-file, line-by-line, in order. Files with zero
    // records (e.g. an empty file) simply keep the default (0, 0) range.
    let mut ranges_by_file: Vec<(usize, usize)> = vec![(0, 0); plan.files.len()];
    {
        let mut i = 0usize;
        while i < plan.records.len() {
            let fi = plan.records[i].file_idx;
            let mut j = i;
            while j < plan.records.len() && plan.records[j].file_idx == fi {
                j += 1;
            }
            ranges_by_file[fi] = (i, j);
            i = j;
        }
    }

    // Ask records, in run order, chunked into batches; and, for every key
    // (Ask-owned or Dup-referenced), which batch resolves it.
    let ask_indices: Vec<usize> = plan
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.kind, RecordKind::Ask { .. }))
        .map(|(i, _)| i)
        .collect();
    let batch_size = cli.batch.max(1);
    let batches: Vec<Vec<usize>> = ask_indices.chunks(batch_size).map(|c| c.to_vec()).collect();

    let mut key_batch: HashMap<&str, usize> = HashMap::new();
    for (bidx, chunk) in batches.iter().enumerate() {
        for &ri in chunk {
            if let RecordKind::Ask { key, .. } = &plan.records[ri].kind {
                key_batch.insert(key.as_str(), bidx);
            }
        }
    }

    // Per-file readiness: the batch index after which every line in this
    // file (its own Ask records, and any Dup records pointing at an Ask
    // elsewhere in the run) is resolvable. `None` = no dependency at all.
    let mut ready_after: Vec<Option<usize>> = vec![None; plan.files.len()];
    for r in &plan.records {
        let dep = match &r.kind {
            RecordKind::Ask { key, .. } => key_batch.get(key.as_str()).copied(),
            RecordKind::Dup { key } => key_batch.get(key.as_str()).copied(),
            RecordKind::Free(_) => None,
        };
        if let Some(d) = dep {
            let slot = &mut ready_after[r.file_idx];
            *slot = Some(slot.map_or(d, |cur| cur.max(d)));
        }
    }

    let query = cli.query.as_str();
    let judge_ref: &Judge = judge.as_ref();
    let records_ref = &plan.records;

    let mut verdict_by_key: HashMap<&str, Verdict> = HashMap::new();
    let mut had_error = false;
    let mut total_matches: u64 = 0u64;
    let mut next_file_to_emit = 0usize;
    let mut printer = Printer::new();
    let mut stop_now = false;

    let emit_ready = |next_file_to_emit: &mut usize,
                      processed_through: Option<usize>,
                      verdict_by_key: &HashMap<&str, Verdict>,
                      printer: &mut Printer,
                      total_matches: &mut u64| {
        while *next_file_to_emit < plan.files.len() {
            let ready = match ready_after[*next_file_to_emit] {
                None => true,
                Some(need) => matches!(processed_through, Some(p) if need <= p),
            };
            if !ready {
                break;
            }
            let fi = *next_file_to_emit;
            emit_file(
                fi,
                &plan.files[fi],
                &records_ref[ranges_by_file[fi].0..ranges_by_file[fi].1],
                verdict_by_key,
                cli,
                &fmt_params,
                printer,
                total_matches,
            );
            *next_file_to_emit += 1;
        }
    };

    // Files with no dependency on any batch (e.g. entirely blank) can be
    // emitted before any batch has run.
    emit_ready(
        &mut next_file_to_emit,
        None,
        &verdict_by_key,
        &mut printer,
        &mut total_matches,
    );

    let mut result_stream = stream::iter(batches.iter().enumerate())
        .map(|(bidx, chunk)| {
            let items: Vec<AskItem> = chunk
                .iter()
                .map(|&ri| match &records_ref[ri].kind {
                    RecordKind::Ask {
                        key,
                        before,
                        line,
                        after,
                    } => AskItem {
                        key,
                        before,
                        line,
                        after,
                    },
                    _ => unreachable!("batch only contains Ask records"),
                })
                .collect();
            async move {
                let outcome = judge_ref.judge_batch(query, &items).await;
                (bidx, chunk, outcome)
            }
        })
        .buffered(cli.concurrency.max(1));

    while let Some((bidx, chunk, outcome)) = result_stream.next().await {
        match outcome {
            Ok(map) => {
                for &ri in chunk {
                    let rec = &records_ref[ri];
                    let key = match &rec.kind {
                        RecordKind::Ask { key, .. } => key.as_str(),
                        _ => unreachable!(),
                    };
                    match map.get(key) {
                        Some(LineOutcome::Verdict(v)) => {
                            verdict_by_key.insert(key, *v);
                        }
                        Some(LineOutcome::MissingAnswer) | None => {
                            eprintln!(
                                "jevgrep: no answer from TypeSafe API for {}:{}",
                                plan.files[rec.file_idx].label, rec.lineno
                            );
                            had_error = true;
                            if cli.strict {
                                stop_now = true;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("jevgrep: {e}");
                had_error = true;
                if cli.strict || e.fatal {
                    stop_now = true;
                }
            }
        }

        emit_ready(
            &mut next_file_to_emit,
            Some(bidx),
            &verdict_by_key,
            &mut printer,
            &mut total_matches,
        );

        if shutdown.load(Ordering::Relaxed) {
            had_error = true;
            stop_now = true;
        }
        if stop_now {
            break;
        }
    }
    drop(result_stream);

    // Anything left (only reachable if the stream ended without an abort,
    // or if abort happened after the last needed batch) gets emitted now.
    if !stop_now {
        emit_ready(
            &mut next_file_to_emit,
            Some(batches.len().saturating_sub(1)),
            &verdict_by_key,
            &mut printer,
            &mut total_matches,
        );
    }

    if let Some((kind, remaining_n)) = plan.truncated {
        had_error = true;
        let (flag, limit) = match kind {
            TruncKind::MaxFiles => ("--max-files", cli.max_files),
            TruncKind::MaxLines => ("--max-lines", cli.max_lines),
        };
        output::print_stop_message(flag, limit, remaining_n);
    }

    if cli.stats {
        let usage = judge.usage();
        let judged = plan.records.len() as u64 - plan.free_count;
        output::print_stats(
            plan.files.len() as u64,
            plan.records.len() as u64,
            judged,
            plan.free_count,
            total_matches,
            usage.requests,
            usage.input_tokens,
            usage.output_tokens,
            start.elapsed().as_secs_f64(),
        );
    }

    Ok(RunOutcome {
        matched_any: total_matches > 0,
        had_error,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_file(
    file_idx: usize,
    meta: &FileMeta,
    records: &[lines::LineRecord],
    verdict_by_key: &HashMap<&str, Verdict>,
    cli: &Cli,
    fmt_params: &FormatParams,
    printer: &mut Printer,
    total_matches: &mut u64,
) {
    let raw_by_lineno: Vec<String> = records.iter().map(|r| r.raw.clone()).collect();
    let mut matched: Vec<(u64, f64)> = Vec::new();
    let mut confidence_by_line: HashMap<u64, f64> = HashMap::new();

    for r in records {
        let verdict = match &r.kind {
            RecordKind::Free(v) => Some(*v),
            RecordKind::Ask { key, .. } => verdict_by_key.get(key.as_str()).copied(),
            RecordKind::Dup { key } => verdict_by_key.get(key.as_str()).copied(),
        };
        let Some(v) = verdict else {
            continue; // dropped: no verdict, never shown, never counted
        };
        let is_match = if cli.invert_match {
            v.probability < cli.threshold
        } else {
            v.probability >= cli.threshold
        };
        if is_match {
            matched.push((r.lineno, v.probability));
            confidence_by_line.insert(r.lineno, v.confidence);
        }
    }

    *total_matches += matched.len() as u64;

    let file_lines = FileLines {
        file_idx,
        label: &meta.label,
        raw_by_lineno: &raw_by_lineno,
        matched: &matched,
    };

    if cli.json {
        output::print_json(&file_lines, &confidence_by_line);
    } else if cli.files_with_matches {
        output::print_files_with_matches(&file_lines);
    } else if cli.files_without_match {
        output::print_files_without_match(&file_lines);
    } else if cli.count {
        output::print_count(&file_lines, fmt_params.show_filename);
    } else {
        printer.print_file(&file_lines, fmt_params);
    }
}

/// `--dry-run`: walk and count only, no API calls, no bound-based early
/// stop (the whole point is to preview whether a bound *would* trip).
pub fn dry_run(cli: &Cli) -> Result<()> {
    let file_args = build_file_args(cli)?;
    let batch = cli.batch.max(1);

    let mut total_files = 0usize;
    let mut total_lines = 0usize;
    let mut would_stop: Option<(&'static str, usize)> = None;

    for arg in &file_args {
        let bytes = match lines::read_file_arg(arg) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("jevgrep: {e}");
                continue;
            }
        };
        if walk::looks_binary(&bytes) {
            continue;
        }
        let n = lines::split_lines(&bytes).len();
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
