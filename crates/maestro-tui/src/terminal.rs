//! Terminal lifecycle, color capability detection and frame flushing.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use maestro_config::{Rgb, Style};

use crate::render::CursorShape;
use crate::surface::Surface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
}

impl ColorMode {
    /// Detects 24-bit color support. `MAESTRO_COLOR=truecolor|256` overrides.
    pub fn detect() -> Self {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        match var("MAESTRO_COLOR").as_str() {
            "truecolor" | "24bit" => return ColorMode::TrueColor,
            "256" => return ColorMode::Ansi256,
            _ => {}
        }
        let colorterm = var("COLORTERM");
        if colorterm == "truecolor" || colorterm == "24bit" {
            return ColorMode::TrueColor;
        }
        if var("TERM_PROGRAM") == "Apple_Terminal" {
            return ColorMode::Ansi256;
        }
        let term = var("TERM");
        if term.contains("direct")
            || ["kitty", "alacritty", "wezterm", "ghostty", "foot"]
                .iter()
                .any(|t| term.contains(t))
            || !var("WT_SESSION").is_empty()
        {
            return ColorMode::TrueColor;
        }
        ColorMode::Ansi256
    }

    pub fn color(self, c: Rgb) -> Color {
        match self {
            ColorMode::TrueColor => Color::Rgb {
                r: c.0,
                g: c.1,
                b: c.2,
            },
            ColorMode::Ansi256 => Color::AnsiValue(rgb_to_256(c)),
        }
    }
}

/// Nearest xterm-256 color: the closer of the 6x6x6 cube and the gray ramp.
pub fn rgb_to_256(c: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (**l as i32 - v as i32).abs())
            .map(|(i, _)| i)
            .expect("levels non-empty")
    };
    let (ri, gi, bi) = (nearest(c.0), nearest(c.1), nearest(c.2));
    let cube = Rgb(LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let avg = (c.0 as u32 + c.1 as u32 + c.2 as u32) / 3;
    let gi_idx = if avg < 8 { 0 } else { ((avg - 8) / 10).min(23) };
    let gray_v = (8 + gi_idx * 10) as u8;
    let gray = Rgb(gray_v, gray_v, gray_v);
    let dist = |a: Rgb, b: Rgb| {
        let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2);
        d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
    };
    if dist(c, gray) < dist(c, cube) {
        232 + gi_idx as u8
    } else {
        16 + (36 * ri + 6 * gi + bi) as u8
    }
}

/// Puts the terminal into editor mode and restores it on drop, including
/// when unwinding from a panic.
pub struct TerminalGuard {
    keyboard_enhanced: bool,
    mouse: bool,
}

impl TerminalGuard {
    pub fn enter(mouse: bool) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        // Disambiguated keys make Esc immediate and tell C-i from Tab where
        // the terminal supports the kitty keyboard protocol.
        let keyboard_enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
        if keyboard_enhanced {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        Ok(Self {
            keyboard_enhanced,
            mouse,
        })
    }
}

/// Best-effort restore, shared by `Drop` and the panic hook.
pub fn restore(keyboard_enhanced: bool, mouse: bool) {
    let mut out = io::stdout();
    if keyboard_enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape,
        SetAttribute(Attribute::Reset),
        Show,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(self.keyboard_enhanced, self.mouse);
    }
}

/// Restores the terminal before the default panic message prints, so the
/// message is readable and the shell is usable afterwards.
pub fn install_panic_hook(mouse: bool) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(true, mouse);
        default(info);
    }));
}

/// Writes the cells of `next` that differ from `prev` (all of them when
/// `prev` is `None`).
pub fn flush(
    out: &mut impl Write,
    prev: Option<&Surface>,
    next: &Surface,
    mode: ColorMode,
    cursor: CursorShape,
) -> io::Result<usize> {
    queue!(out, BeginSynchronizedUpdate, Hide)?;
    let mut current: Option<Style> = None;
    let mut written = 0;
    for y in 0..next.height {
        let row = next.row(y);
        let prev_row = prev
            .filter(|p| p.width == next.width && p.height == next.height)
            .map(|p| p.row(y));
        let mut expect_x: Option<u16> = None;
        let mut x = 0u16;
        while (x as usize) < row.len() {
            let cell = &row[x as usize];
            let unchanged = prev_row.is_some_and(|pr| pr[x as usize] == *cell);
            if cell.symbol.is_empty() || unchanged {
                x += 1;
                continue;
            }
            if expect_x != Some(x) {
                queue!(out, MoveTo(x, y))?;
            }
            if current != Some(cell.style) {
                apply_style(out, current, cell.style, mode)?;
                current = Some(cell.style);
            }
            queue!(out, Print(&cell.symbol))?;
            written += 1;
            let w = unicode_width::UnicodeWidthStr::width(cell.symbol.as_str()).max(1) as u16;
            x += w;
            expect_x = Some(x);
        }
    }
    queue!(out, SetAttribute(Attribute::Reset))?;
    match cursor {
        CursorShape::Hidden => {}
        CursorShape::Bar(x, y) => {
            queue!(out, MoveTo(x, y), SetCursorStyle::SteadyBar, Show)?;
        }
    }
    queue!(out, EndSynchronizedUpdate)?;
    out.flush()?;
    Ok(written)
}

fn apply_style(
    out: &mut impl Write,
    prev: Option<Style>,
    style: Style,
    mode: ColorMode,
) -> io::Result<()> {
    let attrs_changed = prev.is_none_or(|p| {
        p.bold != style.bold || p.italic != style.italic || p.underline != style.underline
    });
    if attrs_changed {
        queue!(out, SetAttribute(Attribute::Reset))?;
        if style.bold {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        if style.italic {
            queue!(out, SetAttribute(Attribute::Italic))?;
        }
        if style.underline {
            queue!(out, SetAttribute(Attribute::Underlined))?;
        }
    }
    let fg = style.fg.map(|c| mode.color(c)).unwrap_or(Color::Reset);
    let bg = style.bg.map(|c| mode.color(c)).unwrap_or(Color::Reset);
    if attrs_changed || prev.is_none_or(|p| p.fg != style.fg) {
        queue!(out, SetForegroundColor(fg))?;
    }
    if attrs_changed || prev.is_none_or(|p| p.bg != style.bg) {
        queue!(out, SetBackgroundColor(bg))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi256_mapping() {
        assert_eq!(rgb_to_256(Rgb(0, 0, 0)), 16);
        assert_eq!(rgb_to_256(Rgb(255, 255, 255)), 231);
        assert_eq!(rgb_to_256(Rgb(128, 128, 128)), 244);
        assert_eq!(rgb_to_256(Rgb(255, 0, 0)), 196);
    }

    #[test]
    fn flush_writes_only_changed_cells() {
        let st = Style::default();
        let a = Surface::new(10, 2, st);
        let mut b = a.clone();
        b.print(3, 1, "hi", st, 10);
        let mut out = Vec::new();
        let n = flush(
            &mut out,
            Some(&a),
            &b,
            ColorMode::TrueColor,
            CursorShape::Hidden,
        )
        .unwrap();
        assert_eq!(n, 2);
        let n = flush(
            &mut out,
            None,
            &b,
            ColorMode::TrueColor,
            CursorShape::Hidden,
        )
        .unwrap();
        assert_eq!(n, 20);
    }
}
