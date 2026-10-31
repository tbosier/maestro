# Roadmap

Status words mean: done (acceptance test passes), partial (some of it works, the rest is listed), not started.

## M0: bootable editor (done)

Acceptance: open a Rust file, change code, undo, redo, save, close, reopen, verify the contents.

Verified two ways on October 30, 2026:

- `m0_acceptance_open_edit_undo_redo_save_reopen` in `crates/maestro-editor/tests/editing.rs` drives the flow through the editor's key handling.
- The release binary was driven in a real terminal (tmux) with the same steps; the file on disk had the new contents after `:w` and `:q`, and reopening showed them.

## M1: daily-drivable modal editor (partial)

| Item | Status |
|---|---|
| Modal commands | done for the Maestro profile; the Vim profile is a small subset (`dd`, `yy`, `cc`, `x`, `D`, `V`) |
| Multiple selections | done |
| Multiple buffers | done |
| Splits | done |
| Fuzzy file finder | done: background walk that respects `.gitignore`, `.ignore`, `.git/info/exclude` and Git's global excludes and skips hidden files; results stream into the picker; file preview |
| Search and replace | done in the buffer; workspace search done (`space /`, `:grep`): regex with smart case, binary and ignored files skipped, 10,000-result cap, preview of the match line; it reads files from disk, so unsaved edits in open buffers are not searched |
| Tree-sitter highlighting | done: Rust, Python, TOML, JSON, Markdown (block level), JavaScript, TypeScript, TSX and C; incremental, with large or slow parses on background threads; structural selection with `A-o`/`A-i`/`A-n`/`A-p`. Markdown inline syntax and injected code blocks are not highlighted |
| LSP diagnostics and navigation | partial: diagnostics (sign column, range underline, cursor-line message, `]d`/`[d`, `space d`/`space D` pickers), goto definition (`gd`), references (`gr`), hover (`space k`), incremental sync, crash reporting with `:lsp-restart`, completion in insert mode (trigger characters, a 150 ms pause after identifier characters, `C-x`; client-side fuzzy filtering, re-requests for incomplete lists, `textEdit`, `insertText` and `additionalTextEdits`, snippets as plain text, multi-cursor where the text matches, stale replies dropped; no `completionItem/resolve`, no documentation pane, no snippet tab stops). Checked against the fake test server and, by hand in tmux, against rust-analyzer on this repo (completion checked October 30, 2026: `std::col` offered `collections::`, accepted and saved). Not started: code actions, rename, formatting, signature help |
| Configuration | done |
| Session restoration | done: open files, the split layout, each view's file, cursor and scroll, and the focused view are saved on quit and restored per directory; a view of a file that is gone is dropped and its split collapses. Not restored: multiple selections, horizontal scroll, unnamed buffers. Recent files across sessions (`space r`) |
| Crash-safe editing | done: atomic saves, external-change detection, recovery journal |

Acceptance (use Maestro to navigate and modify its own source without another editor): exercised once on October 30, 2026, for one real change. In the release binary in tmux, workspace search (`space /`) found the latency logger's `log.record` call in `crates/maestro-tui/src/lib.rs` and opened it at the match; `o` added `handle_time = Duration::ZERO;` with the right indentation; rust-analyzer flagged a missing semicolon in the gutter and status line, which was fixed in place; `:w` saved (the Git sign showed `+`); `gd` on `Duration` jumped into the standard library and `C-o` came back. The fix was then built and tested outside the editor. That is one session, not daily use, so M1 stays partial until the remaining partial rows are closed and Maestro has been used for real work over days.

## M2: agent development environment (partial)

| Item | Status |
|---|---|
| Worktree management | done: `maestro-git` lists, adds, removes (refusing, unless forced, on uncommitted or untracked files and on commits that are ahead of the upstream or, without one, on no other branch) and prunes worktrees. Each agent task gets a worktree on a `maestro/<slug>-<id>` branch from the current `HEAD`; archiving removes it and keeps the branch, and forcing needs a second confirmation that lists what would be deleted. Gutter signs, the branch in the status line, `:git-status` and `:worktrees` pickers |
| Agent terminals | done for the agent panel: terminal providers run in a PTY sized to the panel, drawn with their colors; `C-\` hands keys (and pastes) to the terminal and takes them back. Not shown in splits |
| One working agent integration | partial: the ACP path (session, prompts, plan, tool, permission and file events, feedback prompts) is tested against the fake ACP agent; the terminal path against shell scripts, in tests and by hand in tmux. No real agent (gemini, opencode, claude, codex) has been run through the panel on this branch |
| Sessions | not started: agent records live in memory and are forgotten on quit; worktrees and branches stay |
| Worktree-aware editing | done: `Enter` in the review opens the worktree's file at the line in a normal buffer; the status line says which agent's worktree it belongs to; integration and archiving refuse while such a buffer has unsaved edits |
| Diff review and approval | done: review screen per agent with file list, unified and side-by-side layouts, intraline marks, hunk and file navigation, line comments sent back as feedback; reject and integrate with confirmations |
| Test execution | done: `t` runs `[agents] test-command` (default `cargo test` with a `Cargo.toml`) in the worktree through `maestro-tasks`, with live output and parsed `cargo test` totals in the panel and the agent list |
| Safe merge or cherry-pick | done: integrate commits the worktree's work, then merges, fast-forwards or cherry-picks into the main worktree's branch through `maestro-git`'s checked `integrate`; a dirty or detached main worktree is refused before anything is committed |

Acceptance (launch two independent agent tasks, have them edit isolated worktrees, review both, reject one, safely integrate the other): `m2_acceptance_two_agents_reject_one_integrate_the_other` in `crates/maestro-editor/tests/agents.rs` drives it through `Editor::handle_key` with real repositories and worktrees, including the refusal with a dirty main worktree. On October 30, 2026 the release binary was driven through the same flow in tmux with two terminal providers written as shell scripts. M2 stays partial until a real agent has been run through the panel and agent sessions survive a restart.

## M3: differentiating capabilities (partial)

| Item | Status |
|---|---|
| Automation API | partial: protocol 1.0 over `--headless` (stdin/stdout) and `--listen` (Unix socket); version-checked `document.apply_edit` as one undo step; commands by name; change events. Not built: edit proposals with review, buffer and view management, Windows transport. See `docs/automation.md` |
| Provenance | partial: an in-memory per-buffer log of API edits (actor, description, versions, time). Not persisted; typing is not attributed |
| Task contracts | not started |
| Conflict detection | partial: stale edits are refused by version; no merge or rebase of a stale edit |
| Structural pipelines | not started |

Verified October 30, 2026: `headless_acceptance_open_edit_stale_save_history` in `crates/maestro-cli/tests/headless.rs` spawns `maestro --headless`, opens a file, applies a change against its version, has a second change against the old version refused as stale, saves, checks the file on disk and reads the actor from the history. The live socket was driven with `maestro --remote` against the interactive editor running in tmux.

## M4: advanced (not started)

DAP debugging, extensions, remote workspaces, replay, GUI.

## Machine-readable task list

`docs/tasks.toml` lists workstreams, owners and status for agents.
