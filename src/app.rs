use std::path::PathBuf;

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Menu, MenuItem, PathPromptOptions, WindowBounds,
    WindowOptions, px, size,
};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, TitleBar};

use crate::actions::{self, agent, app as app_actions, editor, git, terminal, workspace};
use crate::settings::{self, Settings};
use crate::workspace::Workspace;

const MIN_FONT_SIZE: f32 = 8.;
const MAX_FONT_SIZE: f32 = 32.;

pub fn init(cx: &mut App) {
    gpui_kit::init(cx);
    settings::init(cx);
    apply_theme(cx);
    actions::bind_keys(cx);

    cx.on_action(|_: &app_actions::Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &app_actions::OpenFolder, cx: &mut App| open_folder(cx));
    cx.on_action(|_: &app_actions::NewWindow, cx: &mut App| {
        open_workspace_window(home_dir(), None, cx);
    });
    cx.on_action(|_: &app_actions::ReloadSettings, cx: &mut App| settings::reload(cx));
    cx.on_action(|_: &app_actions::ToggleTheme, cx: &mut App| {
        let mode = if cx.theme().is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, None, cx);
        cx.refresh_windows();
    });
    cx.on_action(|_: &app_actions::ZoomIn, cx: &mut App| zoom(cx, |size| size + 1.));
    cx.on_action(|_: &app_actions::ZoomOut, cx: &mut App| zoom(cx, |size| size - 1.));
    cx.on_action(|_: &app_actions::ZoomReset, cx: &mut App| {
        let size = settings::file_font_size();
        zoom(cx, |_| size);
    });

    set_menus(cx);
}

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

pub fn apply_theme(cx: &mut App) {
    match settings::get(cx).appearance.theme.as_str() {
        "dark" => Theme::change(ThemeMode::Dark, None, cx),
        "light" => Theme::change(ThemeMode::Light, None, cx),
        _ => Theme::sync_system_appearance(None, cx),
    }
}

/// Changes the in-memory font size only; the settings file is left alone.
fn zoom(cx: &mut App, f: impl FnOnce(f32) -> f32) {
    let appearance = &mut cx.global_mut::<Settings>().appearance;
    appearance.font_size = f(appearance.font_size).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    cx.refresh_windows();
}

fn open_folder(cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: None,
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        if let Ok(Ok(Some(paths))) = paths.await
            && let Some(dir) = paths.into_iter().next()
        {
            cx.update(|cx| open_workspace_window(dir, None, cx));
        }
    })
    .detach();
}

fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("redcode").items([
            MenuItem::action("Settings…", app_actions::OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit", app_actions::Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Window", app_actions::NewWindow),
            MenuItem::action("Open Folder…", app_actions::OpenFolder),
            MenuItem::action("Open File…", workspace::OpenFile),
            MenuItem::action("Go to File…", workspace::GoToFile),
            MenuItem::separator(),
            MenuItem::action("Save", editor::Save),
            MenuItem::action("Close Tab", workspace::CloseActiveTab),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", workspace::ToggleCommandPalette),
            MenuItem::action("Toggle Sidebar", workspace::ToggleSidebar),
            MenuItem::separator(),
            MenuItem::action("Zoom In", app_actions::ZoomIn),
            MenuItem::action("Zoom Out", app_actions::ZoomOut),
            MenuItem::action("Reset Zoom", app_actions::ZoomReset),
            MenuItem::separator(),
            MenuItem::action("Toggle Theme", app_actions::ToggleTheme),
        ]),
        Menu::new("Terminal").items([
            MenuItem::action("New Terminal", workspace::NewTerminal),
            MenuItem::action("Split Right", workspace::SplitRight),
            MenuItem::action("Split Down", workspace::SplitDown),
            MenuItem::separator(),
            MenuItem::action("Clear", terminal::Clear),
        ]),
        Menu::new("Git").items([
            MenuItem::action("Show Graph", git::ShowGraph),
            MenuItem::separator(),
            MenuItem::action("Fetch", git::Fetch),
            MenuItem::action("Pull", git::Pull),
            MenuItem::action("Push", git::Push),
        ]),
        Menu::new("Agent").items([MenuItem::action("New Thread", agent::NewThread)]),
    ]);
}

pub fn open_workspace_window(root: PathBuf, open_file: Option<PathBuf>, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1400.), px(900.)),
            cx,
        ))),
        window_min_size: Some(size(px(640.), px(400.))),
        ..TitleBar::window_options()
    };
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Workspace::new(root, open_file, window, cx))
    }) {
        eprintln!("failed to open window: {err}");
    }
}
