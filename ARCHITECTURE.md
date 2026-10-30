# Architecture

## Crates

```
maestro-cli ──► maestro-tui ──► maestro-editor ──► maestro-core
     │               │                │  │  └──► maestro-git
     │               │                │  └─────► maestro-lsp ──► maestro-core
     │               └──────► maestro-config ◄───┘
     └──► maestro-automation ──► maestro-editor, maestro-protocol
```

| Crate | Owns | Depends on |
|---|---|---|
| `maestro-core` | Rope text, position types, selections, change sets, transactions, undo tree, documents, file IO | ropey, unicode-segmentation, unicode-width, thiserror |
| `maestro-config` | TOML config, theme tokens, the Duskfox palette | serde, toml |
| `maestro-editor` | Editor state, modes, keymaps, the command registry, motions, prompts, pickers, registers, search, crash recovery, Git gutter signs and pickers, language server integration (`lsp.rs`, `lsp_commands.rs`) | core, config, git, lsp, regex |
| `maestro-git` | Repository discovery, status, line diffs for gutter signs, a unified diff model, worktree lifecycle with safety checks, merge dry runs; all through the `git` CLI (ADR 0005) | similar, thiserror |
| `maestro-lsp` | Language server processes on plain threads: framing, JSON-RPC, initialize and shutdown, cancellation, exit detection, a bounded stderr log, and position and change conversions. A fake server for tests behind the `fake-server` feature | core, serde_json |
| `maestro-tui` | Terminal lifecycle, input conversion, rendering, clipboard providers | editor, crossterm |
| `maestro-protocol` | Automation API wire types: JSON-RPC messages, params, results, error codes | serde, serde_json |
| `maestro-automation` | Runs protocol requests against an `Editor`; headless (stdio) and Unix socket transports | core, config, editor, protocol |
| `maestro-cli` | The `maestro` binary: arguments, config loading, startup | tui, automation |

Planned crates (`maestro-syntax`, `maestro-agent`, `maestro-tasks`) are added when their work starts, not before. ADR 0001 explains why the spec's `vesper-command` became `maestro-editor`.

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

Every action is a named command in `maestro_editor::commands::COMMANDS` or a typed command in `maestro_editor::typed`. Keymaps bind key sequences to names, the palette lists names, and user config binds names. The automation API (`command.run`, see `docs/automation.md`) calls the same names. Adding a command means adding one function and one line in the `commands!` table; a test fails if a default binding names a command that does not exist.

## Rendering

The terminal front end draws each frame into an off-screen grid (`Surface`) and writes only the cells that differ from the previous frame, inside a synchronized update. Truecolor is used when the terminal reports it, with a 256-color fallback. ADR 0004 covers why this is a small custom layer and not ratatui.

## Language servers

`maestro-lsp` runs each server with four threads (stdin writer, stdout reader, exit watcher, stderr log) and never waits for a server's answer: a request returns a channel. The editor waits on that channel in a short-lived background thread, which also reads any files the reply points into, and applies the reply as a job on the main thread. The reply is dropped if any open buffer's version moved since the request. Two things still happen on the editor thread: finding a server on `PATH` and spawning it, and, on quit, waiting up to half a second per step for each server to shut down. Notifications such as diagnostics arrive through an idle job sender, so the terminal shows them at its next wake-up (at most a second).

Every text-change path in the editor calls `lsp::document_changed`, which maps stored diagnostics through the change and sends an incremental `didChange` built from the change set in old-document coordinates. `lsp::reconcile` runs after each key, on open and on tick: it opens and closes documents on servers to match the buffers, and resends full text for any document whose version moved without the hook. Positions are UTF-16 through `maestro_core::pos`.

## Persistence

- Files: `maestro_core::io::save` (atomic rename, fsync of file and directory).
- Recovery journal: `$XDG_STATE_HOME/maestro/recovery`, owner-only permissions.
- Config: `$XDG_CONFIG_HOME/maestro/config.toml`.
