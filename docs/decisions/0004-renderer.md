# 0004: A small custom renderer on crossterm

Date: October 30, 2026. Status: accepted.

## Decision

The terminal front end draws into its own cell grid and diffs it against the previous frame, writing changed cells with crossterm. It does not use ratatui.

## Why

The main area needs per-grapheme styling (syntax, several selection layers, cursors, search matches) and correct handling of wide graphemes at the scroll edges. That is most of what a renderer does; the rest (boxes, a status line) is a few dozen lines. Owning the grid keeps the diff and the synchronized-update flush in one place. The whole `maestro-tui` crate is about 1,500 lines, including tests, the clipboard providers and the terminal lifecycle.

## Consequences

Widgets like scrollable lists are written by hand. If panels multiply in M2, ratatui can render into regions of the same terminal; the editor crate does not depend on either choice, which is what a future GUI needs.
