# Security and trust

## What exists today

Maestro is currently a local editor with no network code and no plugin system. Its only subprocesses are the clipboard helpers, `git`, and the language servers, all described below. The automation API (below) listens only on a Unix socket, and only when started with `--listen`.

- Files are written atomically through a temp file in the same directory, created with `create_new` so an existing file of that name is never clobbered, and removed on failure only if this save created it. When an existing file is replaced, the temp file is created owner-only (0600) and gets the original's permissions just before the rename, so private contents are never briefly readable by others. Symlinks, including dangling ones, are followed so the link survives.
- Before overwriting, a save compares the file's metadata, identity (device and inode on Unix) and a hash of its bytes with what was last read or written. Any difference refuses the save until the user picks `:w!` or `:reload`.
- Binary files and files that are not valid in a supported encoding open read-only, so saving cannot rewrite bytes the user never saw.
- Recovery files contain file contents. The directory is created with mode 0700 and each file with mode 0600 on Unix. Unnamed buffers are not journaled. Each editor session writes and deletes only its own recovery files; another session's files are removed only by `:recover` (after copying the text into this session's journal) or `:recover-discard`.
- Config is parsed with `deny_unknown_fields`, and key bindings that name unknown commands are rejected at startup.
- Clipboard: `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip` or `xsel` are run by name from `PATH` with fixed arguments. The text goes over stdin; it is never put on a command line. Without them, copying uses the OSC 52 terminal escape.
- Git: `maestro-git` runs `git` from `PATH` with argument vectors, never a shell. Paths are separate arguments with `GIT_LITERAL_PATHSPECS=1`; revisions starting with `-` are refused and new branch names are checked with `git check-ref-format`. Read-only calls (status, diffs, base texts) set `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`. `GIT_TERMINAL_PROMPT=0` stops terminal password prompts; a configured askpass or credential helper can still run, and calls have no timeout.
- Removing a worktree refuses, unless the caller passes `force`, when `git status` reports uncommitted changes or untracked files, or when its `HEAD` has commits that are ahead of its upstream or (without one) on no other branch or remote-tracking branch. That last check also runs for a worktree whose directory is gone. The main and locked worktrees are never removed, even with `force`. Not detected: edits to files marked `assume-unchanged` or `skip-worktree`, and ignored files, which are deleted with the directory as `git worktree remove` does.
- Language servers are run by name from the user's config (`[language-server.<lang>]` in `config.toml`) or from built-in defaults (`rust-analyzer`, `pyright-langserver` or `pylsp`, `typescript-language-server`, `clangd`). Maestro never reads server names, commands or arguments from files inside a repository, so opening a file cannot choose what runs. The program is looked up in the absolute entries of `PATH`, or, if the config names a path, resolved to an absolute path against the directory Maestro started in, so the workspace root (the server's working directory) never changes which file runs. It is started with fixed arguments, the workspace root as its working directory, and the editor's environment. A server that is not installed is reported once and skipped. One that crashes stays stopped until `:lsp-restart`, so a broken server cannot restart in a loop.
- Language servers only start in the terminal front end. Editors built for tests or headless use leave `lsp.enabled` off.
- A server sees the full text of every open file in its language, plus whatever it reads from the workspace itself. Its stderr and log messages are kept in memory (the last 2,000 lines per server, shown by `:lsp-log`); they are never written to the terminal or to disk. Messages over 64 MiB are refused.
- Replies are applied on the editor thread. A definition, references or hover reply is dropped if any open buffer changed while it was being computed. Files a reply points into are read on a background thread, and only if they are regular files of at most 16 MiB, so a location cannot make the editor block on a FIFO or device. Language servers cannot edit buffers in this version (no code actions, renames or formatting).
- No `unsafe` code; the workspace forbids it.

## Automation API

`maestro --headless` talks only over its own stdin and stdout, so whoever started the process is the only client. `maestro --listen` serves a Unix socket while the editor runs. The trust model for the socket:

- It is local only. There is no TCP listener.
- The socket file has mode 0600 and the default location is `$XDG_RUNTIME_DIR/maestro/<pid>.sock`, a per-user directory. A missing parent directory is created with mode 0700. The socket is bound inside a fresh 0700 directory, set to 0600 and then hard-linked into place, so it never exists with the umask's looser permissions. Linking fails if anything appeared at the path in the meantime, so a file or another editor's socket there is never replaced. There is no fallback to `/tmp`.
- File permissions are the only access control. There is no token or other authentication, so any process running as the same user can connect and do anything the API allows, including editing and saving any open file. This is the same trust a process running as the user already has over the user's files.
- Peer credentials are not checked. On Linux only the owner (and root) can connect to a 0600 socket anyway.
- `document.apply_edit` checks the document version and refuses a stale edit without changing anything. It is the only version-checked way to change text. A client can also change text with `command.run` (for example `delete_selection`) or `editor.execute` (for example `:%s`); those act on the editor's current state with no version check and are not recorded in `document.history`. Every change applies immediately; there is no review step yet.
- `document.open` and `document.save` accept any path the user can read or write, including paths outside the workspace root. A client can therefore read and overwrite files outside the project. A workspace boundary is planned with the permission system.
- The edit log (`document.history`) records the actor string the client sent. The server does not verify it; any client can claim any actor name.
- Writes to a client happen on the editor's thread. Each message has a 2-second deadline in total; a client that misses it is disconnected. A client that stops reading can therefore freeze the editor for up to 2 seconds per message, and several such clients add up.
- A request line longer than 64 MiB gets an error and the connection is closed. Each connection may have at most 32 requests queued for the editor; past that its reader stops reading until the editor catches up.
- `document.open` refuses paths that exist and are not regular files (FIFOs, sockets, devices), because reading one could block the editor thread with no way to cancel.
- The socket is removed when the editor exits normally. After a crash it stays behind; the next `--listen` on that path replaces it only if connecting is refused (nobody listening). Any other connection error is reported and the file is left alone. Between that check and the removal there is a small window in which another process could put a new socket there and have it removed.
- Windows has no socket transport yet; `--listen` is refused there.

## Trust boundaries for what comes next

- Repository contents are untrusted. Maestro will not run tasks, hooks or language servers named by a repository without the user approving them for that workspace. Note that a trusted server can still act on repository contents: `rust-analyzer`, for example, runs `cargo` and build scripts from the project it indexes. Opening a Rust file from an untrusted repository with the default config runs that repository's build scripts. Set `enabled = false` under `[language-server.rust]` to avoid it.
- Agent output is untrusted. Agent edits arrive as proposals checked against document versions, never as silent writes. Destructive shell commands are not auto-approved. Merges of agent work need explicit approval.
- A Git worktree isolates source, not privileges. An agent in a worktree can read anything the user can unless an OS sandbox is in use.
- Secrets: environment variables are not forwarded to agents by default, and tokens are not written to logs or config.
- No project contents go to a model provider unless the user configured that provider and authorized it for the workspace.

## Known gaps

- `git` runs with the user's configuration, including the repository's own `.git/config`. A repository unpacked from an archive with a hostile `.git/config` could name a command in `core.fsmonitor`, which `git status` (run for gutter signs and the status picker) would execute. `git worktree add` runs the repository's `post-checkout` hook. This is the same exposure as a shell prompt that runs `git status`; Maestro does not yet ask before running Git in a newly opened repository.

- Saving a file owned by another user keeps the permission bits but not the owner.
- A file with several hard links loses its link to the others when saved, because the save replaces the directory entry.
- Diagnostics published without a document version are placed on the current text. If the server computed them before your latest edit, they can land a few characters off until it publishes again. Versioned diagnostics for an older version are dropped.
- Ropey counts form feed, vertical tab, NEL and the Unicode line and paragraph separators as line breaks; the protocol counts only CR, LF and CRLF. A file containing those characters gets line numbers that disagree with the server's, and the server's copy of the text drifts after edits below them. Fixing it means building Ropey without its `unicode_lines` feature.
- A server that floods notifications can keep the editor busy: diagnostics are queued as jobs without a limit.
