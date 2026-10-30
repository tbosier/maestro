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
| Fuzzy file finder | partial: a synchronous walker capped at 50,000 files that skips hidden and build directories; not gitignore-aware |
| Search and replace | done in the buffer; workspace search not started |
| Tree-sitter highlighting | not started |
| LSP diagnostics and navigation | not started |
| Configuration | done |
| Session restoration | not started |
| Crash-safe editing | done: atomic saves, external-change detection, recovery journal |

Acceptance (use Maestro to navigate and modify its own source without another editor) has not been attempted. It needs highlighting, workspace search and goto-definition first.

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

## M3: differentiating capabilities (not started)

The foundation exists: version-checked transactions in `maestro-core` refuse stale edits. Not started: task contracts, the automation API, provenance, conflict detection, structural pipelines.

## M4: advanced (not started)

DAP debugging, extensions, remote workspaces, replay, GUI.

## Machine-readable task list

`docs/tasks.toml` lists workstreams, owners and status for agents.
