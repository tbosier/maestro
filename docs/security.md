# Security and trust

## What exists today

Maestro is currently a local editor with no network code and no plugin system. Its only subprocesses are the clipboard helpers and the language servers below.

- Files are written atomically through a temp file in the same directory, created with `create_new` so an existing file of that name is never clobbered. Permissions of the original are copied to the new file. Symlinks are followed so the link survives.
- Binary files and files that are not valid in a supported encoding open read-only, so saving cannot rewrite bytes the user never saw.
- Recovery files contain file contents. The directory is created with mode 0700 and each file with mode 0600 on Unix. Unnamed buffers are not journaled.
- Config is parsed with `deny_unknown_fields`, and key bindings that name unknown commands are rejected at startup.
- Clipboard: `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip` or `xsel` are run by name from `PATH` with fixed arguments. The text goes over stdin; it is never put on a command line. Without them, copying uses the OSC 52 terminal escape.
- Language servers are run by name from the user's config (`[language-server.<lang>]` in `config.toml`) or from built-in defaults (`rust-analyzer`, `pyright-langserver` or `pylsp`, `typescript-language-server`, `clangd`). Maestro never reads server names, commands or arguments from files inside a repository, so opening a file cannot choose what runs. The program is looked up on `PATH` (or used as given if the config names a path) and started with fixed arguments, the workspace root as its working directory, and the editor's environment. A server that is not installed is reported once and skipped. One that crashes stays stopped until `:lsp-restart`, so a broken server cannot restart in a loop.
- Language servers only start in the terminal front end. Editors built for tests or headless use leave `lsp.enabled` off.
- A server sees the full text of every open file in its language, plus whatever it reads from the workspace itself. Its stderr and log messages are kept in memory (the last 2,000 lines per server, shown by `:lsp-log`); they are never written to the terminal or to disk. Messages over 64 MiB are refused.
- Replies are applied on the editor thread and checked against the document version they were computed for. A reply for text that has since changed is dropped. Language servers cannot edit buffers in this version (no code actions, renames or formatting).
- No `unsafe` code; the workspace forbids it.

## Trust boundaries for what comes next

- Repository contents are untrusted. Maestro will not run tasks, hooks or language servers named by a repository without the user approving them for that workspace. Note that a trusted server can still act on repository contents: `rust-analyzer`, for example, runs `cargo` and build scripts from the project it indexes. Opening a Rust file from an untrusted repository with the default config runs that repository's build scripts. Set `enabled = false` under `[language-server.rust]` to avoid it.
- Agent output is untrusted. Agent edits arrive as proposals checked against document versions, never as silent writes. Destructive shell commands are not auto-approved. Merges of agent work need explicit approval.
- A Git worktree isolates source, not privileges. An agent in a worktree can read anything the user can unless an OS sandbox is in use.
- Secrets: environment variables are not forwarded to agents by default, and tokens are not written to logs or config.
- No project contents go to a model provider unless the user configured that provider and authorized it for the workspace.

## Known gaps

- Saving a file owned by another user keeps the permission bits but not the owner.
- A file with several hard links loses its link to the others when saved, because the save replaces the directory entry.
