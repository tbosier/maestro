# Architecture

## Crates

```
maestro-cli ──► maestro-tui ──► maestro-editor ──► maestro-core
     │               │                │  │  ├──► maestro-git
     │               │                │  │  └──► maestro-forge ──► maestro-git
     │               │                │  └─────► maestro-lsp ──► maestro-core
     │               └──────► maestro-config ◄───┘
     └──► maestro-automation ──► maestro-editor, maestro-protocol
```

| Crate | Owns | Depends on |
|---|---|---|
| `maestro-core` | Rope text, position types, selections, change sets, transactions, undo tree, documents, file IO | ropey, unicode-segmentation, unicode-width, thiserror |
| `maestro-config` | TOML config, theme tokens, the Duskfox palette | serde, toml |
| `maestro-editor` | Editor state, modes, keymaps, the command registry, motions, prompts, pickers, registers, search, crash recovery, Git gutter signs and pickers, language server integration (`lsp.rs`, `lsp_commands.rs`), per-buffer syntax state, coding agents with the agent panel, review screen, task contracts, overlap detection between agents, hunk marks with partial integration and composed results (`agents/`), edit proposals from automation clients with their review overlay and verification runs (`proposals/`), the GitHub inbox with its detail view, message buffers and write dialogs (`forge/`), projects, worktree switching, the sidebar, the jump palette and agent attention (`workspaces/`) | core, config, git, lsp, syntax, agent, term, tasks, forge, regex |
| `maestro-git` | Repository discovery, status, line diffs for gutter signs, a unified diff model, worktree lifecycle with safety checks, merge dry runs, the review diff of a worktree against its base, intraline ranges, committing a worktree's work, integrating a branch by fast-forward, merge or cherry-pick with checks before and after, snapshots of a worktree's uncommitted files as unreferenced commits for merge dry runs between worktrees, and trees and commits built from chosen hunks on a base commit (`hunks.rs`); all through the `git` CLI (ADR 0005) | similar, thiserror |
| `maestro-term` | Programs in pseudo-terminals: a VT screen with scrollback, key and paste encoding, resize, exit status, kill (ADR 0006) | portable-pty, vt100 |
| `maestro-agent` | Agent providers found on `PATH`, the Agent Client Protocol client on plain threads (handshake, prompts, cancel, permission requests, transcript events), file reads and writes for an agent confined to its root by path checks (not a sandbox), and a fake agent for tests behind `fake-agent` (ADR 0006) | agent-client-protocol-schema, serde_json |
| `maestro-tasks` | Verification commands in their own process group, output streamed by line, `cargo test` summaries (ADR 0006) | std |
| `maestro-forge` | GitHub through the GitHub CLI (`gh`): searches for review requests, assigned and own pull requests and assigned issues, a repository's open pull requests, a pull request's status (review decision, mergeability, checks), details and diff (parsed with maestro-git's parser), and the write actions as exact command lines (approve and request changes through `gh api` on the head commit shown, after checking it has not moved; comment; merge with `--match-head-commit`, then the resulting state read back). Argument vectors, `--json` output, a timeout per call, errors in plain words (gh missing, logged out, network, rate limit) | git, serde, serde_json, thiserror |
| `maestro-lsp` | Language server processes on plain threads: framing, JSON-RPC, initialize and shutdown, cancellation, exit detection, a bounded stderr log, server requests handed to the caller (`workspace/applyEdit`), position and change conversions, and parsing of completion, signature help and workspace edit replies. A fake server for tests behind the `fake-server` feature | core, serde_json |
| `maestro-syntax` | Tree-sitter grammars, incremental parsing from change sets, highlight spans, syntax node ranges | core, tree-sitter and grammar crates |
| `maestro-tui` | Terminal lifecycle, input conversion, rendering, clipboard providers | editor, crossterm |
| `maestro-protocol` | Automation API wire types: JSON-RPC messages, params, results, error codes | serde, serde_json |
| `maestro-automation` | Runs protocol requests against an `Editor`; headless (stdio) and Unix socket transports | core, config, editor, protocol |
| `maestro-cli` | The `maestro` binary: arguments, config loading, startup | tui, automation |

`maestro-editor` runs `maestro-forge` only on background threads (`forge/`); `maestro-tui` draws the inbox (`src/inbox.rs`) and reuses the agent review's diff rows for pull request diffs.

`maestro-editor` uses the agent crates (`maestro-agent`, `maestro-term`, `maestro-tasks`) for the agent panel in `src/agents/`; `maestro-tui` draws it (`src/agents.rs`). ADR 0006 records how they fit together and `docs/agent-model.md` what the panel does. ADR 0001 explains why the spec's `vesper-command` became `maestro-editor`.

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

Editing actions are named commands in `maestro_editor::commands::COMMANDS` or typed commands in `maestro_editor::typed`. The keys inside the agent panel, its review and the proposal review are handled by those screens directly and are not commands yet. Keymaps bind key sequences to names, the palette lists names, and user config binds names. The automation API (`command.run`, see `docs/automation.md`) calls the same names. Adding a command means adding one function and one line in the `commands!` table; a test fails if a default binding names a command that does not exist.

## Rendering

The terminal front end draws each frame into an off-screen grid (`Surface`) and writes only the cells that differ from the previous frame, inside a synchronized update. Truecolor is used when the terminal reports it, with a 256-color fallback. ADR 0004 covers why this is a small custom layer and not ratatui.

## Language servers

`maestro-lsp` runs each server with four threads (stdin writer, stdout reader, exit watcher, stderr log) and never waits for a server's answer: a request returns a channel. The editor waits on that channel in a short-lived background thread, which also reads any files the reply points into, and applies the reply as a job on the main thread. The reply is dropped if any open buffer's version moved since the request. Two things still happen on the editor thread: finding a server on `PATH` and spawning it, and, on quit, waiting up to half a second per step for each server to shut down. Notifications such as diagnostics arrive through an idle job sender, so the terminal shows them at its next wake-up (at most a second).

Every text-change path in the editor calls `lsp::document_changed`, which maps stored diagnostics through the change and sends an incremental `didChange` built from the change set in old-document coordinates. `lsp::reconcile` runs after each key, on open and on tick: it opens and closes documents on servers to match the buffers, and resends full text for any document whose version moved without the hook. Positions are UTF-16 through `maestro_core::pos`.

Completion (`lsp/completion.rs`) hooks into the same paths: the open popup takes its keys before the insert keymap, `document_changed` maps the popup's char ranges through each edit, and a hook after each key refilters the popup or starts a request. Its reply guard is stricter than the one above: the version, every cursor, the focused view and insert mode must all be unchanged. Signature help (`lsp/signature.rs`) uses the same hook and the same guard.

Rename, formatting and code actions (`lsp/actions.rs`) keep one request out at a time. A reply that edits text is dropped if any buffer changed, opened or closed since the request; one that opens a prompt or picker is also dropped if the cursor moved or the user turned to something else. The edits themselves go through `lsp/edit.rs`, which checks the whole workspace edit (versions, ranges, overlaps, read-only files, files that are not open) before changing anything, then applies each buffer's change as one undo step with `proposals::apply_versioned`. Files that are not open become buffers without a view; nothing is written to disk. `workspace/applyEdit` is the one server-to-client request the client does not answer itself: the reader thread hands it to the editor as `Event::Request`, and the editor answers with `Client::respond` after applying (or refusing) the edit, so its `didChange` notifications reach the server before the answer.

## Persistence

- Files: `maestro_core::io::save` (atomic rename, fsync of file and directory).
- Recovery journal: `$XDG_STATE_HOME/maestro/recovery`, owner-only permissions.
- Sessions and recent files: `$XDG_STATE_HOME/maestro/sessions/` and `recent.toml` (`maestro_editor::navigation`).
- Config: `$XDG_CONFIG_HOME/maestro/config.toml`.
