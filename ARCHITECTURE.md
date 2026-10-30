# Architecture

## Crates

```
maestro-cli ──► maestro-tui ──► maestro-editor ──► maestro-core
                     │                │     │
                     │                │     └──► maestro-git
                     └──────► maestro-config ◄────┘
```

| Crate | Owns | Depends on |
|---|---|---|
| `maestro-core` | Rope text, position types, selections, change sets, transactions, undo tree, documents, file IO | ropey, unicode-segmentation, unicode-width, thiserror |
| `maestro-config` | TOML config, theme tokens, the Duskfox palette | serde, toml |
| `maestro-editor` | Editor state, modes, keymaps, the command registry, motions, prompts, pickers, registers, search, crash recovery, Git gutter signs and pickers | core, config, git, regex |
| `maestro-git` | Repository discovery, status, line diffs for gutter signs, a unified diff model, worktree lifecycle with safety checks, merge dry runs; all through the `git` CLI (ADR 0005) | similar, thiserror |
| `maestro-tui` | Terminal lifecycle, input conversion, rendering, clipboard providers | editor, crossterm |
| `maestro-cli` | The `maestro` binary: arguments, config loading, startup | tui |

Planned crates (`maestro-syntax`, `maestro-lsp`, `maestro-protocol`, `maestro-agent`, `maestro-tasks`) are added when their work starts, not before. ADR 0001 explains why the spec's `vesper-command` became `maestro-editor`.

## Rules

- `maestro-core` has no terminal, async runtime, network, Git or language-server dependency. Its tests run as plain data.
- `maestro-git` has no editor or terminal dependency. It blocks while `git` runs; the editor calls it only from background threads.
- `maestro-editor` never touches a terminal. Tests drive it with key sequences (`crates/maestro-editor/tests/editing.rs`), the same path the terminal uses.
- The terminal crate reads editor state and draws it. It does not implement editing.
- Colors appear in one file, `crates/maestro-config/src/duskfox.rs`. Everything else asks the theme for a role.
- No `unsafe` (`unsafe_code = "forbid"` in the workspace lints).

## The editing model in one pass

A `Document` holds a Ropey rope, its path, encoding and line ending, a version number and an undo tree. It only changes through a `Transaction`: a `ChangeSet` (retain, delete and insert operations covering the whole document) plus an optional new selection.

A `ChangeSet` records the document length it expects. Applying it to text of a different length fails without touching the text. `Document::apply_at_version` adds a version check on top. Together they are the base for agent edits (M3): a stale edit is refused, not applied at shifted offsets.

Changes accumulate until `Document::commit` closes an undo group. The editor commits after every normal-mode command and when insert mode ends. Undo and redo walk a revision tree; a new edit after undo creates a branch.

Positions are char indices internally. Byte offsets, LSP UTF-16 positions and screen columns have their own types in `maestro_core::pos` with tested conversions.

Selections follow Kakoune: a range covers whole graphemes from `min(anchor, head)` through the grapheme at `max(anchor, head)`. A collapsed range still covers the character under the cursor. See ADR 0002 and `docs/editor-model.md`.

## Commands

Every action is a named command in `maestro_editor::commands::COMMANDS` or a typed command in `maestro_editor::typed`. Keymaps bind key sequences to names, the palette lists names, and user config binds names. The automation API (M3) will call the same names. Adding a command means adding one function and one line in the `commands!` table; a test fails if a default binding names a command that does not exist.

## Rendering

The terminal front end draws each frame into an off-screen grid (`Surface`) and writes only the cells that differ from the previous frame, inside a synchronized update. Truecolor is used when the terminal reports it, with a 256-color fallback. ADR 0004 covers why this is a small custom layer and not ratatui.

## Persistence

- Files: `maestro_core::io::save` (atomic rename, fsync of file and directory).
- Recovery journal: `$XDG_STATE_HOME/maestro/recovery`, owner-only permissions.
- Config: `$XDG_CONFIG_HOME/maestro/config.toml`.
