# Performance

## Hardware and build

All numbers below were measured on October 30, 2026 on one machine:

- AMD Ryzen 9 5900X (12 cores, 24 threads), 70 GB RAM
- Linux 6.18.58-1-lts (Arch), rustc 1.99.1
- Release build (`lto = "thin"`, `codegen-units = 1`)
- Inside tmux 3.8, 120x40 cells, truecolor

They are single-machine samples, not a benchmark suite with controlled noise. Treat differences under about 20 percent as noise.

## Startup to first frame

Method: `scripts/bench_startup.py` runs `maestro --clean --first-frame-only <file>` inside a tmux pane and times from process start to exit. `--first-frame-only` loads the file, draws one full frame, flushes it and exits, so the time covers process start, file read, decode, rope construction, first render and terminal output. Generated files are lines of Rust-like text about 80 bytes long.

| File | Runs | Median ms | p95 ms | Max ms |
|---|---|---|---|---|
| none | 30 | 1.6 | 2.3 | 2.4 |
| 60 KB source file | 30 | 1.4 | 1.7 | 2.0 |
| 1 MB | 30 | 3.4 | 4.4 | 4.5 |
| 10 MB | 30 | 17.9 | 21.2 | 21.4 |
| 100 MB | 5 | 170.1 | 180.1 | 180.1 |

The 100 MB time is dominated by reading the file, validating UTF-8, hashing the bytes and building the rope. Nothing is lazy yet.

The content hash (used to detect files rewritten with the same size and timestamp) was added after the first measurement. Before it, the medians were 15.0 ms at 10 MB and 144.4 ms at 100 MB, so it costs roughly 3 ms and 26 ms at those sizes. Saving now also re-reads and hashes the file on disk before overwriting it; that cost has not been measured separately.

## Input processing

Method: run with `MAESTRO_LATENCY_LOG=<path>`. For every batch of input events the editor appends the time spent handling input, rendering the frame into the off-screen grid, and flushing changed cells to the terminal. tmux then sent 267 inputs: 100 `j`, `G`, `gg`, `o`, 100 typed characters, Escape, 30 undo/redo pairs, `w`, `C-d`. This measures time inside the editor. It does not include the terminal's own drawing or the display.

| File | Events | p50 µs | p95 µs | p99 µs | Max µs |
|---|---|---|---|---|---|
| 60 KB source | 267 | 509 | 801 | 831 | 857 |
| 100 MB | 267 | 787 | 940 | 1251 | 1421 |

Rendering is most of it (p50 430 µs on the small file). Key handling itself has a p50 of 17 µs.

## File finder on a large tree

Method: the ignored test `large_tree_streams_without_blocking_input` in `crates/maestro-editor/tests/navigation.rs` (`cargo test --release -p maestro-editor --test navigation -- --ignored --nocapture`). It generates 200,000 empty files in 20,000 directories, opens the finder, and imitates the terminal loop: apply queued results if any, otherwise handle one key (a query typed and erased again), sleeping 1 ms per turn in place of a frame. Nothing is rendered, so "first results" means applied to the picker, not drawn. Load average was about 2.4 on 24 threads during these runs; other builds were running on the machine.

| Measure | Result over 5 runs |
|---|---|
| First results applied | 1.2 to 1.3 ms after the key |
| Whole tree listed | 520 to 545 ms, over 12 to 13 polls that applied results |
| Polls that applied results back to back (no input read in between) | never more than 1 in a row |
| Slowest poll that applied results | 7.6 to 11.8 ms |
| Key handling while loading, p50 / p95 / max | 1.4 to 1.5 / 10.3 to 11.6 / 14.6 to 17.6 ms |
| Typing `file_199999` into the full 200,000-file list, slowest key | 13.5 to 17.0 ms |

How it keeps input moving: the walker has at most one batch queued at a time, including the last one, and waits 40 ms after a batch is applied before sending the next, so the terminal loop reads input between batches. Matching checks only new items when a batch arrives (merged into the sorted list) and only current matches when a character is typed. Lists over 16,384 items are matched on up to 16 threads, spawned per key, and highlight positions are computed only for rows near the top and the selection. A broad query over a list this size still costs 10 to 17 ms per key; the work is not moved off the main thread.

The real binary in tmux with `MAESTRO_LATENCY_LOG`, same tree, load average 4.3 to 4.7: with keys sent 30 ms apart, the slowest input batch was 19 and 38 ms in two runs; with keys 250 ms apart, each key took 0.2 to 15 ms. During an earlier version's runs on a more heavily loaded machine (load average 7 to 10), fast typing queued behind refilters for up to 700 ms. That version computed highlight positions for every match and re-sorted the whole list per batch; it was not re-measured under that load after the fix.

Workspace search over the Cargo registry sources (9,240 files, 249 MB) in tmux: `unsafe fn` found 4,498 lines while the query `write` was typed into the open picker, and `fn` stopped at the 10,000-result cap with the title saying so. The slowest input batch was 5.3 ms.

The picker preview reads files on the main thread when the selection moves: at most 4 MB per file, regular files only. It is fast on local disks; a slow network filesystem would stall the editor for as long as the read takes.

## Known costs not yet measured or fixed

- Search materializes the whole document as a `String` per search (O(n)). Incremental search previews on a 100 MB file will be slow. A rope-aware regex engine is the fix.
- Search highlighting runs the regex over the visible lines every frame.
- Very long lines are rendered by walking graphemes from the line start, so a single 10 MB line will be slow to draw.
- No benchmark yet for thousands of selections, syntax updates, or many agent sessions; those features do not exist yet.

## Targets

From the project brief, and not claimed as met beyond the table above: under 100 ms cold start for a small file, under 16 ms p95 input handling for ordinary files, no input freezes during background work.
