//! Language server commands and their default keys.

use crate::editor::Editor;
use crate::lsp::{self, RequestKind};

crate::commands! {
    goto_definition => "Go to the definition of the symbol under the cursor",
    goto_references => "List references to the symbol under the cursor",
    hover => "Show documentation for the symbol under the cursor",
    goto_next_diag => "Go to the next diagnostic",
    goto_prev_diag => "Go to the previous diagnostic",
    diagnostics_picker => "Pick a diagnostic in this buffer",
    workspace_diagnostics_picker => "Pick a diagnostic in any open buffer",
    lsp_restart => "Restart language servers",
    lsp_log => "Show language server logs in a scratch buffer",
}

/// Default normal-mode keys: (keys, which-key group, command). Bound by
/// `keymap.rs` in both the Maestro and Vim profiles.
pub static DEFAULT_KEYS: &[(&str, &str, &str)] = &[
    ("g d", "goto", "goto_definition"),
    ("g r", "goto", "goto_references"),
    ("space k", "space", "hover"),
    ("space d", "space", "diagnostics_picker"),
    ("space D", "space", "workspace_diagnostics_picker"),
    ("] d", "next", "goto_next_diag"),
    ("[ d", "previous", "goto_prev_diag"),
];

fn goto_definition(ed: &mut Editor) {
    lsp::request_at_cursor(ed, RequestKind::Definition);
}

fn goto_references(ed: &mut Editor) {
    lsp::request_at_cursor(ed, RequestKind::References);
}

fn hover(ed: &mut Editor) {
    lsp::request_at_cursor(ed, RequestKind::Hover);
}

fn goto_next_diag(ed: &mut Editor) {
    lsp::goto_diagnostic(ed, true);
}

fn goto_prev_diag(ed: &mut Editor) {
    lsp::goto_diagnostic(ed, false);
}

fn diagnostics_picker(ed: &mut Editor) {
    lsp::diagnostics_picker(ed, false);
}

fn workspace_diagnostics_picker(ed: &mut Editor) {
    lsp::diagnostics_picker(ed, true);
}

fn lsp_restart(ed: &mut Editor) {
    lsp::restart(ed);
}

fn lsp_log(ed: &mut Editor) {
    lsp::show_log(ed);
}
