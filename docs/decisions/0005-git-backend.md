# 0005: The git command line as the Git backend

Date: October 30, 2026. Status: accepted.

## Decision

`maestro-git` runs the `git` program as a subprocess with argument vectors and parses machine-readable output (`status --porcelain=v2 -z`, `worktree list --porcelain -z`, `ls-files --stage -z`, `merge-tree --write-tree -z`). It does not link libgit2 (the `git2` crate) or use gitoxide (`gix`). Line diffs for gutter signs are computed in process with the `similar` crate; only the base text comes from Git.

## Why

- Worktrees are the reason this crate exists, and `git worktree add/remove/prune/lock` are features of Git itself. A library reimplements them and has to keep up with the on-disk format (`.git/worktrees/<name>`, `gitdir` files, lock and prunable markers). The CLI is the reference implementation.
- The merge dry run must agree with the merge the user will actually run. `git merge-tree --write-tree` (Git 2.38+) uses Git's own merge machinery. libgit2 has its own merge implementation, so a dry run there could disagree with a real `git merge`.
- The user's Git configuration (hooks, `core.fsmonitor`, `safe.directory`, credential helpers, includes) applies without Maestro having to emulate it.
- No C dependency. `git2` builds libgit2 from source (or links a system copy). `gix` is pure Rust; we did not evaluate or test its worktree and merge support, so this decision says nothing about what it can do today.
- The cost is a process per call. Measured on the machine in `docs/performance.md` with `cargo test --release -p maestro-editor measure_open_to_signs -- --ignored --nocapture`: opening `crates/maestro-editor/src/editor.rs` until its signs and branch are applied took a median of 6.0 ms (max 6.3 ms over 20 runs). That covers four `git` processes (`rev-parse`, `symbolic-ref`, `ls-files`, `cat-file`). Typing does not start `git`: the base text is cached, and the line diff of that 1,291-line file with 50 edited lines took 0.13 ms. The base is read again on open, save, rename, a change of comparison base, a change to the repository's index or `HEAD`, and when an edit or undo brings the buffer back to its saved text.

## How it is used

- Never through a shell. Paths and revisions are separate arguments. Revisions that start with `-` are refused, branch names go through `git check-ref-format --branch`, and `GIT_LITERAL_PATHSPECS=1` makes path arguments plain names.
- Read-only calls set `GIT_OPTIONAL_LOCKS=0`, so `git status` from the editor never takes the index lock and never makes the user's own Git command fail.
- `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` and similar variables are cleared so a repository is always found from the path, even when Maestro was started from a Git hook.
- `GIT_TERMINAL_PROMPT=0`, `--no-pager` and a null stdin, so Git does not prompt on the terminal. A configured askpass or credential helper can still run, and there is no timeout. Read-only calls also set `GIT_NO_LAZY_FETCH=1` (Git 2.44+), so a partial clone does not fetch missing blobs over the network for a gutter refresh.
- Every call blocks; the editor runs them on background threads. A Git call that hangs stops sign updates for that buffer until it returns.

## Consequences

- Git must be installed. Without it, buffers have no signs and the Git commands report that Git was not found.
- Version-dependent features check `git --version`: `worktree list -z` needs 2.36 (older versions fall back to newline-separated output, which cannot represent a worktree path containing a newline) and the merge dry run needs 2.38 (older versions get a clear `Unsupported` error).
- Repository discovery reads `git rev-parse` output one path per line, so a repository whose path contains a newline is reported as an error.
- The user's Git configuration runs. A repository's local `.git/config` can name commands (`core.fsmonitor`, hooks such as `post-checkout` on `worktree add`). That is the same exposure as running `git status` in a shell prompt; `docs/security.md` records it.
- Revisit if per-call process cost shows up in profiles (for example a status refresh on every keystroke across thousands of worktrees), or if `gix` covers worktree lifecycle and merges with Git's own semantics.
