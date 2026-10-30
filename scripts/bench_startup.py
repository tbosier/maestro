#!/usr/bin/env python3
"""Startup benchmark: wall time from exec to the first frame being flushed.

Must run inside a real terminal (the editor enters raw mode), e.g. a tmux
pane. Uses the undocumented --first-frame-only flag, which draws one frame
and exits. Prints min / median / p95 / max in milliseconds per file.

usage: bench_startup.py <maestro-binary> <runs> <results-file> [file ...]

Results are appended to <results-file>; the editor keeps the terminal.
"""

import statistics
import subprocess
import sys
import time


def measure(binary, path, runs):
    samples = []
    for _ in range(runs):
        args = [binary, "--clean", "--first-frame-only"]
        if path:
            args.append(path)
        t0 = time.perf_counter()
        subprocess.run(args, check=True)
        samples.append((time.perf_counter() - t0) * 1000)
    samples.sort()
    p95 = samples[min(len(samples) - 1, int(round(0.95 * (len(samples) - 1))))]
    return samples[0], statistics.median(samples), p95, samples[-1]


def main():
    binary, runs, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    files = sys.argv[4:] or [""]
    results = []
    for f in files:
        mn, med, p95, mx = measure(binary, f, runs)
        results.append(f"{f or '(no file)'}\tmin {mn:.1f}\tmedian {med:.1f}\tp95 {p95:.1f}\tmax {mx:.1f}\truns {runs}")
    with open(out, "a") as f:
        f.write("\n".join(results) + "\n")


if __name__ == "__main__":
    main()
