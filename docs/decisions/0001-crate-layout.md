# 0001: Crate layout

Date: October 30, 2026. Status: accepted.

## Context

The brief proposes eleven crates, including `vesper-command` for modal commands, keymaps, macros and the command registry. The project was renamed Maestro before any code existed, so crates use the `maestro-` prefix.

## Decision

Start with five crates: `maestro-core`, `maestro-config`, `maestro-editor`, `maestro-tui`, `maestro-cli`.

`maestro-editor` replaces `vesper-command`. Commands need the editor state they act on (views, buffers, registers, modes), and splitting the state from the commands would put a large mutable interface between two crates that always change together.

The other proposed crates (`syntax`, `lsp`, `git`, `agent`, `protocol`, `tasks`) are created when their workstream starts. An empty crate has no interface worth freezing.

## Consequences

`maestro-editor` is the largest crate and the most likely merge-conflict point. Workstreams add new modules to it instead of growing `editor.rs` or `commands.rs` where they can.
