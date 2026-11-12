use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::App;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use serde::Deserialize;

pub const DEFAULT_SETTINGS: &str = r#"# redcode settings. Saving this file inside redcode applies it immediately.

[appearance]
theme = "system"        # "system" | "dark" | "light"
font_family = "Menlo"
font_size = 13.0
line_height = 1.3

[terminal]
shell = ""              # empty: $SHELL, then /bin/zsh
shell_args = ["-l"]
scrollback_lines = 10000
cursor_blink = true
option_as_meta = true
copy_on_select = false

[editor]
tab_size = 4
soft_wrap = false

[agent]
backend = "acp"         # "acp" | "native"

[agent.acp]
# Runs as: /bin/zsh -lc "exec <command>"
command = "npx -y @agentclientprotocol/claude-agent-acp@0.106.0"

[agent.acp.env]

[agent.native]
model = "claude-opus-5-5"
max_tokens = 32000
thinking = "adaptive"   # "adaptive" | "off"
effort = ""             # empty: model default
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
"#;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub terminal: TerminalSettings,
    pub editor: EditorSettings,
    pub agent: AgentSettings,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: String,
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            font_family: "Menlo".into(),
            font_size: 13.0,
            line_height: 1.3,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    pub shell: String,
    pub shell_args: Vec<String>,
    pub scrollback_lines: usize,
    pub cursor_blink: bool,
    pub option_as_meta: bool,
    pub copy_on_select: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            shell: String::new(),
            shell_args: vec!["-l".into()],
            scrollback_lines: 10000,
            cursor_blink: true,
            option_as_meta: true,
            copy_on_select: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub tab_size: usize,
    pub soft_wrap: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            tab_size: 4,
            soft_wrap: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Acp,
    Native,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub backend: Backend,
    pub acp: AcpSettings,
    pub native: NativeSettings,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AcpSettings {
    pub command: String,
    pub env: BTreeMap<String, String>,
}

impl Default for AcpSettings {
    fn default() -> Self {
        Self {
            command: "npx -y @agentclientprotocol/claude-agent-acp@0.106.0".into(),
            env: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NativeSettings {
    pub model: String,
    pub max_tokens: u32,
    pub thinking: String,
    pub effort: String,
    pub base_url: String,
    pub api_key_env: String,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            max_tokens: 32000,
            thinking: "adaptive".into(),
            effort: String::new(),
            base_url: "https://api.anthropic.com".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
        }
    }
}

impl gpui::Global for Settings {}

/// `$HOME/.config/redcode/settings.toml`
pub fn path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config/redcode/settings.toml")
}

fn load() -> Result<Settings, String> {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(err) = std::fs::write(&path, DEFAULT_SETTINGS) {
                eprintln!("settings.toml: {err}");
            }
            DEFAULT_SETTINGS.to_string()
        }
        Err(err) => return Err(err.to_string()),
    };
    toml::from_str(&text).map_err(|err| err.to_string())
}

/// Creates the file with [`DEFAULT_SETTINGS`] when missing, then loads it.
pub fn init(cx: &mut App) {
    let settings = load().unwrap_or_else(|err| {
        eprintln!("settings.toml: {err}");
        Settings::default()
    });
    cx.set_global(settings);
}

/// Re-reads the file, replaces the global, re-applies the theme and refreshes all windows.
pub fn reload(cx: &mut App) {
    let settings = match load() {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("settings.toml: {err}");
            if let Some(window) = cx.active_window() {
                let message = format!("settings.toml: {err}");
                let _ = window.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
            Settings::default()
        }
    };
    cx.set_global(settings);
    crate::app::apply_theme(cx);
    cx.refresh_windows();
}

/// The font size configured in the file, ignoring any zoom applied at runtime.
pub fn file_font_size() -> f32 {
    load().unwrap_or_default().appearance.font_size
}

pub fn get(cx: &App) -> &Settings {
    cx.global::<Settings>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_default_struct() {
        let parsed: Settings = toml::from_str(DEFAULT_SETTINGS).unwrap();
        let default = Settings::default();
        assert_eq!(format!("{parsed:?}"), format!("{default:?}"));
    }
}
