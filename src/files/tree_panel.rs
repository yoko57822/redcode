use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::dock::PanelEvent;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use ignore::WalkBuilder;

use crate::fs_watch::{FsChanged, FsWatcher};
use crate::workspace::Workspace;

/// Scanning stops after this many entries; the tree then shows a partial listing.
const MAX_ENTRIES: usize = 50_000;
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(300);

/// One file or directory of a scan, in display order.
#[derive(Debug, PartialEq)]
struct Node {
    name: String,
    /// Root-relative path; also the tree item id.
    rel: String,
    is_dir: bool,
    children: Vec<Node>,
}

struct Scan {
    nodes: Vec<Node>,
    files: Arc<[Arc<Path>]>,
    dirs: HashSet<SharedString>,
    truncated: bool,
}

/// Walks `root` honoring ignore files, showing hidden entries but never `.git`.
fn scan_dir(root: &Path) -> Scan {
    let mut entries = Vec::new();
    let mut truncated = false;
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
        entries.push((rel.to_path_buf(), is_dir));
    }

    let mut files: Vec<Arc<Path>> = entries
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(rel, _)| Arc::from(rel.as_path()))
        .collect();
    files.sort();
    let dirs = entries
        .iter()
        .filter(|(_, is_dir)| *is_dir)
        .map(|(rel, _)| SharedString::from(rel.to_string_lossy().into_owned()))
        .collect();
    Scan {
        nodes: build_nodes(entries),
        files: files.into(),
        dirs,
        truncated,
    }
}

/// Nests root-relative entries under their parents: directories first, then names compared
/// case-insensitively. Entries whose parent is missing are dropped.
fn build_nodes(entries: Vec<(PathBuf, bool)>) -> Vec<Node> {
    let mut by_parent: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
    for (rel, is_dir) in entries {
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        by_parent.entry(parent).or_default().push((rel, is_dir));
    }
    assemble(Path::new(""), &mut by_parent)
}

fn assemble(parent: &Path, by_parent: &mut HashMap<PathBuf, Vec<(PathBuf, bool)>>) -> Vec<Node> {
    let Some(children) = by_parent.remove(parent) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = children
        .into_iter()
        .map(|(rel, is_dir)| Node {
            name: rel
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            rel: rel.to_string_lossy().into_owned(),
            is_dir,
            children: if is_dir {
                assemble(&rel, by_parent)
            } else {
                Vec::new()
            },
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

fn tree_items(nodes: &[Node], expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
    nodes
        .iter()
        .map(|node| {
            let id = SharedString::from(node.rel.clone());
            let is_expanded = expanded.contains(&id);
            TreeItem::new(id, node.name.clone())
                .children(tree_items(&node.children, expanded))
                .expanded(is_expanded)
        })
        .collect()
}

/// The workspace file tree in the left dock.
pub struct FileTreePanel {
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    tree: Entity<TreeState>,
    focus_handle: FocusHandle,
    files: Arc<[Arc<Path>]>,
    /// Ids of directories; an empty directory has no children, so the tree cannot tell.
    dirs: Arc<HashSet<SharedString>>,
    /// Ids of expanded directories, restored after each rescan.
    expanded: HashSet<SharedString>,
    truncated: bool,
    scan_task: Option<Task<()>>,
    rescan_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl FileTreePanel {
    pub fn new(
        root: PathBuf,
        fs: Entity<FsWatcher>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let focus_handle = cx.focus_handle();
        let subs = vec![
            cx.subscribe(&tree, |this, _, event: &TreeEvent, _| match event {
                TreeEvent::Expanded(id) => {
                    this.expanded.insert(id.clone());
                }
                TreeEvent::Collapsed(id) => {
                    this.expanded.remove(id);
                }
            }),
            cx.subscribe(&fs, |this, _, event: &FsChanged, cx| {
                let outside_git = event.0.iter().any(|path| {
                    path.strip_prefix(&this.root)
                        .is_ok_and(|rel| !rel.starts_with(".git"))
                });
                if outside_git {
                    this.schedule_rescan(cx);
                }
            }),
            // The panel itself has no keyboard behavior; hand focus to the tree.
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                this.tree.update(cx, |tree, cx| tree.focus(window, cx));
            }),
        ];
        let mut this = Self {
            root,
            workspace,
            tree,
            focus_handle,
            files: Arc::from([]),
            dirs: Arc::default(),
            expanded: HashSet::new(),
            truncated: false,
            scan_task: None,
            rescan_task: None,
            _subs: subs,
        };
        this.scan(cx);
        this
    }

    /// Root-relative file paths from the latest scan.
    pub fn files(&self) -> Arc<[Arc<Path>]> {
        self.files.clone()
    }

    fn schedule_rescan(&mut self, cx: &mut Context<Self>) {
        self.rescan_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                cx.background_executor().timer(RESCAN_DEBOUNCE).await;
                this.update(cx, |this, cx| this.scan(cx)).ok();
            },
        ));
    }

    /// Starts a scan, dropping the one in progress.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        self.scan_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let scan = cx
                    .background_executor()
                    .spawn(async move { scan_dir(&root) })
                    .await;
                this.update(cx, |this, cx| this.apply_scan(scan, cx)).ok();
            }),
        );
    }

    fn apply_scan(&mut self, scan: Scan, cx: &mut Context<Self>) {
        let items = tree_items(&scan.nodes, &self.expanded);
        self.tree.update(cx, |tree, cx| {
            let selected = tree.selected_item().map(|item| item.id.clone());
            tree.set_items(items, cx);
            if let Some(ix) = selected.and_then(|id| tree.index_of(&id)) {
                tree.set_selected_index(Some(ix), cx);
            }
        });
        self.files = scan.files;
        self.dirs = Arc::new(scan.dirs);
        self.truncated = scan.truncated;
        cx.notify();
    }
}

/// Calls into the workspace after the current update, avoiding re-entrancy.
fn defer_workspace(
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let workspace = workspace.clone();
    window.defer(cx, move |window, cx| {
        workspace
            .update(cx, |workspace, cx| f(workspace, window, cx))
            .ok();
    });
}

impl gpui_kit::base::dock::Panel for FileTreePanel {
    fn panel_name(&self) -> &'static str {
        "Files"
    }

    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl gpui_kit::component::dock::Panel for FileTreePanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        "Files"
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for FileTreePanel {}

impl Focusable for FileTreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        let item_dirs = self.dirs.clone();
        let item_root = self.root.clone();
        let item_workspace = self.workspace.clone();
        let menu_dirs = self.dirs.clone();
        let menu_root = self.root.clone();
        let menu_workspace = self.workspace.clone();

        let files = tree(&self.tree, move |ix, entry, _selected, _window, _cx| {
            let item = entry.item();
            let is_dir = item_dirs.contains(&item.id);
            let icon = if !is_dir {
                IconName::File
            } else if entry.is_folder() && entry.is_expanded() {
                IconName::FolderOpen
            } else {
                IconName::Folder
            };
            let row = ListItem::new(ix)
                .pl(px(8. + entry.depth() as f32 * 14.))
                .py_0p5()
                .text_sm()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Icon::new(icon).small().text_color(muted))
                        .child(item.label.clone()),
                );
            if is_dir {
                return row;
            }
            let path = item_root.join(item.id.as_ref());
            let workspace = item_workspace.clone();
            row.on_click(move |_, window, cx| {
                let path = path.clone();
                defer_workspace(&workspace, window, cx, move |workspace, window, cx| {
                    workspace.open_path(path, None, window, cx)
                });
            })
        })
        .context_menu(move |_ix, entry, menu, _window, _cx| {
            let id = &entry.item().id;
            let path = menu_root.join(id.as_ref());
            let dir = if menu_dirs.contains(id) {
                path.clone()
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| menu_root.clone())
            };
            let reveal = path.clone();
            let text = path.to_string_lossy().into_owned();
            let workspace = menu_workspace.clone();
            menu.item(
                PopupMenuItem::new("Reveal in Finder")
                    .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
            )
            .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
            }))
            .item(
                PopupMenuItem::new("Open Terminal Here").on_click(move |_, window, cx| {
                    let dir = dir.clone();
                    defer_workspace(&workspace, window, cx, move |workspace, window, cx| {
                        workspace.new_terminal_at(dir, window, cx)
                    });
                }),
            )
        });

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(div().flex_1().min_h_0().child(files))
            .when(self.truncated, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(muted)
                        .child("항목이 많아 일부만 표시합니다"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(rel: &str, is_dir: bool, children: Vec<Node>) -> Node {
        Node {
            name: Path::new(rel)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            rel: rel.into(),
            is_dir,
            children,
        }
    }

    #[test]
    fn nests_entries_with_directories_first_case_insensitively() {
        let entries = vec![
            (PathBuf::from("b.txt"), false),
            (PathBuf::from("src"), true),
            (PathBuf::from("src/main.rs"), false),
            (PathBuf::from("src/Lib.rs"), false),
            (PathBuf::from("A.md"), false),
            (PathBuf::from("assets"), true),
            (PathBuf::from("Zeta"), true),
        ];
        assert_eq!(
            build_nodes(entries),
            vec![
                node("assets", true, vec![]),
                node(
                    "src",
                    true,
                    vec![
                        node("src/Lib.rs", false, vec![]),
                        node("src/main.rs", false, vec![])
                    ]
                ),
                node("Zeta", true, vec![]),
                node("A.md", false, vec![]),
                node("b.txt", false, vec![]),
            ]
        );
    }

    #[test]
    fn scan_skips_git_dir_but_keeps_hidden_files() {
        let root = std::env::temp_dir().join(format!("redcode-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("src/empty")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();

        let scan = scan_dir(&root);
        std::fs::remove_dir_all(&root).unwrap();

        let files: Vec<&Path> = scan.files.iter().map(|path| path.as_ref()).collect();
        assert_eq!(files, vec![Path::new(".env"), Path::new("src/main.rs")]);
        assert!(scan.dirs.contains("src") && scan.dirs.contains("src/empty"));
        assert!(!scan.truncated);
        assert_eq!(
            scan.nodes,
            vec![
                node(
                    "src",
                    true,
                    vec![
                        node("src/empty", true, vec![]),
                        node("src/main.rs", false, vec![])
                    ]
                ),
                node(".env", false, vec![]),
            ]
        );
    }
}
