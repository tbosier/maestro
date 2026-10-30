//! Configuration and theming.
//!
//! Every color the interface draws comes from a [`theme::Theme`] token.
//! Hex values appear in exactly one place, [`duskfox`], which mirrors the
//! upstream palette file.

pub mod config;
pub mod duskfox;
pub mod theme;

pub use config::{
    Config, ConfigError, EditorConfig, IconMode, IndentStyle, LanguageServerConfig, LineNumbers,
    Profile,
};
pub use theme::{Rgb, Style, Theme};
