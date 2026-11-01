# Automation API

Scripts and agents drive Maestro through a JSON-RPC 2.0 protocol instead of simulated keystrokes. This is the M3 slice: version-checked edits, edit proposals that wait for the person at the editor to approve them and can run a verification command, an edit log per buffer, named commands and change events.

- Wire types: `crates/maestro-protocol` (serde only, no editor dependency, so clients can depend on it).
- Server and transports: `crates/maestro-automation`.
- Protocol version: `1.2`. Version 1.1 added the `proposal.*` methods, the `proposal.resolved` notification, error codes -32018 and -32019, and two fields in `document.history` (`proposal` on an edit, and `proposals`). Version 1.2 limits `proposal.withdraw` to the connection that created the proposal and added error code -32020. A 1.0 or 1.1 client keeps working unless it withdrew proposals another connection created.

## Transports

Every message is one JSON object on one line (newline-delimited JSON). Responses to one connection come back in request order.

### Headless: `maestro --headless`

Runs an editor with no terminal. Requests arrive on stdin, responses and notifications go to stdout, and diagnostics go to stderr. The process exits with status 0 at end of input or after `editor.quit` (or a `:q` through `editor.execute`). File arguments open as usual, and the user config loads unless `--clean` is given. Headless runs start no language servers and read or write no session or recent-files state. In the interactive editor, `editor.quit` saves the session as `:q` does.

```
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"1.0"}}' | maestro --headless --clean
maestro: headless automation server, protocol 1.0; send newline-delimited JSON-RPC on stdin
{"jsonrpc":"2.0","id":1,"result":{...}}
```

The headless editor has an 80 by 24 screen for scrolling purposes, does not write recovery files, and uses an in-memory clipboard.

Nobody can approve a proposal in a headless run, so `proposal.create` is refused there with -32018 and nothing is queued or run. There is no flag to approve proposals automatically. A script that wants to change text headless uses `document.apply_edit`, which applies at once.

### Live: `maestro --listen <path|auto>`

Serves the API on a Unix socket while the interactive editor runs. `auto` means `$XDG_RUNTIME_DIR/maestro/<pid>.sock`; there is no fallback to `/tmp` when `XDG_RUNTIME_DIR` is unset. The status line shows the socket path at startup. The socket is created with mode 0600 (a missing parent directory is created 0700) and removed when the editor exits. A stale socket left by a crash (connecting is refused) is replaced; a live one, a non-socket file, or anything that appears at the path while the socket is being set up is refused. There is no TCP listener.

Each request runs on the editor's thread as a background job, the same path other background work uses, so requests and keystrokes never run at the same time. The terminal loop checks for jobs once a second when idle and every 16 ms while a client is connected, so the first request on a new connection can wait up to a second. Later requests on that connection, and new connections made within about 100 ms of the last one closing, are answered in milliseconds (18 ms for a `--remote` round trip in the run below, process start included).

Windows: `--listen` and `--remote` are refused with a message ("not supported yet"). `--headless` is platform-independent. A named-pipe transport is a TODO in `crates/maestro-automation/src/cli.rs`.

### One-shot client: `maestro --remote <socket> '<json>'`

Connects, sends `initialize` for you (unless your request is itself an `initialize`), sends your request, prints the response line, and skips notifications. Exit status: 0 for a result, 1 for an error response, 2 if the editor could not be reached. A request without an `id` is sent and nothing is printed.

This run drove the interactive editor in tmux (`maestro --clean --listen /tmp/claude-1000/mlive/run/ed.sock main.rs`) on October 30, 2026:

```
$ maestro --remote /tmp/claude-1000/mlive/run/ed.sock '{"jsonrpc":"2.0","id":2,"method":"document.apply_edit","params":{"doc":3,"base_version":0,"encoding":"lsp","edits":[{"range":{"start":{"line":1,"character":14},"end":{"line":1,"character":19}},"text":"from the API"}],"actor":"agent:demo","description":"change the greeting"}}'
{"jsonrpc":"2.0","id":2,"result":{"base_version":0,"doc":3,"version":1}}

$ maestro --remote /tmp/claude-1000/mlive/run/ed.sock '{"jsonrpc":"2.0","id":3,"method":"document.apply_edit","params":{"doc":3,"base_version":0,"encoding":"char","edits":[{"range":{"start":0,"end":2}, "text":"pub fn"}],"actor":"agent:late"}}'
{"jsonrpc":"2.0","id":3,"error":{"code":-32010,"message":"edit was computed against version 0 but the document is at 1; nothing was changed","data":{"current":1,"expected":0}}}
```

The terminal showed `println!("from the API");` and the `[+]` modified marker right after the first call. `document.save` then `editor.quit` wrote the file, closed the editor and removed the socket file.

## Messages

Requests: `{"jsonrpc": "2.0", "id": <number|string>, "method": "...", "params": {...}}`. `params` may be omitted when every field has a default. A request without `id` is a JSON-RPC notification: it runs and gets no response.

Responses: `{"jsonrpc": "2.0", "id": ..., "result": ...}` or `{"jsonrpc": "2.0", "id": ..., "error": {"code", "message", "data"?}}`. `id` is `null` only when the line could not be parsed far enough to read it.

Notifications from the server: `{"jsonrpc": "2.0", "method": "...", "params": {...}}`, sent only after `events.subscribe`.

### Session rules

`initialize` comes first on every connection. Any other method before it fails with `-32002`. The client sends `protocol_version` as `MAJOR.MINOR`. A different major version is refused with `-32003`; any minor version of major 1 is accepted, and the result states the version the server speaks. Calling `initialize` twice on one connection is an error.

### Positions

Every request that carries positions also carries `encoding`:

- `"char"`: a number, the offset in Unicode scalar values (Rust `char`s) from the start of the document.
- `"lsp"`: `{"line": n, "character": n}`, zero-based line and UTF-16 code unit offset, as in the Language Server Protocol. Following LSP, a `character` past the end of a line clamps to the line end (before the line break), and a position inside a surrogate pair rounds down to the start of the character. A `line` past the last line is out of bounds.

A position in the other encoding than the one named is refused with `-32602`. Ranges are half-open (`start..end`). A char position may fall between `\r` and `\n` or inside a grapheme cluster; the edit is applied as given.

Selections use the editor's model: a range covers every grapheme from `min(anchor, head)` through the grapheme that starts at `max(anchor, head)`, so a collapsed range still covers the character under the cursor. Results give each end both ways (`anchor`, `head` as chars, `anchor_lsp`, `head_lsp`).

## Errors

| Code | Name | When |
|---|---|---|
| -32700 | parse error | the line is not JSON or not UTF-8 |
| -32600 | invalid request | not a request object, `jsonrpc` is not `"2.0"`, or a second `initialize` |
| -32601 | method not found | unknown method |
| -32602 | invalid params | missing or malformed params, positions out of bounds or in the wrong encoding, overlapping or reversed edits, empty edits or actor, edits that change nothing, a path that is not a regular file |
| -32603 | internal | a bug in the server |
| -32002 | not initialized | a method before `initialize` |
| -32003 | unsupported protocol version | `initialize` with another major version; `data` is `{"requested", "supported"}` |
| -32010 | stale version | `base_version` is not the current version (`document.apply_edit`, `proposal.create`); `data` is `{"expected", "current"}`; nothing changed |
| -32011 | not found | no such document, view, command, file or proposal |
| -32012 | read-only | the document opened read-only (binary, undecodable or unwritable) |
| -32013 | permission denied | the operating system refused a file operation |
| -32014 | conflict | the file changed on disk since it was read; save again with `force` |
| -32015 | unsaved changes | `editor.quit` with modified buffers; `data` lists them |
| -32016 | command failed | a command reported an error in the status line, needed a prompt, picker or typed character, or the user was in the middle of input; `editor.quit` while agents, their tests or their Git work are running |
| -32017 | I/O | any other file error |
| -32018 | review unavailable | `proposal.create` where nobody can review it (headless), or with 100 proposals already pending; nothing was queued |
| -32019 | not pending | `proposal.withdraw` on a proposal that was already decided |
| -32020 | not owner | `proposal.withdraw` from a connection other than the one that created the proposal; nothing changed |

## Methods

The pairs below are real. The headless ones come from one `maestro --headless --clean` session run in `/tmp/claude-1000/demo` on October 30, 2026, with `greet.txt` containing `hello world\n`; long results are cut where marked.

### initialize

Params: `protocol_version` (string), `client_name` (optional). Result: `protocol_version`, `server_name`, `server_version`, `methods`, `notifications`, `capabilities`.

```
--> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2.0"}}
<-- {"jsonrpc":"2.0","id":1,"error":{"code":-32003,"message":"this server speaks protocol 1.0; version 2.0 is not supported","data":{"requested":"2.0","supported":"1.0"}}}
--> {"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocol_version":"1.0","client_name":"docs"}}
<-- {"jsonrpc":"2.0","id":2,"result":{"capabilities":["apply_edit.char_positions","apply_edit.lsp_positions","apply_edit.version_check","document.history","events.document_changed","events.editor_status"],"methods":["initialize","editor.snapshot","editor.execute","editor.quit","document.open","document.text","document.save","document.apply_edit","document.history","selections.get","selections.set","command.run","command.list","events.subscribe"],"notifications":["document.changed","editor.status"],"protocol_version":"1.0","server_name":"maestro","server_version":"0.1.0"}}
```

That exchange is from protocol 1.0. A 1.1 server, headless, answered a 1.0 client on October 31, 2026 with the block below; on November 1, 2026 a 1.2 server answered a 1.1 client with the same result except `"protocol_version":"1.2"`:

```
<-- {"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.1","server_name":"maestro","server_version":"0.1.0","methods":["initialize","editor.snapshot","editor.execute","editor.quit","document.open","document.text","document.save","document.apply_edit","document.history","selections.get","selections.set","command.run","command.list","events.subscribe","proposal.create","proposal.status","proposal.list","proposal.withdraw"],"notifications":["document.changed","editor.status","proposal.resolved"],"capabilities":["apply_edit.char_positions","apply_edit.lsp_positions","apply_edit.version_check","document.history","events.document_changed","events.editor_status"]}}
```

The interactive editor behind `--listen` adds two capabilities, `proposal.review` and `proposal.verification`. Check for `proposal.review` before creating proposals: without it, `proposal.create` is refused.

### document.open

Params: `path` (absolute, or relative to the editor's working directory; `~` expands), `focus` (default false). Without `focus` the user's view does not move. A path that does not exist opens as an empty document that creates the file on first save, as `:e` does. A path that exists but is not a regular file (a FIFO, socket or device) is refused with `-32602`. Result: `doc`, `path`, `version`, `read_only`, `already_open`.

```
--> {"jsonrpc":"2.0","id":3,"method":"document.open","params":{"path":"greet.txt","focus":true}}
<-- {"jsonrpc":"2.0","id":3,"result":{"already_open":false,"doc":3,"path":"/tmp/claude-1000/demo/greet.txt","read_only":null,"version":0}}
```

### document.text

Params: `doc`, optional `range` (requires `encoding`). Result: `doc`, `version`, `text`.

```
--> {"jsonrpc":"2.0","id":4,"method":"document.text","params":{"doc":3}}
<-- {"jsonrpc":"2.0","id":4,"result":{"doc":3,"text":"hello world\n","version":0}}
--> {"jsonrpc":"2.0","id":10,"method":"document.text","params":{"doc":3,"encoding":"lsp","range":{"start":{"line":0,"character":12},"end":{"line":0,"character":14}}}}
<-- {"jsonrpc":"2.0","id":10,"result":{"doc":3,"text":"😀","version":2}}
```

### document.apply_edit

Params: `doc`, `base_version`, `encoding`, `edits` (a list of `{range, text}` against the document at `base_version`, in any order, touching allowed, overlapping refused), `actor` (non-empty string), `description` (optional). Result: `doc`, `base_version`, `version`.

The edits apply as one transaction through `Document::apply_at_version`, so a stale `base_version` changes nothing. Any undo group the user had open is closed first, then the edit is committed as its own group, so one undo (`u`, or `command.run` `undo` while that document is focused) reverts exactly this edit. Selections in every view of the document move with the text. Text is inserted as given; line endings are not converted. A batch that would change nothing (for example replacing an empty range with empty text) is refused with `-32602`, since it would create no version and no undo step.

```
--> {"jsonrpc":"2.0","id":6,"method":"document.apply_edit","params":{"doc":3,"base_version":0,"encoding":"char","edits":[{"range":{"start":6,"end":11},"text":"there"}],"actor":"agent:docs","description":"greet someone closer"}}
<-- {"jsonrpc":"2.0","id":6,"result":{"base_version":0,"doc":3,"version":1}}
<-- {"jsonrpc":"2.0","method":"document.changed","params":{"actor":"agent:docs","doc":3,"version":1}}
--> {"jsonrpc":"2.0","id":7,"method":"document.apply_edit","params":{"doc":3,"base_version":0,"encoding":"char","edits":[{"range":{"start":0,"end":5},"text":"goodbye"}],"actor":"agent:late"}}
<-- {"jsonrpc":"2.0","id":7,"error":{"code":-32010,"message":"edit was computed against version 0 but the document is at 1; nothing was changed","data":{"current":1,"expected":0}}}
--> {"jsonrpc":"2.0","id":8,"method":"document.apply_edit","params":{"doc":3,"base_version":1,"encoding":"char","edits":[{"range":{"start":0,"end":4},"text":"x"},{"range":{"start":2,"end":6},"text":"y"}],"actor":"agent:docs"}}
<-- {"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"edits[0] (0..4) and edits[1] (2..6) overlap"}}
--> {"jsonrpc":"2.0","id":9,"method":"document.apply_edit","params":{"doc":3,"base_version":1,"encoding":"lsp","edits":[{"range":{"start":{"line":0,"character":11},"end":{"line":0,"character":11}},"text":" 😀"}],"actor":"agent:docs"}}
<-- {"jsonrpc":"2.0","id":9,"result":{"base_version":1,"doc":3,"version":2}}
<-- {"jsonrpc":"2.0","method":"document.changed","params":{"actor":"agent:docs","doc":3,"version":2}}
```

(The `document.changed` lines appear because the session called `events.subscribe` as request 5.)

### document.history

Params: `doc`. Result: `doc`, `edits` and `proposals`.

`edits` lists, oldest first (by `new_version`), the edits made through `document.apply_edit` and approved proposals: `actor`, `description`, `base_version`, `new_version`, `timestamp_ms` (milliseconds since the Unix epoch, from the editor's clock), and `proposal` (the proposal's id) on an edit that came from one. Typing, commands, undo and reloads are not listed. For `document.apply_edit` the log stores what the request gave and nothing else; it lives in memory, is dropped when the buffer closes, and keeps the most recent 10,000 records per buffer.

`proposals` lists, oldest first, every proposal for the document that was decided (approved, rejected, withdrawn, stale or failed), in the shape `proposal.status` returns: who proposed it, the decision and who made it, the versions, the verification command and its exit code, duration and output tail. The editor keeps the last 500 decided proposals in memory; none of this is written to disk.

```
--> {"jsonrpc":"2.0","id":18,"method":"document.history","params":{"doc":3}}
<-- {"jsonrpc":"2.0","id":18,"result":{"doc":3,"edits":[{"actor":"agent:docs","base_version":0,"description":"greet someone closer","new_version":1,"timestamp_ms":1793398466513},{"actor":"agent:docs","base_version":1,"description":null,"new_version":2,"timestamp_ms":1793398467415}]}}
```

### document.save

Params: `doc`, `force` (default false). Saves atomically like `:w`. A file that changed on disk since it was read is a `-32014` conflict unless `force` is set. A scratch buffer has no path and gets `-32602`. Result: `doc`, `path`, `version`.

```
--> {"jsonrpc":"2.0","id":20,"method":"document.save","params":{"doc":3}}
<-- {"jsonrpc":"2.0","id":20,"result":{"doc":3,"path":"/tmp/claude-1000/demo/greet.txt","version":3}}
```

The file then held `HELLO there 😀\n`.

### editor.snapshot

No params. Result: `buffers` (`id`, `path`, `name`, `version`, `modified`, `read_only`, `line_count`, `char_count`), `views` (`id`, `doc`, `selections`, `primary`, `top_line`), `focused_view`, `mode` (`normal`, `insert` or `select`) and `status` (`{severity, message}` or `null`). `line_count` counts lines as the editor shows them: a trailing newline does not add one.

```
--> {"jsonrpc":"2.0","id":12,"method":"editor.snapshot"}
<-- {"jsonrpc":"2.0","id":12,"result":{"buffers":[{"char_count":14,"id":3,"line_count":1,"modified":true,"name":"/tmp/claude-1000/demo/greet.txt","path":"/tmp/claude-1000/demo/greet.txt","read_only":null,"version":2}],"focused_view":2,"mode":"normal","status":null,"views":[{"doc":3,"id":2,"primary":0,"selections":[{"anchor":12,"anchor_lsp":{"character":12,"line":0},"head":12,"head_lsp":{"character":12,"line":0}}],"top_line":0}]}}
```

### selections.get, selections.set

`selections.get` params: `view`. `selections.set` params: `view`, `encoding`, `ranges` (`[{anchor, head}]`, at least one), `primary` (index, default the last range). The editor normalizes the result (sorted, overlapping ranges merged, ends snapped to grapheme boundaries, and kept off the end-of-text position outside insert mode), so read the returned selections back. Result: `view`, `doc`, `version`, `selections`, `primary`.

```
--> {"jsonrpc":"2.0","id":13,"method":"selections.get","params":{"view":2}}
<-- {"jsonrpc":"2.0","id":13,"result":{"doc":3,"primary":0,"selections":[{"anchor":12,"anchor_lsp":{"character":12,"line":0},"head":12,"head_lsp":{"character":12,"line":0}}],"version":2,"view":2}}
--> {"jsonrpc":"2.0","id":14,"method":"selections.set","params":{"view":2,"encoding":"char","ranges":[{"anchor":0,"head":4}]}}
<-- {"jsonrpc":"2.0","id":14,"result":{"doc":3,"primary":0,"selections":[{"anchor":0,"anchor_lsp":{"character":0,"line":0},"head":4,"head_lsp":{"character":4,"line":0}}],"version":2,"view":2}}
```

### command.run

Params: `name` (a registry command or a `:` command name), `count` (optional, at least 1). Runs on the focused view with the same bookkeeping a key press gets (the undo group closes outside insert mode). The status line is cleared first. Result: `mode` and `status`. While the user is in the middle of input (a prompt, a picker, a pending typed character, a count or a partial key sequence) the command is not run and the request fails with `-32016`; the user's input is left as it was. Commands are not version-checked and are not recorded in `document.history`. A command that reports an error, or that opens a prompt, a picker or waits for a typed character (`f`, `r`, `/`), fails with `-32016`; the interactive part is canceled with Esc so the editor is not left waiting. `goto_definition`, `goto_references` and `hover` are refused up front with `-32016`: their result arrives later from a language server as a jump, picker or popup, after the response, where it would block later requests.

```
--> {"jsonrpc":"2.0","id":11,"method":"command.run","params":{"name":"goto_line_end"}}
<-- {"jsonrpc":"2.0","id":11,"result":{"mode":"normal","status":null}}
--> {"jsonrpc":"2.0","id":15,"method":"command.run","params":{"name":"find_next_char"}}
<-- {"jsonrpc":"2.0","id":15,"error":{"code":-32016,"message":"find_next_char needs interactive input (a typed character), which the automation API cannot supply; it was canceled"}}
--> {"jsonrpc":"2.0","id":16,"method":"command.run","params":{"name":"nope"}}
<-- {"jsonrpc":"2.0","id":16,"error":{"code":-32011,"message":"no command named \"nope\""}}
```

### command.list

No params. Result: `commands`, each with `name`, `kind` (`command` or `typed`), `doc`, `aliases` and `keys` (`[{mode, keys}]` from the active keymaps, user bindings included). The session above listed 155 commands; two of them:

```
--> {"jsonrpc":"2.0","id":21,"method":"command.list"}
<-- {"jsonrpc":"2.0","id":21,"result":{"commands":[... {"aliases":[],"doc":"Undo","keys":[{"keys":"u","mode":"normal"},{"keys":"u","mode":"select"}],"kind":"command","name":"undo"}, ... {"aliases":["w"],"doc":"Save the buffer, optionally to a new path (! overwrites)","keys":[{"keys":"C-s","mode":"normal"},{"keys":"C-s","mode":"select"},{"keys":"C-s","mode":"insert"}],"kind":"typed","name":"write"} ...]}}
```

### editor.execute

Params: `line`, a `:` command line with or without the colon. Same result and failure rules as `command.run`.

```
--> {"jsonrpc":"2.0","id":17,"method":"editor.execute","params":{"line":"s/hello/HELLO/"}}
<-- {"jsonrpc":"2.0","id":17,"result":{"mode":"normal","status":{"message":"1 substitution on 1 line","severity":"info"}}}
<-- {"jsonrpc":"2.0","method":"document.changed","params":{"actor":null,"doc":3,"version":3}}
<-- {"jsonrpc":"2.0","method":"editor.status","params":{"status":{"message":"1 substitution on 1 line","severity":"info"}}}
```

### editor.quit

Params: `force` (default false). Refused with `-32015` while buffers have unsaved changes, and with `-32016` (command failed) while agents or their test runs are still running; `data` lists them. `force` overrides both, and stops the agents first. Git work for an agent (creating its worktree, integrating, archiving) refuses the quit even with `force`, until it finishes. On success the response is written before the editor stops: the headless process exits, and the interactive editor exits after its next frame.

```
--> {"jsonrpc":"2.0","id":19,"method":"editor.quit"}
<-- {"jsonrpc":"2.0","id":19,"error":{"code":-32015,"message":"unsaved changes in /tmp/claude-1000/demo/greet.txt; save them or pass force","data":["/tmp/claude-1000/demo/greet.txt"]}}
--> {"jsonrpc":"2.0","id":22,"method":"editor.quit"}
<-- {"jsonrpc":"2.0","id":22,"result":{}}
```

### events.subscribe

No params. Result: `notifications`, the names this connection will receive from now on.

- `document.changed` `{doc, version, actor}`: a buffer's version changed. `actor` is the actor of the `document.apply_edit` or approved proposal that produced exactly this version, or `null` for any other change (typing, commands, undo, reload). Changes are coalesced: on the socket transport, changes made outside the API are checked every 100 ms, so several keystrokes can arrive as one notification with the latest version. Treat it as a cue to re-read.
- `editor.status` `{status}`: the status line changed; `status` is `null` when it was cleared.
- `proposal.resolved`: a proposal reached its final state. The params are the proposal, in the shape `proposal.status` returns. A rejected, withdrawn, stale or failed proposal resolves at once. An approved one with a verification command resolves when the command has exited (or could not run), so the notification always carries the result.

On the headless transport the notifications caused by a request follow its response. On the socket transport they go to every subscribed connection.

## Proposals

A proposal is a change a client asks to make to an open document. The editor checks it when it arrives, shows it to the person using the editor, and applies it only after they approve it. Proposals need the interactive editor (`maestro --listen`); headless runs refuse them.

### proposal.create

Params:

- `doc`, or `path` (exactly one). A path that is not open is opened as `document.open` does, without moving the user's view.
- `base_version`, `encoding`, `edits`: as for `document.apply_edit`.
- `actor`: `{"name": "...", "agent_id": "..."}`. `name` must not be empty; `agent_id` is optional. Both are recorded as given.
- `description`: what the change does, shown to the reviewer. Must not be empty.
- `verification` (optional): `{"program": "...", "args": [...]}`. No shell is involved; `program` is looked up on `PATH` and gets `args` as given. Write `{"program": "sh", "args": ["-c", "..."]}` for a shell command. It runs in the editor's working directory (where `maestro` was started), which the result reports as `verification.cwd`. A document with no path (a scratch buffer) cannot have one.

The base version and the edits are checked at once, with the same rules and error codes as `document.apply_edit`: a stale `base_version` is -32010, invalid edits are -32602, a read-only document is -32012. On any error nothing is queued. On success the result is the proposal (below) with `state` `pending`, and the status line says that a proposal waits and how to open the review. While any are pending, the focused status line says how many.

### proposal.status, proposal.list, proposal.withdraw

- `proposal.status` params: `proposal` (the id). Result: the proposal. -32011 if there is no such proposal.
- `proposal.list` params: `doc` (optional). Result: `proposals`, oldest first, pending and decided, for that document or for all.
- `proposal.withdraw` params: `proposal`, `reason` (optional). The client takes back a pending proposal; it resolves as `withdrawn` with `decided_by` `client`. Only the connection that created the proposal can withdraw it: any other connection gets -32020, whatever actor name it sends, since actor names are whatever a client says and cannot tell clients apart. The check comes before the state check, so -32019 (already decided) goes only to the creator. A proposal whose connection closed can no longer be withdrawn; a new connection is a different connection. The person at the editor can still reject it. A confirmation dialog the reviewer had open for it is closed.

Two Python clients on the socket of `maestro --clean --listen <path> greet.txt` (release build, November 1, 2026). Client A created proposal 1 with actor `agent:shared-name`; client B, on its own connection, tried to withdraw it:

```
B --> {"jsonrpc": "2.0", "id": 2, "method": "proposal.withdraw", "params": {"proposal": 1, "reason": "B tries"}}
B <-- {"jsonrpc": "2.0", "id": 2, "error": {"code": -32020, "message": "proposal 1 was created by another connection; only that connection can withdraw it (the person at the editor can reject it). Nothing changed"}}
```

`proposal.status` from B still said `"state": "pending"`, and A's own `proposal.withdraw` then returned it with `"decision": "withdrawn"`.

A proposal has `id`, `doc`, `path`, `actor`, `description`, `base_version`, `edit_count`, `verification` (`{program, args, cwd}` or `null`), `state` (`pending`, `verifying` while its command runs, `resolved`), `decision` (`approved`, `rejected`, `withdrawn`, `stale`, `failed`, or `null` while pending), `decided_by` (`{"kind": "user", "name": $USER}`, `{"kind": "client"}` or `{"kind": "editor"}`), `reason`, `applied_version`, `saved`, `verification_result`, and `created_ms`, `decided_ms`, `resolved_ms`.

`verification_result` is `null` for a proposal without a command, and otherwise `{status, exit_code, duration_ms, output_tail, output_lines_dropped, error}`. `status` is `not_run` (the proposal was not applied), `running`, `passed` (exit 0), `failed` (another exit code, or killed: `exit_code` is then `null`), or `error` (saving failed or the program did not start; `error` says which). `output_tail` holds the last 40 lines of stdout and stderr, interleaved as read, each cut to 1,000 chars.

### Review in the editor

`space e` or `:proposals` opens the review over the editor area: the queue on the left, and for the selected proposal the actor, its base version next to the document's current version (with a warning when they differ), the description, the verification command, and a unified diff of the document text before and after, with intraline marks, in the agent review screen's colors. The diff is computed from the document text, not from Git. It compares exact bytes: a line whose only change is a `\r` before its newline is shown as changed, with the `\r` drawn as `?`, and a change to the newline at the end of the file gets a note above the diff. A line wider than the review is cut at the right edge with a `>` in the last column; `h` and `l` scroll sideways (a `<` marks text scrolled off to the left), and the approval dialog says how many changed lines are that wide.

| Key | Does |
|---|---|
| `a` | Approve: opens a confirmation that says which file changes, against which version, that it is one undo step, and, with a verification command, that the file is saved and exactly which command runs in which directory. `y` approves, `n` or `esc` cancels. A dialog that does not fit the window cannot be accepted. |
| `x` | Reject: asks for an optional reason, which goes back to the client. `enter` rejects, `esc` cancels. |
| `n`, `p` (`tab`, `S-tab`) | Next and previous proposal |
| `j`, `k`, `]`, `[`, `g`, `G` | Move in the diff, by line or hunk |
| `h`, `l` | Scroll the diff sideways, half a screen at a time |
| `s` | Unified or side-by-side diff |
| `o` | The verification output instead of the diff |
| `c` | Stop a running verification command; its result says it was canceled |
| `esc`, `q` | Close the review |

On `y` the editor checks the proposal again. If the document is no longer at `base_version` (the user typed, another client edited it, another proposal was approved first), it is refused and resolved as `stale` with the reason, and nothing changes; there is no rebase. The same happens when the buffer was saved under another name (`:w other`) since the proposal was made. Keys replayed by a macro or `.` are ignored in the review. Pressing `a` on a proposal that is already stale resolves it that way without a dialog. Otherwise the change is applied with `Document::apply_at_version` against the base version as one undo step: any undo group the user had open is closed first, so one `u` reverts exactly the proposal.

A proposal with a verification command is then saved to disk, because the command reads the file. If the buffer already had unsaved edits of the user's (the client computed its change against them), saving writes those too, and the dialog says so before `y`. If the file's metadata (size, modification time, identity) shows it changed on disk since it was read, approval is refused before anything is applied, and the proposal stays pending. A rewrite that keeps the size and time is only found by the content check when saving, after the proposal was applied. Then, as for any failed save, the proposal stays applied (one `u` reverts it), the file is not written, the command does not run, and the proposal resolves with `verification_result.status` `error`. The command runs through `maestro-tasks` in its own process group; the editor does not wait for it. A proposal without a command is applied and not saved, like `document.apply_edit`.

Closing a document resolves its pending proposals as `stale` with `decided_by` `editor`. Quitting the editor, or `c` in the review, kills a running verification command's process group. When the command exits, background processes it left in its process group get SIGHUP, then SIGKILL once nothing else is left in the group or after 200 ms, and only then does the proposal resolve. `verification_result.exit_code` is the command's own exit code. A process that moved to another process group or session (`setsid`, a daemon) is not reached. This goes through the `kill` program (each call waits for it at most a second); if `kill` is missing or fails, the leftovers keep running. On Linux, when Maestro was started with SIGCHLD ignored (the kernel then reaps children by itself, so no child's pid is safe to signal), no process group is signaled at all and leftovers keep running; elsewhere that case cannot be detected.

### Example

On October 31, 2026 the release binary ran in tmux as `maestro --clean --listen auto calc.py` in a scratch directory, with `calc.py` holding an `add` and a `mul` that added. A Python client on the socket sent these lines (paths shortened to `<dir>`; `created_ms` and the other times are real):

```
--> {"jsonrpc":"2.0","id":5,"method":"proposal.create","params":{"doc":3,"base_version":0,"encoding":"lsp","edits":[{"range":{"start":{"line":5,"character":11},"end":{"line":5,"character":16}},"text":"a * b"}],"actor":{"name":"agent:m3demo","agent_id":"m3demo-1"},"description":"fix mul: it added instead of multiplying","verification":{"program":"sh","args":["-c","python3 -c 'import calc; assert calc.mul(3, 4) == 12' && echo mul ok"]}}}
<-- {"jsonrpc":"2.0","id":5,"result":{"id":1,"doc":3,"path":"<dir>/calc.py","actor":{"name":"agent:m3demo","agent_id":"m3demo-1"},"description":"fix mul: it added instead of multiplying","base_version":0,"edit_count":1,"verification":{"program":"sh","args":["-c","python3 -c 'import calc; assert calc.mul(3, 4) == 12' && echo mul ok"],"cwd":"<dir>"},"state":"pending","decision":null,"decided_by":null,"reason":null,"applied_version":null,"saved":false,"verification_result":null,"created_ms":1793508168617,"decided_ms":null,"resolved_ms":null}}
<-- {"jsonrpc":"2.0","method":"editor.status","params":{"status":{"severity":"info","message":"proposal 1 from agent:m3demo for calc.py waits for review: space e or :proposals"}}}
```

In the editor, `space e` showed the diff (`-    return a + b`, `+    return a * b`, with only `+` and `*` marked), `a` showed the dialog naming `sh -c 'python3 -c '\''import calc; assert calc.mul(3, 4) == 12'\'' && echo mul ok'` and the directory, and `y` approved it. The client then received:

```
<-- {"jsonrpc":"2.0","method":"document.changed","params":{"doc":3,"version":1,"actor":"agent:m3demo"}}
<-- {"jsonrpc":"2.0","method":"proposal.resolved","params":{"id":1,"doc":3,"path":"<dir>/calc.py","actor":{"name":"agent:m3demo","agent_id":"m3demo-1"},"description":"fix mul: it added instead of multiplying","base_version":0,"edit_count":1,"verification":{"program":"sh","args":["-c","python3 -c 'import calc; assert calc.mul(3, 4) == 12' && echo mul ok"],"cwd":"<dir>"},"state":"resolved","decision":"approved","decided_by":{"kind":"user","name":"user"},"reason":null,"applied_version":1,"saved":true,"verification_result":{"status":"passed","exit_code":0,"duration_ms":32,"output_tail":["mul ok"],"output_lines_dropped":0,"error":null},"created_ms":1793508168617,"decided_ms":1793508185727,"resolved_ms":1793508185781}}
--> {"jsonrpc":"2.0","id":6,"method":"document.history","params":{"doc":3}}
<-- {"jsonrpc":"2.0","id":6,"result":{"doc":3,"edits":[{"actor":"agent:m3demo","description":"fix mul: it added instead of multiplying","base_version":0,"new_version":1,"timestamp_ms":1793508185727,"proposal":1}],"proposals":[{"id":1, ... the same record as in proposal.resolved ...}]}}
```

The login name in `decided_by` is shown as `user` here; the editor sends the real one.

In the same session a proposal against version 0, sent after the first was applied, was refused at once with -32010. A second proposal against version 1 went stale when the user typed a line before approving it: `a` resolved it as `stale` ("the document is at version 21, the proposal was made against version 1"), and its command (`touch late-ran`) never ran. A third was rejected with the reason "unused import". Two `u` presses then undid the typed line and the approved proposal, one each.


## Not built yet

- Rebasing a stale proposal. A proposal whose document moved on is refused; the client must compute a new one against the current version.
- Proposals that span several files, and proposals that delete or rename files. (A `path` that does not exist opens as an empty document, so a proposal that inserts text there creates the file when it is saved: on approval with a verification command, or when the user saves.)
- Approving proposals without a person (headless, or a flag). By design for now.
- Persistent provenance. The edit log and the proposal records are in memory.
- Opening or closing buffers and views, splits, and typing into prompts through the API.
- Multi-file edits as one operation.
- A wake-up for the interactive loop, which would remove the up-to-one-second delay on a new connection's first request. That needs a change to the shared `Editor` interface.

See `docs/security.md` for the trust model.
