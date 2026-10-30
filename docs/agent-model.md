# Agent model

Nothing in this document is built yet except where marked. It records the decisions that the existing code already commits to, so agent work (M2, M3) starts from them.

## Built: version-checked edits

`Document::apply_at_version(tx, expected_version, selection)` applies a transaction only if the document is still at the version the edit was computed against. Otherwise it returns `EditError::Stale` and changes nothing. Separately, every `ChangeSet` records the length of the document it was built for and refuses to apply to any other length.

Edits produced away from the keyboard and computed against a known version must go through `apply_at_version`. The automation API's `document.apply_edit` does. Its `command.run` and `editor.execute` do not: they run editor commands on the current text, as a key press would, with no version check and no entry in the edit history (see `docs/automation.md`).

## Planned

- An agent is a record with an id, a name, a task, a provider, an optional worktree, a status (planning, editing, running tests, awaiting approval, blocked, ready for review, completed, failed) and a history. Status colors already exist in the theme (`AgentTheme`) and each is always shown with its name.
- Providers: Agent Client Protocol where the agent supports it; a managed PTY for any other CLI agent. The UI will say which kind a session is, and will not show structured progress for PTY-only agents.
- A worktree is source isolation, not a security boundary. Agents run with the user's permissions unless an OS sandbox is configured; the permission system will be enforced in backend code, not by confirmation dialogs alone.
- Edit proposals carry base versions or content hashes per file, the operations, the originating actor and a review status. Multi-file application uses a durable journal; it will not be described as atomic.
