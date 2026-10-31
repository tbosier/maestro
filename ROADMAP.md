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
| LSP diagnostics and navigation | partial: diagnostics (sign column, range underline, cursor-line message, `]d`/`[d`, `space d`/`space D` pickers), goto definition (`gd`), references (`gr`), hover (`space k`), incremental sync, crash reporting with `:lsp-restart`. Checked against the fake test server and, by hand in tmux, against rust-analyzer on this repo. Not started: completion, code actions, rename, formatting, signature help |
| Configuration | done |
| Session restoration | done: open files, the split layout, each view's file, cursor and scroll, and the focused view are saved on quit and restored per directory; a view of a file that is gone is dropped and its split collapses. Not restored: multiple selections, horizontal scroll, unnamed buffers. Recent files across sessions (`space r`) |
| Crash-safe editing | done: atomic saves, external-change detection, recovery journal |

Acceptance (use Maestro to navigate and modify its own source without another editor): exercised once on October 30, 2026, for one real change. In the release binary in tmux, workspace search (`space /`) found the latency logger's `log.record` call in `crates/maestro-tui/src/lib.rs` and opened it at the match; `o` added `handle_time = Duration::ZERO;` with the right indentation; rust-analyzer flagged a missing semicolon in the gutter and status line, which was fixed in place; `:w` saved (the Git sign showed `+`); `gd` on `Duration` jumped into the standard library and `C-o` came back. The fix was then built and tested outside the editor. That is one session, not daily use, so M1 stays partial until the remaining partial rows are closed and Maestro has been used for real work over days.

## M2: agent development environment (partial)

| Item | Status |
|---|---|
| Worktree management | partial: crate and gutter, no agent UI. `maestro-git` lists, adds, removes (refusing, unless forced, on uncommitted or untracked files and on commits that are ahead of the upstream or, without one, on no other branch) and prunes worktrees, and checks fast-forwards and merge conflicts without touching the working tree. The editor shows `+`/`~`/`-` gutter signs, the branch in the status line, and `:git-status` and `:worktrees` pickers. |
| Agent terminals | not started |
| One working agent integration | not started |
| Sessions | not started |
| Worktree-aware editing | not started |
| Diff review and approval | not started; the unified diff model it needs exists in `maestro-git` |
| Test execution | not started |
| Safe merge or cherry-pick | not started; `can_fast_forward` and the `merge-tree` dry run exist in `maestro-git` |

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
