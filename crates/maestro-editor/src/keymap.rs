//! Keymaps: per-mode tries from key sequences to command names.
//!
//! Bindings name commands from the registry (`crate::commands`) or typed
//! commands (`crate::typed`), so every key is also reachable from the
//! command palette and the automation API under the same name.

use std::collections::BTreeMap;

use maestro_config::{Config, Profile};

use crate::editor::Mode;
use crate::key::{KeyEvent, parse_sequence};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTrie {
    Command(String),
    Node {
        name: String,
        map: BTreeMap<KeyEvent, KeyTrie>,
    },
}

impl KeyTrie {
    fn node(name: &str) -> Self {
        KeyTrie::Node {
            name: name.to_string(),
            map: BTreeMap::new(),
        }
    }

    /// Binds `keys` to `command`, creating intermediate nodes. Intermediate
    /// node names come from `names` when given.
    fn bind(&mut self, keys: &[KeyEvent], command: &str, group: Option<&str>) {
        let KeyTrie::Node { map, .. } = self else {
            return;
        };
        match keys {
            [] => {}
            [k] => {
                map.insert(*k, KeyTrie::Command(command.to_string()));
            }
            [k, rest @ ..] => {
                let child = map
                    .entry(*k)
                    .or_insert_with(|| KeyTrie::node(group.unwrap_or("…")));
                if let KeyTrie::Command(_) = child {
                    *child = KeyTrie::node(group.unwrap_or("…"));
                }
                if let (KeyTrie::Node { name, .. }, Some(g)) = (&mut *child, group)
                    && rest.len() == 1
                {
                    *name = g.to_string();
                }
                child.bind(rest, command, group);
            }
        }
    }

    pub fn name(&self) -> &str {
        match self {
            KeyTrie::Command(c) => c,
            KeyTrie::Node { name, .. } => name,
        }
    }

    pub fn children(&self) -> Option<&BTreeMap<KeyEvent, KeyTrie>> {
        match self {
            KeyTrie::Node { map, .. } => Some(map),
            KeyTrie::Command(_) => None,
        }
    }

    fn get(&self, keys: &[KeyEvent]) -> Option<&KeyTrie> {
        let mut node = self;
        for k in keys {
            node = node.children()?.get(k)?;
        }
        Some(node)
    }

    fn collect(&self, prefix: &mut Vec<KeyEvent>, out: &mut Vec<(Vec<KeyEvent>, String)>) {
        match self {
            KeyTrie::Command(c) => out.push((prefix.clone(), c.clone())),
            KeyTrie::Node { map, .. } => {
                for (k, child) in map {
                    prefix.push(*k);
                    child.collect(prefix, out);
                    prefix.pop();
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Command(String),
    Pending,
    None,
}

#[derive(Debug, Clone)]
pub struct Keymaps {
    normal: KeyTrie,
    select: KeyTrie,
    insert: KeyTrie,
    /// Problems found in user key config, reported at startup.
    pub errors: Vec<String>,
}

impl Keymaps {
    pub fn for_config(config: &Config) -> Self {
        let mut maps = Self::defaults(config.profile);
        for (mode, binds) in &config.keys {
            let trie = match mode.as_str() {
                "normal" => &mut maps.normal,
                "select" => &mut maps.select,
                "insert" => &mut maps.insert,
                other => {
                    maps.errors.push(format!("unknown keymap mode {other:?}"));
                    continue;
                }
            };
            for (keys, command) in binds {
                if crate::commands::lookup(command).is_none()
                    && crate::typed::lookup(command).is_none()
                {
                    maps.errors.push(format!(
                        "[keys.{mode}] {keys:?}: unknown command {command:?}"
                    ));
                    continue;
                }
                match parse_sequence(keys) {
                    Ok(seq) if !seq.is_empty() => trie.bind(&seq, command, None),
                    Ok(_) => maps
                        .errors
                        .push(format!("[keys.{mode}] empty key sequence")),
                    Err(e) => maps.errors.push(format!("[keys.{mode}] {e}")),
                }
            }
        }
        maps
    }

    pub fn trie(&self, mode: Mode) -> &KeyTrie {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Select => &self.select,
            Mode::Insert => &self.insert,
        }
    }

    pub fn lookup(&self, mode: Mode, keys: &[KeyEvent]) -> Lookup {
        match self.trie(mode).get(keys) {
            Some(KeyTrie::Command(c)) => Lookup::Command(c.clone()),
            Some(KeyTrie::Node { .. }) => Lookup::Pending,
            None => Lookup::None,
        }
    }

    pub fn node(&self, mode: Mode, keys: &[KeyEvent]) -> Option<&KeyTrie> {
        self.trie(mode).get(keys)
    }

    /// Every binding in a mode, as (keys, command).
    pub fn bindings(&self, mode: Mode) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.trie(mode).collect(&mut Vec::new(), &mut out);
        out.into_iter()
            .map(|(keys, c)| {
                let keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
                (keys.join(" "), c)
            })
            .collect()
    }

    /// The first key sequence bound to `command` in `mode`, for display.
    pub fn key_for(&self, mode: Mode, command: &str) -> Option<String> {
        let mut all = self.bindings(mode);
        all.sort_by_key(|(k, _)| k.len());
        all.into_iter().find(|(_, c)| c == command).map(|(k, _)| k)
    }

    fn defaults(profile: Profile) -> Self {
        let mut normal = KeyTrie::node("normal");
        let b = |t: &mut KeyTrie, keys: &str, cmd: &str| {
            t.bind(
                &parse_sequence(keys).expect("default keymap parses"),
                cmd,
                None,
            )
        };
        let g = |t: &mut KeyTrie, keys: &str, group: &str, cmd: &str| {
            t.bind(
                &parse_sequence(keys).expect("default keymap parses"),
                cmd,
                Some(group),
            )
        };

        // Movement.
        for (k, c) in [
            ("h", "move_char_left"),
            ("left", "move_char_left"),
            ("l", "move_char_right"),
            ("right", "move_char_right"),
            ("j", "move_line_down"),
            ("down", "move_line_down"),
            ("k", "move_line_up"),
            ("up", "move_line_up"),
            ("w", "select_next_word_start"),
            ("b", "select_prev_word_start"),
            ("e", "select_next_word_end"),
            ("W", "select_next_long_word_start"),
            ("B", "select_prev_long_word_start"),
            ("E", "select_next_long_word_end"),
            ("0", "goto_line_start"),
            ("home", "goto_line_start"),
            ("^", "goto_first_nonblank"),
            ("$", "goto_line_end"),
            ("end", "goto_line_end"),
            ("G", "goto_last_line"),
            ("f", "find_next_char"),
            ("t", "till_next_char"),
            ("F", "find_prev_char"),
            ("T", "till_prev_char"),
            ("A-.", "repeat_last_find"),
            ("C-d", "half_page_down"),
            ("C-u", "half_page_up"),
            ("C-f", "page_down"),
            ("pagedown", "page_down"),
            ("C-b", "page_up"),
            ("pageup", "page_up"),
            ("C-e", "scroll_down"),
            ("C-y", "scroll_up"),
            ("C-o", "jump_backward"),
            ("C-i", "jump_forward"),
            ("tab", "jump_forward"),
            ("C-s", "write"),
        ] {
            b(&mut normal, k, c);
        }
        for (k, c) in [
            ("g g", "goto_file_start"),
            ("g e", "goto_last_line"),
            ("g h", "goto_line_start"),
            ("g l", "goto_line_end"),
            ("g s", "goto_first_nonblank"),
            ("g n", "buffer_next"),
            ("g p", "buffer_prev"),
            ("g a", "goto_alternate_file"),
            ("g -", "earlier"),
            ("g +", "later"),
            ("g t", "goto_window_top"),
            ("g c", "goto_window_center"),
            ("g b", "goto_window_bottom"),
        ] {
            g(&mut normal, k, "goto", c);
        }
        for (k, c) in [
            ("z z", "align_view_center"),
            ("z t", "align_view_top"),
            ("z b", "align_view_bottom"),
        ] {
            g(&mut normal, k, "view", c);
        }

        // Selection manipulation.
        for (k, c) in [
            ("x", "extend_line_below"),
            ("X", "extend_to_line_bounds"),
            ("%", "select_all"),
            (";", "collapse_selection"),
            ("A-;", "flip_selections"),
            (",", "keep_primary_selection"),
            ("A-,", "remove_primary_selection"),
            ("(", "rotate_selections_backward"),
            (")", "rotate_selections_forward"),
            ("C", "copy_selection_on_next_line"),
            ("A-C", "copy_selection_on_prev_line"),
            ("s", "select_regex"),
            ("S", "split_selection"),
            ("A-s", "split_selection_on_newline"),
            ("K", "keep_selections"),
            ("A-K", "remove_selections"),
            ("_", "trim_selections"),
            ("v", "select_mode"),
            ("/", "search"),
            ("?", "rsearch"),
            ("n", "search_next"),
            ("N", "search_prev"),
            ("*", "search_selection"),
        ] {
            b(&mut normal, k, c);
        }
        for (k, c) in [
            ("m m", "match_brackets"),
            ("m s", "surround_add"),
            ("m r", "surround_replace"),
            ("m d", "surround_delete"),
            ("m i", "select_textobject_inner"),
            ("m a", "select_textobject_around"),
        ] {
            g(&mut normal, k, "match", c);
        }

        // Changes.
        for (k, c) in [
            ("i", "insert_mode"),
            ("a", "append_mode"),
            ("I", "insert_at_line_start"),
            ("A", "insert_at_line_end"),
            ("o", "open_below"),
            ("O", "open_above"),
            ("d", "delete_selection"),
            ("A-d", "delete_selection_noyank"),
            ("c", "change_selection"),
            ("A-c", "change_selection_noyank"),
            ("y", "yank"),
            ("p", "paste_after"),
            ("P", "paste_before"),
            ("R", "replace_with_yanked"),
            ("r", "replace"),
            ("u", "undo"),
            ("U", "redo"),
            ("C-r", "redo"),
            (">", "indent"),
            ("<", "unindent"),
            ("~", "switch_case"),
            ("`", "switch_to_lowercase"),
            ("A-`", "switch_to_uppercase"),
            ("J", "join_selections"),
            (".", "repeat_last_change"),
            ("Q", "record_macro"),
            ("q", "replay_macro"),
            ("M", "set_mark"),
            ("'", "goto_mark"),
            (":", "command_mode"),
            ("esc", "normal_mode"),
        ] {
            b(&mut normal, k, c);
        }

        // Window commands under C-w and space w.
        for prefix in ["C-w", "space w"] {
            for (k, c) in [
                ("v", "vsplit"),
                ("s", "hsplit"),
                ("w", "rotate_view"),
                ("C-w", "rotate_view"),
                ("q", "wclose"),
                ("o", "wonly"),
                ("h", "jump_view_left"),
                ("j", "jump_view_down"),
                ("k", "jump_view_up"),
                ("l", "jump_view_right"),
            ] {
                g(&mut normal, &format!("{prefix} {k}"), "window", c);
            }
        }

        // Space leader.
        for (k, c) in [
            ("space f", "file_picker"),
            ("space b", "buffer_picker"),
            ("space ?", "command_palette"),
            ("space /", "global_search"),
            ("space y", "yank_to_clipboard"),
            ("space p", "paste_clipboard_after"),
            ("space P", "paste_clipboard_before"),
            ("space R", "replace_selections_with_clipboard"),
            ("space z", "toggle_focus_mode"),
        ] {
            g(&mut normal, k, "space", c);
        }
        for (k, group, c) in crate::lsp_commands::DEFAULT_KEYS {
            g(&mut normal, k, group, c);
        }
        for (k, c) in crate::navigation::DEFAULT_KEYS {
            g(&mut normal, k, "space", c);
        }
        for (k, c) in crate::agents::DEFAULT_KEYS
            .iter()
            .chain(crate::proposals::DEFAULT_KEYS)
            .chain(crate::workspaces::DEFAULT_KEYS)
            .chain(crate::forge::DEFAULT_KEYS)
            .chain(crate::git_screen::DEFAULT_KEYS)
            .chain(crate::shell::DEFAULT_KEYS)
        {
            g(&mut normal, k, "space", c);
        }
        for (k, group, c) in crate::git_hunks::DEFAULT_KEYS {
            g(&mut normal, k, group, c);
        }

        if profile == Profile::Vim {
            // A small, honest subset of Vim's grammar layered on the
            // selection model. See docs/editor-model.md for what is missing.
            for (k, c) in [
                ("x", "delete_char_forward"),
                ("X", "delete_char_backward"),
                ("D", "delete_to_line_end"),
                ("d d", "delete_line"),
                ("y y", "yank_line"),
                ("c c", "change_line"),
                ("C-r", "redo"),
                ("u", "undo"),
                ("V", "select_line_mode"),
            ] {
                b(&mut normal, k, c);
            }
        }

        bind_syntax(&mut normal);

        let mut select = normal.clone();
        b(&mut select, "v", "normal_mode");
        b(&mut select, "esc", "normal_mode");

        let mut insert = KeyTrie::node("insert");
        for (k, c) in [
            ("esc", "normal_mode"),
            ("ret", "insert_newline"),
            ("backspace", "delete_char_backward"),
            ("del", "delete_char_forward"),
            ("tab", "insert_tab"),
            ("C-w", "delete_word_backward"),
            ("C-u", "kill_to_line_start"),
            ("C-s", "write"),
            ("left", "move_char_left"),
            ("right", "move_char_right"),
            ("up", "move_line_up"),
            ("down", "move_line_down"),
            ("home", "goto_line_start"),
            ("end", "goto_line_end_insert"),
            ("pageup", "page_up"),
            ("pagedown", "page_down"),
        ] {
            b(&mut insert, k, c);
        }
        for (k, c) in crate::lsp_commands::INSERT_KEYS {
            b(&mut insert, k, c);
        }

        Self {
            normal,
            select,
            insert,
            errors: Vec::new(),
        }
    }
}

/// Structural selection keys (`crate::syntax_commands`).
fn bind_syntax(trie: &mut KeyTrie) {
    for (keys, command) in crate::syntax_commands::DEFAULT_KEYS {
        trie.bind(
            &parse_sequence(keys).expect("default keymap parses"),
            command,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_binding_names_a_real_command() {
        for profile in [Profile::Maestro, Profile::Vim] {
            let maps = Keymaps::defaults(profile);
            for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
                for (keys, cmd) in maps.bindings(mode) {
                    assert!(
                        crate::commands::lookup(&cmd).is_some()
                            || crate::typed::lookup(&cmd).is_some(),
                        "{mode:?} {keys} -> {cmd} is not a command"
                    );
                }
            }
        }
    }

    /// `C-x` asks for completions in insert mode in both profiles, and the
    /// keys the open popup takes (`C-n`, `C-p`) are not bound to anything
    /// else there.
    #[test]
    fn insert_completion_keys_do_not_clash() {
        for profile in [Profile::Maestro, Profile::Vim] {
            let maps = Keymaps::defaults(profile);
            let key = |s: &str| parse_sequence(s).unwrap();
            assert_eq!(
                maps.lookup(Mode::Insert, &key("C-x")),
                Lookup::Command("completion".into())
            );
            let bound: Vec<String> = maps
                .bindings(Mode::Insert)
                .into_iter()
                .filter(|(_, c)| c == "completion")
                .map(|(k, _)| k)
                .collect();
            assert_eq!(bound.len(), 1, "{bound:?}");
            assert_eq!(maps.lookup(Mode::Insert, &key("C-n")), Lookup::None);
            assert_eq!(maps.lookup(Mode::Insert, &key("C-p")), Lookup::None);
        }
    }

    /// Rename, code actions, formatting and signature help each have one
    /// key of their own in both profiles, in normal and select mode, and
    /// the keys next to them keep their old commands.
    #[test]
    fn lsp_action_keys_are_bound_once() {
        for profile in [Profile::Maestro, Profile::Vim] {
            let maps = Keymaps::defaults(profile);
            let key = |s: &str| parse_sequence(s).unwrap();
            for mode in [Mode::Normal, Mode::Select] {
                for (keys, command) in [
                    ("space n", "rename_symbol"),
                    ("space c", "code_action"),
                    ("space =", "format_document"),
                    ("space a", "agent_panel"),
                    ("space r", "recent_files"),
                    ("space e", "proposal_review"),
                    ("space i", "github_inbox"),
                    ("space k", "hover"),
                    ("g d", "goto_definition"),
                    ("g r", "goto_references"),
                ] {
                    assert_eq!(
                        maps.lookup(mode, &key(keys)),
                        Lookup::Command(command.into()),
                        "{profile:?} {mode:?} {keys}"
                    );
                }
                for command in ["rename_symbol", "code_action", "format_document"] {
                    let n = maps
                        .bindings(mode)
                        .iter()
                        .filter(|(_, c)| c == command)
                        .count();
                    assert_eq!(n, 1, "{profile:?} {mode:?} {command}");
                }
            }
            assert_eq!(
                maps.lookup(Mode::Insert, &key("C-k")),
                Lookup::Command("signature_help".into())
            );
        }
    }

    #[test]
    fn lookup_pending_and_user_overrides() {
        let mut config = Config::default();
        config
            .keys
            .entry("normal".into())
            .or_default()
            .insert("space x".into(), "write".into());
        config
            .keys
            .entry("normal".into())
            .or_default()
            .insert("Z".into(), "not_a_command".into());
        let maps = Keymaps::for_config(&config);
        let seq = parse_sequence("space x").unwrap();
        assert_eq!(maps.lookup(Mode::Normal, &seq[..1]), Lookup::Pending);
        assert_eq!(
            maps.lookup(Mode::Normal, &seq),
            Lookup::Command("write".into())
        );
        assert_eq!(maps.errors.len(), 1);
    }
}
