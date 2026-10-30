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
## Syntax

Measured October 30, 2026 on the same machine, release build of `feat/syntax` at 54c0356, against a release build of `main` at 28ec419 run the same way in the same session. Other build jobs shared the machine (load average 3 to 4), so the baseline was rerun next to each measurement rather than taken from the tables above.

Files: the 60 KB file is `crates/maestro-editor/src/commands.rs` from 28ec419 (60,586 bytes). The 1 MB and 10 MB files are generated lines of valid Rust, 83 bytes each, of the form `fn item_0000001(x: u64, name: &str) -> u64 { let y = x * 1 + name.len() as u64; y }`.

### Input processing with highlighting

Method: the input-processing method above (`MAESTRO_LATENCY_LOG`, the same 267 inputs, each sent by tmux 30 ms apart), started 1 s (60 KB), 2 s (1 MB) or 5 s (10 MB) after launch, when the first parse had finished. The value is the sum of the first three columns (handle, render, flush) per logged frame, in microseconds.

Frames redrawn because a background parse finished are logged too, so there are more rows than inputs. The logger in `maestro-tui` keeps the last input's handle time until the next input, so those extra rows repeat it. The "corrected" rows come from a temporary build (not committed) that resets the handle time after each logged frame and also times `poll_background`.

| File | Build | Rows | p50 µs | p95 µs | p99 µs | Max µs |
|---|---|---|---|---|---|---|
| 60 KB | main | 269 | 562 | 770 | 840 | 995 |
| 60 KB | feat/syntax | 270 | 924 | 1498 | 1970 | 2564 |
| 60 KB | corrected | 270 | 922 | 1271 | 1914 | 2572 |
| 1 MB | main | 269 | 912 | 1060 | 1191 | 1328 |
| 1 MB | feat/syntax | 424 | 1551 | 6222 | 6902 | 8031 |
| 1 MB | corrected | 420 | 2200 | 5727 | 7048 | 9575 |
| 10 MB | main | 269 | 941 | 1050 | 1179 | 1468 |
| 10 MB | feat/syntax | 321 | 1639 | 2408 | 4137 | 7353 |
| 10 MB | corrected | 328 | 1582 | 2676 | 5427 | 7880 |

Starting the keys 0.3 s after opening the 10 MB file, while its first parse was still running on a background thread, gave (corrected build) p50 1514, p95 1926, max 2604 µs.

Where the time goes:

- Highlighting adds about 0.2 to 1.0 ms to a frame whose visible range or tree changed (render p50 654 µs against 490 on the 60 KB file, 1.4 to 1.8 ms against 0.85 on the dense generated files). Frames where neither changed, such as cursor motion within the screen, reuse cached highlights.
- On the 1 MB file most typed characters reparse on the main thread in 3 to 5 ms, which is the p95. Tree-sitter's incremental reparse grows with file size on these flat files: a one-character edit in the middle costs about 0.3 ms at 60 KB, 4 ms at 1 MB and 70 to 200 ms at 10 MB, measured with tree-sitter alone on a contiguous string as well as through the rope.
- So the main thread gives a reparse 4 ms (`REPARSE_BUDGET` in `crates/maestro-editor/src/syntax.rs`). Past that, or when the previous reparse was over budget, the reparse runs on a background thread. Until it returns, the screen draws the old tree with the edits applied to it, so colors shift with the text but new text is not parsed yet. Edits made meanwhile are replayed onto the result.
- Tree-sitter checks the budget between tokens, so one long token (a block comment opened at the top of a 1 MB file) can overrun it. Edits over 256 KB, such as a reload or a huge paste, skip the old tree and parse from scratch, off the main thread when the file is over 256 KB.
- Applying a background result runs inside `poll_background`, outside the logged handle time. The corrected build measured p50 117 µs, max 632 µs there on the 1 MB file and p50 97 µs, max 382 µs on the 10 MB file. Before replaced trees were freed on a separate thread, the 10 MB file spent p50 14 ms, max 37 ms there, freeing the old tree.

### Startup

Method: `scripts/bench_startup.py`, 30 runs per file, inside a tmux pane.

| File | main median ms | feat/syntax median ms | feat/syntax p95 ms | feat/syntax max ms |
|---|---|---|---|---|
| 60 KB | 1.5 | 18.8 | 20.5 | 22.5 |
| 1 MB | 4.1 | 5.3 | 6.3 | 6.4 |
| 10 MB | 21.0 | 21.8 | 24.1 | 24.9 |

Files up to 256 KB are parsed on the main thread before the first frame (within a 50 ms budget, after which the parse moves to a background thread), and the first frame waits for the language's highlight query. Compiling the Rust query takes about 15 ms (TSX 15, TypeScript 14, JavaScript 6, Python 4, C 2.6, the rest under 1). It starts on another thread in parallel with the parse and runs once per language per process. Bigger files open without color and gain it when the background parse finishes: about 0.2 s for 1 MB and 1.5 to 2.7 s for 10 MB.

## Known costs not yet measured or fixed

- Search materializes the whole document as a `String` per search (O(n)). Incremental search previews on a 100 MB file will be slow. A rope-aware regex engine is the fix.
- Search highlighting runs the regex over the visible lines every frame.
- Very long lines are rendered by walking graphemes from the line start, so a single 10 MB line will be slow to draw.
- No benchmark yet for thousands of selections or many agent sessions.
- Visible lines totaling more than 256K chars (minified files) are drawn without syntax colors.
- An edit that changes everything after it in a big file (an unclosed quote at the top of 10 MB) makes tree-sitter re-lex everything after it. That runs on a background thread (0.16 to 0.28 s at 10 MB in two runs), and colors below the edit are stale until it lands.

## Targets

From the project brief, and not claimed as met beyond the table above: under 100 ms cold start for a small file, under 16 ms p95 input handling for ordinary files, no input freezes during background work.
