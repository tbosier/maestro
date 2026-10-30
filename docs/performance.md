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
| none | 30 | 1.1 | 1.6 | 1.6 |
| 60 KB source file | 30 | 1.8 | 2.0 | 2.3 |
| 1 MB | 30 | 3.6 | 3.9 | 4.1 |
| 10 MB | 30 | 15.0 | 17.3 | 17.3 |
| 100 MB | 5 | 144.4 | 151.8 | 151.8 |

The 100 MB time is dominated by reading the file, validating UTF-8 and building the rope. Nothing is lazy yet.

## Input processing

Method: run with `MAESTRO_LATENCY_LOG=<path>`. For every batch of input events the editor appends the time spent handling input, rendering the frame into the off-screen grid, and flushing changed cells to the terminal. tmux then sent 267 inputs: 100 `j`, `G`, `gg`, `o`, 100 typed characters, Escape, 30 undo/redo pairs, `w`, `C-d`. This measures time inside the editor. It does not include the terminal's own drawing or the display.

| File | Events | p50 µs | p95 µs | p99 µs | Max µs |
|---|---|---|---|---|---|
| 60 KB source | 267 | 509 | 801 | 831 | 857 |
| 100 MB | 267 | 787 | 940 | 1251 | 1421 |

Rendering is most of it (p50 430 µs on the small file). Key handling itself has a p50 of 17 µs.

## File finder on a large tree

Method: the ignored test `large_tree_streams_without_blocking_input` in `crates/maestro-editor/tests/navigation.rs` (`cargo test --release -p maestro-editor --test navigation -- --ignored --nocapture`). It generates 200,000 empty files in 20,000 directories, opens the finder, and imitates the terminal loop: apply queued results if any, otherwise handle one key (a query typed and erased again), sleeping 1 ms per turn for the frame. The machine was shared with other builds during these runs (load average 5 to 8 on 24 threads), so the spread is wide.

| Measure | Result over 5 runs |
|---|---|
| First results on screen | 1.2 to 2.4 ms after the key |
| Whole tree listed | 490 to 750 ms, in 11 to 14 batches |
| Batches applied back to back (input not read in between) | never; at most 1 batch per loop turn |
| Key handling while loading, p50 / p95 / max | 1.1 to 1.5 / 7.9 to 24 / 12 to 43 ms |
| Typing `file_199999` into the full 200,000-file list, slowest key | 13 to 33 ms (31 to 38 ms with matching on one thread) |

How it stays responsive: the walker sends at most one batch at a time and waits 40 ms after each batch is applied before sending the next, so the front end always gets a turn to read input. Matching only checks new items when a batch arrives and only current matches when a character is typed, splits large lists across threads, and computes highlight positions only for rows near the top and the selection.

The real binary in tmux with `MAESTRO_LATENCY_LOG`, same tree: keys typed 250 ms apart took 2 to 17 ms each. Keys typed 30 ms apart while the machine was loaded (load average 7 to 10) queued behind broad-query refilters of the full list and were handled in single batches of 60 to 700 ms. Typing fast into a list of hundreds of thousands of files on a busy machine can therefore lag; it does not lag on ordinary projects (the Maestro repository lists 68 files).

Workspace search over the Cargo registry sources (9,240 files, 249 MB) in tmux: `unsafe fn` found 4,498 lines while the query `write` was typed into the open picker; `fn` stopped at the 10,000-result cap. Key handling stayed under 2.2 ms.

## Known costs not yet measured or fixed

- Search materializes the whole document as a `String` per search (O(n)). Incremental search previews on a 100 MB file will be slow. A rope-aware regex engine is the fix.
- Search highlighting runs the regex over the visible lines every frame.
- Very long lines are rendered by walking graphemes from the line start, so a single 10 MB line will be slow to draw.
- No benchmark yet for thousands of selections, syntax updates, or many agent sessions; those features do not exist yet.

## Targets

From the project brief, and not claimed as met beyond the table above: under 100 ms cold start for a small file, under 16 ms p95 input handling for ordinary files, no input freezes during background work.
