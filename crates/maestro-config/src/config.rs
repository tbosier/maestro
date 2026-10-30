//! User configuration, read from `$XDG_CONFIG_HOME/maestro/config.toml`
//! (or the platform equivalent). Every field has a default, so an empty or
//! missing file is a complete configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// Selection-first hybrid: Kakoune/Helix selections with Vim-familiar
    /// motions and commands.
    #[default]
    Maestro,
    /// Vim operator-pending grammar where implemented (see docs/editor-model.md).
    Vim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LineNumbers {
    #[default]
    Absolute,
    Relative,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IndentStyle {
    #[default]
    Spaces,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IconMode {
    /// Plain text markers. Works in every terminal and font.
    #[default]
    Text,
    /// Nerd Font glyphs. Only enable if your terminal font has them.
    Nerd,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct EditorConfig {
    pub tab_width: usize,
    pub indent: IndentStyle,
    pub indent_width: usize,
    pub line_numbers: LineNumbers,
    pub cursorline: bool,
    /// Lines kept visible above and below the cursor when scrolling.
    pub scrolloff: usize,
    pub icons: IconMode,
    /// Seconds of idle time before unsaved changes are written to the
    /// recovery directory. 0 disables recovery files.
    pub recovery_interval_secs: u64,
    pub mouse: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: 4,
            indent: IndentStyle::Spaces,
            indent_width: 4,
            line_numbers: LineNumbers::Absolute,
            cursorline: true,
            scrolloff: 5,
            icons: IconMode::Text,
            recovery_interval_secs: 2,
            mouse: true,
        }
    }
}

impl EditorConfig {
    pub fn indent_unit(&self) -> String {
        match self.indent {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces => " ".repeat(self.indent_width.max(1)),
        }
    }
}

/// A language server for one language: `[language-server.rust]`.
///
/// Servers are run by name from this config or the built-in defaults, never
/// from files inside a repository (see docs/security.md).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LanguageServerConfig {
    /// Program name looked up on `PATH`, or a path.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `false` turns the language's server off.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl LanguageServerConfig {
    pub fn new(command: &str, args: &[&str]) -> Self {
        Self {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            enabled: true,
        }
    }
}

/// Built-in servers per language, in order of preference. The first one
/// found on `PATH` is used.
pub fn default_language_servers(language: &str) -> Vec<LanguageServerConfig> {
    let ts = || LanguageServerConfig::new("typescript-language-server", &["--stdio"]);
    match language {
        "rust" => vec![LanguageServerConfig::new("rust-analyzer", &[])],
        "python" => vec![
            LanguageServerConfig::new("pyright-langserver", &["--stdio"]),
            LanguageServerConfig::new("pylsp", &[]),
        ],
        "typescript" | "javascript" => vec![ts()],
        "c" | "cpp" => vec![LanguageServerConfig::new("clangd", &[])],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub theme: String,
    pub profile: Profile,
    pub editor: EditorConfig,
    /// Extra key bindings per mode: `[keys.normal] "space w" = "write"`.
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
    /// Language servers that replace the built-in choice for a language.
    pub language_server: BTreeMap<String, LanguageServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "duskfox".into(),
            profile: Profile::default(),
            editor: EditorConfig::default(),
            keys: BTreeMap::new(),
            language_server: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Server candidates for `language`: the user's entry if there is one
    /// (none if it is disabled), else the built-in defaults.
    pub fn language_servers(&self, language: &str) -> Vec<LanguageServerConfig> {
        match self.language_server.get(language) {
            Some(c) if c.enabled => vec![c.clone()],
            Some(_) => Vec::new(),
            None => default_language_servers(language),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(e),
        })
    }

    /// Loads the user config. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

/// `$XDG_CONFIG_HOME/maestro`, falling back to `~/.config/maestro`, or
/// `%APPDATA%\maestro` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("maestro"));
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("maestro"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("maestro"))
}

/// `$XDG_STATE_HOME/maestro`, falling back to `~/.local/state/maestro`, or
/// `%LOCALAPPDATA%\maestro` on Windows. Holds sessions and recovery files.
pub fn state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("maestro"));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("maestro"));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".local")
            .join("state")
            .join("maestro")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_default() {
        assert_eq!(
            Config::parse("", Path::new("x")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn parses_fields_and_rejects_typos() {
        let c = Config::parse(
            r#"
profile = "vim"
[editor]
tab-width = 8
indent = "tabs"
line-numbers = "relative"
[keys.normal]
"space w" = "write"
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(c.profile, Profile::Vim);
        assert_eq!(c.editor.tab_width, 8);
        assert_eq!(c.editor.indent_unit(), "\t");
        assert_eq!(c.keys["normal"]["space w"], "write");
        assert!(Config::parse("[editor]\ntab-widht = 2\n", Path::new("x")).is_err());
    }

    #[test]
    fn language_servers_override_defaults_per_language() {
        let c = Config::parse(
            r#"
[language-server.rust]
command = "/opt/ra/rust-analyzer"
args = ["--log", "x"]
[language-server.python]
command = "pylsp"
enabled = false
"#,
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(
            c.language_servers("rust"),
            vec![LanguageServerConfig::new("/opt/ra/rust-analyzer", &["--log", "x"])]
        );
        assert!(c.language_servers("python").is_empty());
        // Untouched languages keep the defaults.
        assert_eq!(c.language_servers("c")[0].command, "clangd");
        assert_eq!(
            Config::default().language_servers("python")[1].command,
            "pylsp"
        );
        assert!(c.language_servers("cobol").is_empty());
        assert!(Config::parse("[language-server.rust]\ncmd = \"x\"\n", Path::new("x")).is_err());
    }
}
