# Security and trust

## What exists today

Maestro is currently a local editor with no network code, no plugin system and no subprocesses except the clipboard helpers below. The automation API (below) listens only on a Unix socket, and only when started with `--listen`.

- Files are written atomically through a temp file in the same directory, created with `create_new` so an existing file of that name is never clobbered. Permissions of the original are copied to the new file. Symlinks are followed so the link survives.
- Binary files and files that are not valid in a supported encoding open read-only, so saving cannot rewrite bytes the user never saw.
- Recovery files contain file contents. The directory is created with mode 0700 and each file with mode 0600 on Unix. Unnamed buffers are not journaled.
- Config is parsed with `deny_unknown_fields`, and key bindings that name unknown commands are rejected at startup.
- Clipboard: `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`, `xclip` or `xsel` are run by name from `PATH` with fixed arguments. The text goes over stdin; it is never put on a command line. Without them, copying uses the OSC 52 terminal escape.
- No `unsafe` code; the workspace forbids it.

## Automation API

`maestro --headless` talks only over its own stdin and stdout, so whoever started the process is the only client. `maestro --listen` serves a Unix socket while the editor runs. The trust model for the socket:

- It is local only. There is no TCP listener.
- The socket file has mode 0600 and the default location is `$XDG_RUNTIME_DIR/maestro/<pid>.sock`, a per-user directory. A missing parent directory is created with mode 0700. The socket is bound inside a fresh 0700 directory, set to 0600 and then renamed into place, so it never exists with the umask's looser permissions. There is no fallback to `/tmp`.
- File permissions are the only access control. There is no token or other authentication, so any process running as the same user can connect and do anything the API allows, including editing and saving any open file. This is the same trust a process running as the user already has over the user's files.
- Peer credentials are not checked. On Linux only the owner (and root) can connect to a 0600 socket anyway.
- Agent edits change text only through `document.apply_edit`, which checks the document version and refuses a stale edit without changing anything. The edit is applied immediately; there is no review step yet.
- `document.open` and `document.save` accept any path the user can read or write, including paths outside the workspace root. A client can therefore read and overwrite files outside the project. A workspace boundary is planned with the permission system.
- The edit log (`document.history`) records the actor string the client sent. The server does not verify it; any client can claim any actor name.
- A client that stops reading is disconnected after a 2-second write timeout, so it cannot hang the editor.
- The socket is removed when the editor exits normally. After a crash it stays behind; the next `--listen` on that path detects that nobody is listening and replaces it.
- Windows has no socket transport yet; `--listen` is refused there.

## Trust boundaries for what comes next

- Repository contents are untrusted. Maestro will not run tasks, hooks or language servers named by a repository without the user approving them for that workspace.
- Agent output is untrusted. Agent edits arrive as proposals checked against document versions, never as silent writes. Destructive shell commands are not auto-approved. Merges of agent work need explicit approval.
- A Git worktree isolates source, not privileges. An agent in a worktree can read anything the user can unless an OS sandbox is in use.
- Secrets: environment variables are not forwarded to agents by default, and tokens are not written to logs or config.
- No project contents go to a model provider unless the user configured that provider and authorized it for the workspace.

## Known gaps

- Saving a file owned by another user keeps the permission bits but not the owner.
- A file with several hard links loses its link to the others when saved, because the save replaces the directory entry.
