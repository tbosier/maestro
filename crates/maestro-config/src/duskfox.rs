//! The Duskfox palette from nightfox.nvim.
//!
//! Source: <https://github.com/EdenEast/nightfox.nvim>, file
//! `lua/nightfox/palette/duskfox.lua`. Copyright (c) 2021 James Simpson,
//! MIT License; the full license text is in `third_party/nightfox-LICENSE`
//! and the source file is vendored as `third_party/duskfox.lua`.
//!
//! The blended diff and diagnostic backgrounds are computed the same way
//! upstream computes them (`bg1` blended toward the color by a fixed
//! amount) rather than copied, so they stay correct if the palette changes.

use crate::theme::Rgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shade {
    pub base: Rgb,
    pub bright: Rgb,
    pub dim: Rgb,
}

const fn shade(base: u32, bright: u32, dim: u32) -> Shade {
    Shade {
        base: Rgb::hex(base),
        bright: Rgb::hex(bright),
        dim: Rgb::hex(dim),
    }
}

pub const BLACK: Shade = shade(0x393552, 0x47407d, 0x322e42);
pub const RED: Shade = shade(0xeb6f92, 0xf083a2, 0xd84f76);
pub const GREEN: Shade = shade(0xa3be8c, 0xb1d196, 0x8aa872);
pub const YELLOW: Shade = shade(0xf6c177, 0xf9cb8c, 0xe6a852);
pub const BLUE: Shade = shade(0x569fba, 0x65b1cd, 0x4a869c);
pub const MAGENTA: Shade = shade(0xc4a7e7, 0xccb1ed, 0xa580d2);
pub const CYAN: Shade = shade(0x9ccfd8, 0xa6dae3, 0x7bb8c1);
pub const WHITE: Shade = shade(0xe0def4, 0xe2e0f7, 0xb1acde);
pub const ORANGE: Shade = shade(0xea9a97, 0xf0a4a2, 0xd6746f);
pub const PINK: Shade = shade(0xeb98c3, 0xf0a6cc, 0xd871a6);

pub const COMMENT: Rgb = Rgb::hex(0x817c9c);

/// Dark background: status line and floats.
pub const BG0: Rgb = Rgb::hex(0x191726);
/// Default background.
pub const BG1: Rgb = Rgb::hex(0x232136);
/// Lighter background: elevated surfaces.
pub const BG2: Rgb = Rgb::hex(0x2d2a45);
/// Cursor line.
pub const BG3: Rgb = Rgb::hex(0x373354);
/// Borders and conceal.
pub const BG4: Rgb = Rgb::hex(0x4b4673);

pub const FG0: Rgb = Rgb::hex(0xeae8ff);
pub const FG1: Rgb = Rgb::hex(0xe0def4);
pub const FG2: Rgb = Rgb::hex(0xcdcbe0);
pub const FG3: Rgb = Rgb::hex(0x6e6a86);

/// Popup background and visual selection.
pub const SEL0: Rgb = Rgb::hex(0x433c59);
/// Popup selection and search highlight.
pub const SEL1: Rgb = Rgb::hex(0x63577d);
