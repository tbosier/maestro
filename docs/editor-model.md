# Editor model

## Positions

Internal positions are char indices into the rope (`usize`). Conversions to other coordinate systems live in `maestro_core::pos`:

| Type | Meaning | Used by |
|---|---|---|
| `usize` | char index | everything inside the editor |
| `ByteOffset` | UTF-8 byte offset | file IO, regex matches |
| `LspPosition` | line plus UTF-16 code unit | language servers |
| `LineCol` | line plus display column (tabs expanded, wide chars count 2) | rendering, vertical motion |

Cursor movement steps over extended grapheme clusters, so a CRLF pair, an emoji ZWJ sequence or a letter with combining marks moves as one unit.

## Selections

A selection is one or more ranges, sorted and non-overlapping, with one primary. Each range has an anchor and a head.

In normal and select mode a range is inclusive: it covers every grapheme from the smaller end through the grapheme that starts at the larger end. A collapsed range (anchor equals head) covers the character under the block cursor. So `d` on a fresh cursor deletes one character, and `wd` deletes a word plus its trailing space.

In insert mode ranges are collapsed insertion points.

After every normal-mode command the editor:

1. commits the undo group,
2. clamps selections so no cursor sits past the last character,
3. scrolls the view to keep the primary cursor visible with `scrolloff` lines of margin.

Ranges that overlap after an edit merge. The merged range stays primary if either part was.

## Modes

- Normal: motions replace the selection (`w` selects the next word).
- Select (`v`): motions extend from the existing anchor.
- Insert: typing inserts at every cursor. The whole session, from entering insert to leaving it, is one undo step and one unit for `.`.
- Prompts (`:`, `/`, `?`, `s`, `S`, `K`) are overlays. Regex prompts preview their effect as you type; Escape restores the selection.

## Profiles

`maestro` (default) is the selection-first model described here.

`vim` adds a small set of Vim bindings on top: `dd`, `yy`, `cc`, `x`, `X`, `D`, `V`. It does not implement Vim's operator-pending grammar (`dw`, `ci(`, `3dd` with counts on operators beyond `dd`), visual block mode, or ex ranges other than `%`. Those keys report "no binding" instead of doing something unexpected.

There is no separate Helix profile yet. The Maestro profile already follows Helix for most keys; the differences are documented in `crates/maestro-editor/src/keymap.rs`.

## Repeat, macros, registers

- `.` replays the keys of the last change. A change is any command that modified the document, or an insert session from the key that entered insert mode through Escape.
- `Q` starts and stops recording into register `@` (or `"xQ` for register `x`). `q` replays; a count repeats it.
- Registers hold one string per selection. Yanking three selections and pasting into three selections puts each piece in its own place. `+` and `*` are the system clipboard. `_` discards.
- Text ending in a newline pastes linewise: below the line with `p`, above with `P`.

## Invariants the tests hold

- A change set refuses to apply to a document of a different length.
- Applying a change set and then its inverse restores the original text (property test).
- Mapped positions stay inside the new document (property test).
- Selections stay inside the document after arbitrary key sequences (`ranges_stay_valid_after_random_edits`).
- Saving never leaves a partial file: the new text is written and synced to a temp file, then renamed over the target.
- A buffer whose file changed on disk refuses `:w` until the user chooses `:w!` or `:reload`.
