mod actions;
mod agent;
mod app;
mod diff_panel;
mod files;
mod fs_watch;
mod git;
mod picker;
mod settings;
mod terminal;
mod workspace;

use std::path::PathBuf;

use gpui::QuitMode;

/// Resolves the workspace root and an optional file to open from the first CLI argument.
fn resolve_args() -> (PathBuf, Option<PathBuf>) {
    if let Some(arg) = std::env::args_os().nth(1)
        && let Ok(path) = std::fs::canonicalize(&arg)
    {
        if path.is_dir() {
            return (path, None);
        }
        if let Some(parent) = path.parent() {
            return (parent.to_path_buf(), Some(path));
        }
    }
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
    match std::env::current_dir() {
        Ok(dir) if dir != std::path::Path::new("/") => (dir, None),
        _ => (home(), None),
    }
}

fn main() {
    let (root, open_file) = resolve_args();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            app::init(cx);
            app::open_workspace_window(root, open_file, cx);
            cx.activate(true);
        });
}
