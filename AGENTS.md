# Rules for AI coding agents

These apply to every agent working on Maestro, including the integration lead.

## Workspace

- Work in your own Git worktree on your own branch (`feat/<area>` or `test/<area>`). Never commit to `main` directly; the integration lead merges.
- Stay inside the files your task names. If you need to change a shared interface (anything `pub` in `maestro-core`, the `Editor` struct, the command registry), stop and describe the change in your report instead of making it, unless your task explicitly allows it.
- Do not edit workspace-wide files (`Cargo.toml` at the root, `rustfmt.toml`, CI) unless your task says you may. Adding a dependency to `[workspace.dependencies]` counts.
- Never force-push, never rewrite another branch, never delete files you did not create.

## Quality

- Run the three gates before reporting: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- A stub must say it is a stub: in the code, in the UI message the user sees, and in your report. A panel that shows invented data is not acceptable.
- Never report a test as passing unless you ran it and saw it pass. Quote the result line.
- Anything a user does should have a behavior test that drives keys through `Editor::handle_key`.
- Text changes go through `Transaction`. Respect `Document::apply_at_version` when an edit was computed asynchronously.

## Report

Finish every unit of work with:

1. What now works, in user terms.
2. Branch, worktree path and commit hash.
3. Files changed.
4. Public interfaces added or changed.
5. Tests added.
6. The exact verification commands you ran and their result lines.
7. Known limitations.
8. What the integration lead must do to merge.
9. The next task you would recommend.

If a tool or dependency was unavailable, say what failed and how to reproduce it.

## Task list

`docs/tasks.toml` lists the workstreams with owners and status. Update your entry in your branch.
