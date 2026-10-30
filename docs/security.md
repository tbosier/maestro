# Security and trust

## What exists today

Maestro is currently a local editor with no network code, no plugin system and no subprocesses except the clipboard helpers below.

- Files are written atomically through a temp file in the same directory, created with `create_new` so an existing file of that name is never clobbered. Permissions of the original are copied to the new file. Symlinks are followed so the link survives.
- Binary files and files that are not valid in a supported encoding open read-only, so saving cannot rewrite bytes the user never saw.
- Recovery files contain file contents. The directory is created with mode 0700 and each file with mode 0600 on Unix. Unnamed buffers are not journaled.
- Sessions (`sessions/<hash of the directory>.toml`) and the recent-files list (`recent.toml`) in the state directory hold file paths and cursor positions, not contents. They are written atomically with mode 0600; the directories are created with, or tightened to, mode 0700, and the temp file is created exclusively so a file or symlink already at its name is never written through. `--clean` reads and writes neither; `restore-session = false` turns sessions off.
- The file finder and workspace search only read files; they do not follow symlinked directories, and they skip anything the repository's ignore files exclude. The preview opens regular files only, so a FIFO in the tree cannot hang the editor.
- Control characters are never written to the terminal from a cell: the renderer shows them as `?`, so a file name or file text containing an escape sequence cannot drive the terminal.
- Config is parsed with `deny_unknown_fields`, and key bindings that name unknown commands are rejected at startup.
- Clipboard: `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip` or `xsel` are run by name from `PATH` with fixed arguments. The text goes over stdin; it is never put on a command line. Without them, copying uses the OSC 52 terminal escape.
- No `unsafe` code; the workspace forbids it.

## Trust boundaries for what comes next

- Repository contents are untrusted. Maestro will not run tasks, hooks or language servers named by a repository without the user approving them for that workspace.
- Agent output is untrusted. Agent edits arrive as proposals checked against document versions, never as silent writes. Destructive shell commands are not auto-approved. Merges of agent work need explicit approval.
- A Git worktree isolates source, not privileges. An agent in a worktree can read anything the user can unless an OS sandbox is in use.
- Secrets: environment variables are not forwarded to agents by default, and tokens are not written to logs or config.
- No project contents go to a model provider unless the user configured that provider and authorized it for the workspace.

## Known gaps

- Saving a file owned by another user keeps the permission bits but not the owner.
- A file with several hard links loses its link to the others when saved, because the save replaces the directory entry.
