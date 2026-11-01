//! Language server commands and their default keys.

use crate::editor::{Editor, Mode};
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
    completion => "Ask the language server for completions at the cursor (insert mode)",
    rename_symbol => "Rename the symbol under the cursor in every file that uses it",
    format_document => "Format the buffer with the language server",
    format_selection => "Format the primary selection with the language server",
    code_action => "Pick a code action for the cursor or selection",
    signature_help => "Show the signature of the call around the cursor (insert mode)",
}

/// Default normal-mode keys: (keys, which-key group, command). Bound by
/// `keymap.rs` in both the Maestro and Vim profiles.
pub static DEFAULT_KEYS: &[(&str, &str, &str)] = &[
    ("g d", "goto", "goto_definition"),
    ("g r", "goto", "goto_references"),
    ("space k", "space", "hover"),
    ("space d", "space", "diagnostics_picker"),
    ("space D", "space", "workspace_diagnostics_picker"),
    ("space n", "space", "rename_symbol"),
    ("space c", "space", "code_action"),
    ("space =", "space", "format_document"),
    ("] d", "next", "goto_next_diag"),
    ("[ d", "previous", "goto_prev_diag"),
];

/// Default insert-mode keys: (keys, command). `C-x` and `C-k` are free in
/// insert mode (`C-w`, `C-u` and `C-s` are taken). While the completion
/// popup is open it also takes `C-n`, `C-p`, Up, Down, Tab, Enter and
/// Escape; those are handled by the popup, not bound here.
pub static INSERT_KEYS: &[(&str, &str)] = &[("C-x", "completion"), ("C-k", "signature_help")];

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

fn completion(ed: &mut Editor) {
    if ed.mode != Mode::Insert {
        ed.info("completion works in insert mode (C-x)");
        return;
    }
    lsp::completion::request(ed, lsp::completion::Trigger::Manual);
}

fn rename_symbol(ed: &mut Editor) {
    lsp::actions::rename(ed, None);
}

fn format_document(ed: &mut Editor) {
    lsp::actions::format(ed, false);
}

fn format_selection(ed: &mut Editor) {
    lsp::actions::format(ed, true);
}

fn code_action(ed: &mut Editor) {
    lsp::actions::code_actions(ed);
}

fn signature_help(ed: &mut Editor) {
    if ed.mode != Mode::Insert {
        ed.info("signature help works in insert mode (C-k)");
        return;
    }
    lsp::signature::request(ed, lsp::signature::Trigger::Manual);
}
