# 0003: Ropey for the text buffer

Date: October 30, 2026. Status: accepted.

## Decision

Use Ropey 1.6.

## Why

- Char-indexed API with built-in line counting that handles LF, CRLF and the Unicode line separators.
- Built-in UTF-16 code unit conversion, which the LSP position mapping needs.
- Cheap clones (shared, copy-on-write), which make snapshots for background work free.
- Mature and used by Helix, so its edge cases have been exercised.

Crop was considered. It indexes by bytes and may be faster for some edits. We did not benchmark the two against each other; this decision rests on API fit. Measured startup and input numbers (docs/performance.md) do not show rope operations as a bottleneck. Revisit if profiling ever does.
