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

## Syntax

Measured October 30, 2026 on the same machine, release build of `feat/syntax`, against a release build of `main` at 28ec419 run the same way in the same session. The machine was shared with other build jobs (load average 1 to 5), so the baseline was rerun next to each measurement rather than taken from the tables above.

Files: the 60 KB file is `crates/maestro-editor/src/commands.rs` from 28ec419 (60,586 bytes). The 1 MB and 10 MB files are generated lines of valid Rust, 83 bytes each, of the form `fn item_0000001(x: u64, name: &str) -> u64 { let y = x * 1 + name.len() as u64; y }`.

### Input processing with highlighting

Method: the input-processing method above (`MAESTRO_LATENCY_LOG`, the same 267 inputs, each sent by tmux 30 ms apart), started 1 s (60 KB), 2 s (1 MB) or 5 s (10 MB) after launch. The value is the sum of the first three columns (handle, render, flush) per logged frame, in microseconds. Frames redrawn because a background parse finished are logged too, so there are more rows than inputs.

| File | Build | Rows | p50 µs | p95 µs | p99 µs | Max µs |
|---|---|---|---|---|---|---|
| 60 KB | main | 269 | 560 | 747 | 777 | 787 |
| 60 KB | feat/syntax | 270 | 937 | 1201 | 1858 | 2535 |
| 1 MB | main | 269 | 926 | 1054 | 1232 | 1310 |
| 1 MB | feat/syntax | 423 | 1520 | 6002 | 6565 | 6887 |
| 10 MB | main | 269 | 966 | 1095 | 1146 | 1558 |
| 10 MB | feat/syntax | 327 | 1506 | 1860 | 2134 | 5075 |

Starting the keys 0.3 s after opening the 10 MB file, while its first parse was still running on a background thread, gave p50 1508, p95 1983, max 3709 µs.

Where the time goes:

- Highlighting adds about 0.5 ms to a frame whose visible range or tree changed (render p50 655 µs against 484 on the 60 KB file). Frames where neither changed, such as cursor motion within the screen, reuse cached highlights.
- On the 1 MB file most typed characters reparse on the main thread in 3 to 5 ms, which is the p95. Tree-sitter's incremental reparse grows with file size on these flat files: a one-character edit in the middle costs about 0.3 ms at 60 KB, 4 ms at 1 MB and 70 to 200 ms at 10 MB, measured with tree-sitter alone on a contiguous string as well as through the rope.
- So the main thread gives a reparse 4 ms (`REPARSE_BUDGET` in `crates/maestro-editor/src/syntax.rs`). Past that, or when the previous reparse was over budget, the reparse runs on a background thread. Until it returns, the screen draws the old tree with the edits applied to it, so colors shift with the text but new text is not parsed yet. Edits made meanwhile are replayed onto the result.
- Applying a background result runs inside `poll_background`, outside the logged handle time. A temporary build that timed `poll_background` measured p50 46 µs, max 83 µs on the 1 MB file and p50 90 µs, max 423 µs on the 10 MB file. Before replaced trees were freed on a separate thread, the 10 MB file spent p50 14 ms, max 37 ms there, freeing the old tree.

### Startup

Method: `scripts/bench_startup.py`, 30 runs per file, inside a tmux pane.

| File | main median ms | feat/syntax median ms | feat/syntax p95 ms | feat/syntax max ms |
|---|---|---|---|---|
| 60 KB | 1.9 | 19.0 | 20.1 | 20.7 |
| 1 MB | 4.0 | 5.4 | 5.8 | 6.1 |
| 10 MB | 20.5 | 21.7 | 24.8 | 25.0 |

Files up to 256 KB are parsed on the main thread before the first frame (within a 50 ms budget, after which the parse moves to a background thread), and the first frame waits for the language's highlight query. Compiling the Rust query takes about 15 ms (TSX 15, TypeScript 14, JavaScript 6, Python 4, C 2.6, the rest under 1). It starts on another thread in parallel with the parse and runs once per language per process. Bigger files open without color and gain it when the background parse finishes: about 0.2 s for 1 MB and 1.5 to 2.7 s for 10 MB.

## Known costs not yet measured or fixed

- Search materializes the whole document as a `String` per search (O(n)). Incremental search previews on a 100 MB file will be slow. A rope-aware regex engine is the fix.
- Search highlighting runs the regex over the visible lines every frame.
- Very long lines are rendered by walking graphemes from the line start, so a single 10 MB line will be slow to draw.
- No benchmark yet for thousands of selections, project search, or many agent sessions; those features do not exist yet.
- An edit that changes everything after it in a big file (an unclosed quote at the top of 10 MB) makes tree-sitter re-lex everything after it. That runs on a background thread (0.16 to 0.28 s at 10 MB in two runs), and colors below the edit are stale until it lands.

## Targets

From the project brief, and not claimed as met beyond the table above: under 100 ms cold start for a small file, under 16 ms p95 input handling for ordinary files, no input freezes during background work.
