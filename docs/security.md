# Security and trust

## What exists today

Maestro is currently a local editor with no network code, no plugin system and no subprocesses except the clipboard helpers and `git` below.

- Files are written atomically through a temp file in the same directory, created with `create_new` so an existing file of that name is never clobbered. Permissions of the original are copied to the new file. Symlinks are followed so the link survives.
- Binary files and files that are not valid in a supported encoding open read-only, so saving cannot rewrite bytes the user never saw.
- Recovery files contain file contents. The directory is created with mode 0700 and each file with mode 0600 on Unix. Unnamed buffers are not journaled.
- Config is parsed with `deny_unknown_fields`, and key bindings that name unknown commands are rejected at startup.
- Clipboard: `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip` or `xsel` are run by name from `PATH` with fixed arguments. The text goes over stdin; it is never put on a command line. Without them, copying uses the OSC 52 terminal escape.
- Git: `maestro-git` runs `git` from `PATH` with argument vectors, never a shell. Paths are separate arguments with `GIT_LITERAL_PATHSPECS=1`; revisions starting with `-` are refused and new branch names are checked with `git check-ref-format`. Read-only calls (status, diffs, base texts) set `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`. `GIT_TERMINAL_PROMPT=0` stops terminal password prompts; a configured askpass or credential helper can still run, and calls have no timeout.
- Removing a worktree refuses, unless the caller passes `force`, when `git status` reports uncommitted changes or untracked files, or when its `HEAD` has commits that are ahead of its upstream or (without one) on no other branch or remote-tracking branch. That last check also runs for a worktree whose directory is gone. The main and locked worktrees are never removed, even with `force`. Not detected: edits to files marked `assume-unchanged` or `skip-worktree`, and ignored files, which are deleted with the directory as `git worktree remove` does.
- No `unsafe` code; the workspace forbids it.

## Trust boundaries for what comes next

- Repository contents are untrusted. Maestro will not run tasks, hooks or language servers named by a repository without the user approving them for that workspace.
- Agent output is untrusted. Agent edits arrive as proposals checked against document versions, never as silent writes. Destructive shell commands are not auto-approved. Merges of agent work need explicit approval.
- A Git worktree isolates source, not privileges. An agent in a worktree can read anything the user can unless an OS sandbox is in use.
- Secrets: environment variables are not forwarded to agents by default, and tokens are not written to logs or config.
- No project contents go to a model provider unless the user configured that provider and authorized it for the workspace.

## Known gaps

- `git` runs with the user's configuration, including the repository's own `.git/config`. A repository unpacked from an archive with a hostile `.git/config` could name a command in `core.fsmonitor`, which `git status` (run for gutter signs and the status picker) would execute. `git worktree add` runs the repository's `post-checkout` hook. This is the same exposure as a shell prompt that runs `git status`; Maestro does not yet ask before running Git in a newly opened repository.

- Saving a file owned by another user keeps the permission bits but not the owner.
- A file with several hard links loses its link to the others when saved, because the save replaces the directory entry.
