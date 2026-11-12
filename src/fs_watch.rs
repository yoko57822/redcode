use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncApp, Context, EventEmitter, Task, WeakEntity};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecursiveMode, Watcher as _};

/// Watches the workspace root recursively and emits debounced, filtered change batches.
pub struct FsWatcher {
    _watcher: Option<notify::RecommendedWatcher>,
    _task: Task<()>,
}

/// Absolute, de-duplicated paths under the workspace root.
pub struct FsChanged(pub Arc<[PathBuf]>);

impl EventEmitter<FsChanged> for FsWatcher {}

impl FsWatcher {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<Vec<PathBuf>>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = tx.try_send(event.paths);
            }
        })
        .and_then(|mut watcher| {
            watcher.watch(&root, RecursiveMode::Recursive)?;
            Ok(watcher)
        });
        // Without a watcher the channel closes and the task ends: manual refresh only.
        let watcher = watcher
            .map_err(|err| eprintln!("fs watch {}: {err}", root.display()))
            .ok();

        let task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // FSEvents reports canonical paths; map them back under the root as it was given.
            let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            while let Ok(mut paths) = rx.recv().await {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                while let Ok(more) = rx.try_recv() {
                    paths.extend(more);
                }
                let changed = filter_changes(&root, &canonical, paths);
                if changed.is_empty() {
                    continue;
                }
                let emitted = this.update(cx, |_, cx| cx.emit(FsChanged(changed.into())));
                if emitted.is_err() {
                    break;
                }
            }
        });

        Self {
            _watcher: watcher,
            _task: task,
        }
    }
}

/// Drops ignored paths. Paths under `.git/` bypass the ignore rules, except `.git/objects/` and
/// lock files, which only add noise.
fn filter_changes(root: &Path, canonical: &Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git/info/exclude"));
    let ignore = builder.build().unwrap_or_else(|_| Gitignore::empty());

    let mut changed = BTreeSet::new();
    for path in paths {
        let Ok(relative) = path
            .strip_prefix(canonical)
            .or_else(|_| path.strip_prefix(root))
        else {
            continue;
        };
        let path = root.join(relative);
        if relative.starts_with(".git") {
            if relative.starts_with(".git/objects") || relative.to_string_lossy().ends_with(".lock")
            {
                continue;
            }
        } else if ignore
            .matched_path_or_any_parents(&path, path.is_dir())
            .is_ignore()
        {
            continue;
        }
        changed.insert(path);
    }
    changed.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_ignored_paths_but_keeps_git_metadata() {
        let root = std::env::temp_dir().join(format!("redcode-fswatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join(".gitignore"), "/target\n*.log\n").unwrap();
        std::fs::write(root.join(".git/info/exclude"), "secret.txt\n").unwrap();

        let paths = vec![
            root.join("src/main.rs"),
            root.join("src/main.rs"),
            root.join("target/debug/app"),
            root.join("build.log"),
            root.join("secret.txt"),
            root.join(".git/HEAD"),
            root.join(".git/index.lock"),
            root.join(".git/objects/ab/cdef"),
            PathBuf::from("/somewhere/else"),
        ];
        let changed = filter_changes(&root, &root, paths);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            changed,
            vec![root.join(".git/HEAD"), root.join("src/main.rs")]
        );
    }
}
