//! Theme tokens.
//!
//! UI code asks for a role (`theme.ui.statusline`, `theme.syntax("keyword")`)
//! and never for a color. A filled control and the text on it are separate
//! tokens, and `tests::ui_text_meets_contrast` checks every text pair the
//! interface actually draws.

use std::collections::HashMap;

use crate::duskfox as p;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Self {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Blends `self` toward `other` by `amount` (0.0 = self, 1.0 = other),
    /// matching nightfox's `Color:blend`.
    pub fn blend(self, other: Rgb, amount: f32) -> Rgb {
        let mix = |a: u8, b: u8| (a as f32 * (1.0 - amount) + b as f32 * amount).round() as u8;
        Rgb(
            mix(self.0, other.0),
            mix(self.1, other.1),
            mix(self.2, other.2),
        )
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    /// WCAG contrast ratio between two colors.
    pub fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    pub const fn fg(c: Rgb) -> Self {
        Self {
            fg: Some(c),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
        }
    }

    pub const fn bg(c: Rgb) -> Self {
        Self {
            fg: None,
            bg: Some(c),
            bold: false,
            italic: false,
            underline: false,
        }
    }

    pub const fn on(fg: Rgb, bg: Rgb) -> Self {
        Self {
            fg: Some(fg),
            bg: Some(bg),
            bold: false,
            italic: false,
            underline: false,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Layers `top` over `self`: set fields in `top` win.
    pub fn patch(self, top: Style) -> Style {
        Style {
            fg: top.fg.or(self.fg),
            bg: top.bg.or(self.bg),
            bold: self.bold || top.bold,
            italic: self.italic || top.italic,
            underline: self.underline || top.underline,
        }
    }
}

/// Interface roles. Grouped by surface so that every text token sits next
/// to the background it is drawn on.
#[derive(Debug, Clone)]
pub struct UiTheme {
    pub text: Style,
    pub text_muted: Style,
    pub gutter: Style,
    pub gutter_current: Style,
    pub cursorline: Style,
    pub selection: Style,
    pub selection_primary: Style,
    pub cursor: Style,
    pub cursor_secondary: Style,
    pub search_match: Style,
    pub whitespace: Style,
    pub ruler: Style,

    pub statusline: Style,
    pub statusline_muted: Style,
    pub statusline_inactive: Style,
    pub mode_normal: Style,
    pub mode_insert: Style,
    pub mode_select: Style,
    pub mode_command: Style,

    pub border: Style,
    pub border_active: Style,

    pub tab_active: Style,
    pub tab_inactive: Style,
    pub tabline: Style,

    pub popup: Style,
    pub popup_muted: Style,
    pub popup_selected: Style,
    pub popup_match: Style,
    /// Match highlight on the selected row. Pink on the selection fill is
    /// 3.1:1, so the selected row marks matches with weight and underline.
    pub popup_match_selected: Style,
    pub popup_border: Style,
    pub popup_title: Style,

    pub prompt: Style,
    pub message_info: Style,
    pub message_warning: Style,
    pub message_error: Style,

    pub file_dir: Style,
    pub file_modified: Style,
}

#[derive(Debug, Clone)]
pub struct DiagnosticTheme {
    pub error: Style,
    pub warning: Style,
    pub info: Style,
    pub hint: Style,
    pub error_bg: Rgb,
    pub warning_bg: Rgb,
    pub info_bg: Rgb,
    pub hint_bg: Rgb,
}

#[derive(Debug, Clone)]
pub struct DiffTheme {
    pub add_bg: Rgb,
    pub delete_bg: Rgb,
    pub change_bg: Rgb,
    pub text_bg: Rgb,
    pub git_add: Style,
    pub git_removed: Style,
    pub git_changed: Style,
    pub git_conflict: Style,
    pub git_ignored: Style,
}

/// Agent lifecycle states. Each color is always shown with the state's name.
#[derive(Debug, Clone)]
pub struct AgentTheme {
    pub planning: Style,
    pub editing: Style,
    pub running: Style,
    pub awaiting_approval: Style,
    pub blocked: Style,
    pub ready: Style,
    pub completed: Style,
    pub failed: Style,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub background: Rgb,
    pub ui: UiTheme,
    pub diagnostic: DiagnosticTheme,
    pub diff: DiffTheme,
    pub agent: AgentTheme,
    syntax: HashMap<String, Style>,
}

impl Theme {
    /// Looks up a syntax capture such as `function.method.call`, falling back
    /// to its parents (`function.method`, then `function`).
    pub fn syntax(&self, scope: &str) -> Option<Style> {
        let mut scope = scope;
        loop {
            if let Some(s) = self.syntax.get(scope) {
                return Some(*s);
            }
            scope = &scope[..scope.rfind('.')?];
        }
    }

    pub fn syntax_scopes(&self) -> impl Iterator<Item = (&str, &Style)> {
        self.syntax.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn duskfox() -> Self {
        // Muted text on the default background. Upstream's comment color
        // (#817c9c) measures 3.9:1 on bg1, under the 4.5:1 AA text minimum,
        // so interface text that must be read uses a lighter mix. Comments in
        // code keep the upstream color.
        let muted = p::COMMENT.blend(p::FG1, 0.3);
        let statusline_bg = p::BG0;
        let popup_bg = p::BG2;

        let ui = UiTheme {
            text: Style::on(p::FG1, p::BG1),
            text_muted: Style::on(muted, p::BG1),
            gutter: Style::on(muted, p::BG1),
            gutter_current: Style::on(p::YELLOW.base, p::BG3).bold(),
            cursorline: Style::bg(p::BG3),
            selection: Style::bg(p::SEL0),
            selection_primary: Style::bg(p::SEL1),
            cursor: Style::on(p::BG1, p::FG1),
            cursor_secondary: Style::on(p::BG1, p::MAGENTA.dim),
            search_match: Style::on(p::BG0, p::CYAN.dim),
            whitespace: Style::fg(p::BG4),
            ruler: Style::bg(p::BG2),

            statusline: Style::on(p::FG2, statusline_bg),
            statusline_muted: Style::on(p::COMMENT.blend(p::FG2, 0.35), statusline_bg),
            statusline_inactive: Style::on(p::COMMENT.blend(p::FG2, 0.35), statusline_bg),
            mode_normal: Style::on(p::BG0, p::BLUE.bright).bold(),
            mode_insert: Style::on(p::BG0, p::GREEN.base).bold(),
            mode_select: Style::on(p::BG0, p::MAGENTA.base).bold(),
            mode_command: Style::on(p::BG0, p::CYAN.base).bold(),

            border: Style::on(p::BG4, p::BG1),
            border_active: Style::on(p::MAGENTA.base, p::BG1),

            tab_active: Style::on(p::FG0, p::BG1).bold(),
            tab_inactive: Style::on(p::COMMENT.blend(p::FG2, 0.35), p::BG0),
            tabline: Style::bg(p::BG0),

            popup: Style::on(p::FG1, popup_bg),
            popup_muted: Style::on(p::COMMENT.blend(p::FG1, 0.4), popup_bg),
            popup_selected: Style::on(p::FG0, p::SEL1).bold(),
            popup_match: Style::fg(p::PINK.base).bold(),
            popup_match_selected: Style::fg(p::FG0).bold().underline(),
            popup_border: Style::on(p::BG4.blend(p::MAGENTA.base, 0.35), popup_bg),
            popup_title: Style::on(p::MAGENTA.base, popup_bg).bold(),

            prompt: Style::on(p::FG1, p::BG1),
            message_info: Style::on(p::FG2, p::BG1),
            message_warning: Style::on(p::YELLOW.base, p::BG1),
            message_error: Style::on(p::RED.base, p::BG1),

            file_dir: Style::fg(p::BLUE.bright),
            file_modified: Style::fg(p::YELLOW.base),
        };

        let blend = |c: Rgb, a: f32| p::BG1.blend(c, a);
        let diagnostic = DiagnosticTheme {
            error: Style::fg(p::RED.base),
            warning: Style::fg(p::YELLOW.base),
            info: Style::fg(p::BLUE.base),
            hint: Style::fg(p::GREEN.base),
            error_bg: blend(p::RED.base, 0.15),
            warning_bg: blend(p::YELLOW.base, 0.15),
            info_bg: blend(p::BLUE.base, 0.15),
            hint_bg: blend(p::GREEN.base, 0.15),
        };

        let diff = DiffTheme {
            add_bg: blend(p::GREEN.base, 0.2),
            delete_bg: blend(p::RED.base, 0.2),
            change_bg: blend(p::BLUE.base, 0.2),
            text_bg: blend(p::BLUE.base, 0.4),
            git_add: Style::fg(p::GREEN.base),
            // Bright: base red is 4.1:1 on the current-line gutter.
            git_removed: Style::fg(p::RED.bright),
            git_changed: Style::fg(p::YELLOW.base),
            git_conflict: Style::fg(p::ORANGE.base),
            git_ignored: Style::fg(p::COMMENT),
        };

        let agent = AgentTheme {
            planning: Style::fg(p::BLUE.bright),
            editing: Style::fg(p::MAGENTA.base),
            running: Style::fg(p::CYAN.base),
            awaiting_approval: Style::fg(p::YELLOW.base),
            blocked: Style::fg(p::ORANGE.base),
            ready: Style::fg(p::GREEN.base),
            completed: Style::fg(p::GREEN.dim),
            failed: Style::fg(p::RED.base),
        };

        // Capture names follow the nvim-treesitter / Helix conventions and
        // map onto upstream's `spec.syntax` roles.
        let syn_bracket = Style::fg(p::FG2);
        let syn_builtin0 = Style::fg(p::RED.base);
        let syn_builtin1 = Style::fg(p::CYAN.bright);
        let syn_builtin2 = Style::fg(p::ORANGE.bright);
        let syn_comment = Style::fg(p::COMMENT);
        let syn_conditional = Style::fg(p::MAGENTA.bright);
        let syn_const = Style::fg(p::ORANGE.bright);
        let syn_field = Style::fg(p::BLUE.base);
        let syn_func = Style::fg(p::BLUE.bright);
        let syn_ident = Style::fg(p::CYAN.base);
        let syn_keyword = Style::fg(p::MAGENTA.base);
        let syn_number = Style::fg(p::ORANGE.base);
        let syn_operator = Style::fg(p::FG2);
        let syn_preproc = Style::fg(p::PINK.bright);
        let syn_regex = Style::fg(p::YELLOW.bright);
        let syn_string = Style::fg(p::GREEN.base);
        let syn_type = Style::fg(p::YELLOW.base);
        let syn_variable = Style::fg(p::WHITE.base);

        let syntax: HashMap<String, Style> = [
            ("attribute", syn_preproc),
            ("boolean", syn_number),
            ("character", syn_string),
            ("comment", syn_comment),
            ("comment.documentation", syn_comment),
            ("constant", syn_const),
            ("constant.builtin", syn_builtin2),
            ("constant.character", syn_string),
            ("constant.character.escape", syn_regex.bold()),
            ("constant.macro", syn_builtin0),
            ("constant.numeric", syn_number),
            ("constructor", syn_ident),
            ("diff.plus", Style::fg(p::GREEN.base)),
            ("diff.minus", Style::fg(p::RED.base)),
            ("diff.delta", Style::fg(p::YELLOW.base)),
            ("escape", syn_regex.bold()),
            ("function", syn_func),
            ("function.builtin", syn_builtin0),
            ("function.macro", syn_builtin0),
            ("keyword", syn_keyword),
            ("keyword.conditional", syn_conditional),
            ("keyword.control", syn_conditional),
            ("keyword.directive", syn_preproc),
            ("keyword.function", syn_keyword),
            ("keyword.import", syn_preproc),
            ("keyword.operator", syn_operator),
            ("keyword.repeat", syn_conditional),
            ("keyword.return", syn_keyword),
            ("keyword.storage", syn_keyword),
            ("label", syn_conditional),
            ("lifetime", syn_builtin1),
            ("markup.bold", Style::default().bold()),
            ("markup.heading", syn_func.bold()),
            ("markup.italic", Style::default().italic()),
            ("markup.link", syn_const.underline()),
            ("markup.link.url", syn_const.underline()),
            ("markup.list", syn_operator),
            ("markup.quote", syn_comment),
            ("markup.raw", syn_string),
            ("module", syn_ident),
            ("namespace", syn_ident),
            ("number", syn_number),
            ("operator", syn_operator),
            ("property", syn_field),
            ("punctuation", syn_bracket),
            ("punctuation.special", syn_operator),
            ("string", syn_string),
            ("string.escape", syn_regex.bold()),
            ("string.regexp", syn_regex),
            ("string.special", syn_const),
            ("tag", syn_keyword),
            ("type", syn_type),
            ("type.builtin", syn_builtin1),
            ("variable", syn_variable),
            ("variable.builtin", syn_builtin0),
            ("variable.member", syn_field),
            ("variable.other.member", syn_field),
            ("variable.parameter", syn_ident),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        Theme {
            name: "duskfox".into(),
            background: p::BG1,
            ui,
            diagnostic,
            diff,
            agent,
            syntax,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(s: Style, fallback_bg: Rgb) -> f64 {
        s.fg.unwrap().contrast(s.bg.unwrap_or(fallback_bg))
    }

    #[test]
    fn hex_and_blend() {
        assert_eq!(Rgb::hex(0x232136), Rgb(0x23, 0x21, 0x36));
        assert_eq!(Rgb(0, 0, 0).blend(Rgb(200, 100, 50), 0.5), Rgb(100, 50, 25));
        let white = Rgb(255, 255, 255);
        let black = Rgb(0, 0, 0);
        assert!((white.contrast(black) - 21.0).abs() < 1e-9);
    }

    /// Every pair of interface text and the surface it is drawn on must meet
    /// WCAG AA (4.5:1). Gutter, cursor and selection pairs are checked too
    /// because they carry information.
    #[test]
    fn ui_text_meets_contrast() {
        let t = Theme::duskfox();
        let ui = &t.ui;
        let bg = t.background;
        let pairs: Vec<(&str, f64)> = vec![
            ("text", ratio(ui.text, bg)),
            ("text_muted", ratio(ui.text_muted, bg)),
            ("gutter", ratio(ui.gutter, bg)),
            ("gutter_current", ratio(ui.gutter_current, bg)),
            ("cursor", ratio(ui.cursor, bg)),
            ("statusline", ratio(ui.statusline, bg)),
            ("statusline_muted", ratio(ui.statusline_muted, bg)),
            ("statusline_inactive", ratio(ui.statusline_inactive, bg)),
            ("mode_normal", ratio(ui.mode_normal, bg)),
            ("mode_insert", ratio(ui.mode_insert, bg)),
            ("mode_select", ratio(ui.mode_select, bg)),
            ("mode_command", ratio(ui.mode_command, bg)),
            ("tab_active", ratio(ui.tab_active, bg)),
            ("tab_inactive", ratio(ui.tab_inactive, bg)),
            ("popup", ratio(ui.popup, bg)),
            ("popup_muted", ratio(ui.popup_muted, bg)),
            ("popup_selected", ratio(ui.popup_selected, bg)),
            ("popup_title", ratio(ui.popup_title, bg)),
            (
                "popup_match_on_popup",
                ui.popup_match.fg.unwrap().contrast(ui.popup.bg.unwrap()),
            ),
            (
                "popup_match_on_selected",
                ui.popup_match_selected
                    .fg
                    .unwrap()
                    .contrast(ui.popup_selected.bg.unwrap()),
            ),
            ("search_match", ratio(ui.search_match, bg)),
            ("message_info", ratio(ui.message_info, bg)),
            ("message_warning", ratio(ui.message_warning, bg)),
            ("message_error", ratio(ui.message_error, bg)),
            (
                "text_on_selection",
                ui.text.fg.unwrap().contrast(ui.selection.bg.unwrap()),
            ),
            (
                "text_on_primary_selection",
                ui.text
                    .fg
                    .unwrap()
                    .contrast(ui.selection_primary.bg.unwrap()),
            ),
            (
                "text_on_cursorline",
                ui.text.fg.unwrap().contrast(ui.cursorline.bg.unwrap()),
            ),
        ];
        let failing: Vec<_> = pairs.iter().filter(|(_, r)| *r < 4.5).collect();
        assert!(failing.is_empty(), "pairs under 4.5:1: {failing:?}");
    }

    /// Git signs (`+`, `~`, `-`) are text in the gutter, on both the plain
    /// and the current-line gutter background.
    #[test]
    fn git_signs_meet_contrast_on_the_gutter() {
        let t = Theme::duskfox();
        let d = &t.diff;
        let mut failing = Vec::new();
        for (name, sign) in [
            ("git_add", d.git_add),
            ("git_changed", d.git_changed),
            ("git_removed", d.git_removed),
        ] {
            for (surface, gutter) in [
                ("gutter", t.ui.gutter),
                ("gutter_current", t.ui.gutter_current),
            ] {
                let r = sign.fg.unwrap().contrast(gutter.bg.unwrap());
                if r < 4.5 {
                    failing.push((name, surface, r));
                }
            }
        }
        assert!(failing.is_empty(), "under 4.5:1: {failing:?}");
    }

    /// Borders identify panes, so they need 3:1 (WCAG 1.4.11) when active.
    #[test]
    fn active_border_meets_non_text_contrast() {
        let t = Theme::duskfox();
        assert!(ratio(t.ui.border_active, t.background) >= 3.0);
    }

    #[test]
    fn syntax_falls_back_to_parent_scope() {
        let t = Theme::duskfox();
        assert_eq!(t.syntax("function.method.call"), t.syntax("function"));
        assert!(t.syntax("nonexistent").is_none());
    }
}
