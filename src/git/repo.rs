//! `GitRepo`: the workspace's view of its Git repository, refreshed from the `git` CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;
use gpui::{
    AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Task, WeakEntity,
};

use super::cli::{self, GitOutput};
use super::graph::{GraphLayout, GraphRow};
use super::model::{
    self, Commit, GitRef, Head, LOG_ARGS, REFS_ARGS, RefKind, STASH_ARGS, STATUS_ARGS, Sha, Stash,
    StatusEntry,
};
use crate::fs_watch::{FsChanged, FsWatcher};

const MERGE_HEAD_ARGS: &[&str] = &["rev-parse", "-q", "--verify", "MERGE_HEAD"];
const BUSY_ERROR: &str = "다른 Git 작업이 실행 중입니다";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepoState {
    Detecting,
    NotRepo,
    Repo,
}

pub enum GitEvent {
    Refreshed,
    OperationFailed(SharedString),
}

pub struct GitRepo {
    /// The repository's top-level directory once detected (the workspace root until then).
    pub root: PathBuf,
    pub head: Head,
    pub status: Vec<StatusEntry>,
    pub refs: Vec<GitRef>,
    pub stashes: Vec<Stash>,
    /// `git rev-parse -q --verify MERGE_HEAD` exited with 0.
    pub merging: bool,
    pub commits: Vec<Commit>,
    pub rows: Vec<GraphRow>,
    pub graph_width: u16,
    /// Label of the running mutation ("Pushing…").
    pub busy: Option<SharedString>,
    workspace_root: PathBuf,
    state: RepoState,
    detect_task: Option<Task<()>>,
    status_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    _fs_sub: gpui::Subscription,
}

impl EventEmitter<GitEvent> for GitRepo {}

struct StatusSnapshot {
    head: Head,
    status: Vec<StatusEntry>,
    merging: bool,
}

struct FullSnapshot {
    status: StatusSnapshot,
    refs: Vec<GitRef>,
    stashes: Vec<Stash>,
    commits: Vec<Commit>,
    rows: Vec<GraphRow>,
    graph_width: u16,
}

async fn load_status(root: &Path) -> anyhow::Result<StatusSnapshot> {
    let (head, status) = model::parse_status(&cli::run_ok(root, STATUS_ARGS, None).await?);
    let merging = cli::run(root, MERGE_HEAD_ARGS, None).await?.code == 0;
    Ok(StatusSnapshot {
        head,
        status,
        merging,
    })
}

async fn load_all(root: &Path) -> anyhow::Result<FullSnapshot> {
    let status = load_status(root).await?;
    let refs = model::parse_refs(&cli::run_ok(root, REFS_ARGS, None).await?);
    let stashes = model::parse_stashes(&cli::run_ok(root, STASH_ARGS, None).await?);
    let commits = model::parse_log(&cli::run_ok(root, LOG_ARGS, None).await?);
    let mut layout = GraphLayout::new();
    let rows = commits
        .iter()
        .map(|commit| layout.push(&commit.id, &commit.parents))
        .collect();
    Ok(FullSnapshot {
        status,
        refs,
        stashes,
        commits,
        rows,
        graph_width: layout.max_width(),
    })
}

/// The repository's top level for `workspace_root`. When the workspace root itself is the top
/// level, it is kept in the form it was given so paths match the rest of the workspace.
async fn detect_root(workspace_root: &Path) -> anyhow::Result<PathBuf> {
    let out = cli::run_ok(workspace_root, &["rev-parse", "--show-toplevel"], None).await?;
    let toplevel = PathBuf::from(String::from_utf8_lossy(&out).trim_end_matches('\n'));
    let canonical = std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.into());
    Ok(if canonical == toplevel {
        workspace_root.to_path_buf()
    } else {
        toplevel
    })
}

fn path_args(paths: &[Arc<Path>]) -> impl Iterator<Item = String> + '_ {
    paths.iter().map(|path| path.to_string_lossy().into_owned())
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}

impl GitRepo {
    pub fn new(workspace_root: PathBuf, fs: Entity<FsWatcher>, cx: &mut Context<Self>) -> Self {
        let fs_sub = cx.subscribe(&fs, |repo, _, event: &FsChanged, cx| {
            repo.on_fs_changed(event, cx)
        });
        let mut this = Self {
            root: workspace_root.clone(),
            head: Head::default(),
            status: Vec::new(),
            refs: Vec::new(),
            stashes: Vec::new(),
            merging: false,
            commits: Vec::new(),
            rows: Vec::new(),
            graph_width: 0,
            busy: None,
            workspace_root,
            state: RepoState::Detecting,
            detect_task: None,
            status_task: None,
            refresh_task: None,
            _fs_sub: fs_sub,
        };
        this.detect(cx);
        this
    }

    pub fn is_repo(&self) -> bool {
        self.state == RepoState::Repo
    }

    /// Repository detection has finished and found no repository.
    pub fn is_not_repo(&self) -> bool {
        self.state == RepoState::NotRepo
    }

    fn detect(&mut self, cx: &mut Context<Self>) {
        let workspace_root = self.workspace_root.clone();
        self.detect_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let root = cx
                    .background_spawn(async move { detect_root(&workspace_root).await })
                    .await;
                this.update(cx, |repo, cx| {
                    repo.detect_task = None;
                    match root {
                        Ok(root) => {
                            repo.root = root;
                            repo.state = RepoState::Repo;
                            repo.refresh_all(cx);
                        }
                        Err(_) => repo.set_not_repo(cx),
                    }
                })
                .ok();
            },
        ));
    }

    fn set_not_repo(&mut self, cx: &mut Context<Self>) {
        self.state = RepoState::NotRepo;
        self.root = self.workspace_root.clone();
        self.head = Head::default();
        self.status.clear();
        self.refs.clear();
        self.stashes.clear();
        self.merging = false;
        self.commits.clear();
        self.rows.clear();
        self.graph_width = 0;
        self.status_task = None;
        self.refresh_task = None;
        cx.emit(GitEvent::Refreshed);
        cx.notify();
    }

    fn on_fs_changed(&mut self, event: &FsChanged, cx: &mut Context<Self>) {
        let git_dir = self.workspace_root.join(".git");
        let mut inside = false;
        let mut outside = false;
        for path in event.0.iter() {
            if path.starts_with(&git_dir) {
                inside = true;
            } else {
                outside = true;
            }
        }
        match self.state {
            RepoState::Detecting => {}
            RepoState::NotRepo => {
                if inside {
                    self.detect(cx);
                }
            }
            RepoState::Repo => {
                if inside {
                    self.refresh_all(cx);
                } else if outside {
                    self.refresh_status(cx);
                }
            }
        }
    }

    /// Re-reads the working tree status and the merge state.
    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        if !self.is_repo() {
            return;
        }
        let root = self.root.clone();
        self.status_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let snapshot = cx
                    .background_spawn(async move { load_status(&root).await })
                    .await;
                this.update(cx, |repo, cx| {
                    repo.status_task = None;
                    match snapshot {
                        Ok(snapshot) => {
                            repo.apply_status(snapshot);
                            cx.emit(GitEvent::Refreshed);
                            cx.notify();
                        }
                        Err(err) => repo.on_load_error(err, cx),
                    }
                })
                .ok();
            },
        ));
    }

    /// Re-reads status, refs, stashes and the whole log, replacing everything at once. A refresh
    /// already in flight is cancelled.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        if !self.is_repo() {
            return;
        }
        let root = self.root.clone();
        self.status_task = None;
        self.refresh_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let snapshot = cx
                    .background_spawn(async move { load_all(&root).await })
                    .await;
                this.update(cx, |repo, cx| {
                    repo.refresh_task = None;
                    match snapshot {
                        Ok(snapshot) => repo.apply_all(snapshot, cx),
                        Err(err) => repo.on_load_error(err, cx),
                    }
                })
                .ok();
            },
        ));
    }

    fn apply_status(&mut self, snapshot: StatusSnapshot) {
        self.head = snapshot.head;
        self.status = snapshot.status;
        self.merging = snapshot.merging;
    }

    fn apply_all(&mut self, snapshot: FullSnapshot, cx: &mut Context<Self>) {
        self.status_task = None;
        self.refresh_task = None;
        self.apply_status(snapshot.status);
        self.refs = snapshot.refs;
        self.stashes = snapshot.stashes;
        self.commits = snapshot.commits;
        self.rows = snapshot.rows;
        self.graph_width = snapshot.graph_width;
        cx.emit(GitEvent::Refreshed);
        cx.notify();
    }

    /// A failed read usually means the repository went away; otherwise it is reported.
    fn on_load_error(&mut self, err: anyhow::Error, cx: &mut Context<Self>) {
        if self.root.join(".git").exists() || self.workspace_root.join(".git").exists() {
            cx.emit(GitEvent::OperationFailed(err.to_string().into()));
        } else {
            self.detect(cx);
        }
    }

    /// Runs a mutating git command. While it runs `busy` holds `label`; afterwards everything is
    /// refreshed before the task resolves, so callers observe the post-command state.
    fn mutate(
        &mut self,
        label: &'static str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if self.busy.is_some() {
            return Task::ready(Err(anyhow!(BUSY_ERROR)));
        }
        self.busy = Some(label.into());
        cx.notify();
        let root = self.root.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let (result, snapshot) = cx
                .background_spawn(async move {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    let result = cli::run_ok(&root, &args, stdin.as_deref())
                        .await
                        .map(|_| ());
                    (result, load_all(&root).await)
                })
                .await;
            this.update(cx, |repo, cx| {
                repo.busy = None;
                match snapshot {
                    Ok(snapshot) => repo.apply_all(snapshot, cx),
                    Err(err) => {
                        cx.notify();
                        repo.on_load_error(err, cx);
                    }
                }
            })?;
            result
        })
    }

    /// `mutate` whose failure is reported as `GitEvent::OperationFailed`.
    fn run_op(
        &mut self,
        label: &'static str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let task = self.mutate(label, args, stdin, cx);
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = task.await;
            if let Err(err) = &result {
                let message: SharedString = err.to_string().into();
                this.update(cx, |_, cx| cx.emit(GitEvent::OperationFailed(message)))
                    .ok();
            }
            result
        })
    }

    fn fire(&mut self, label: &'static str, args: Vec<String>, cx: &mut Context<Self>) {
        self.run_op(label, args, None, cx).detach();
    }

    fn has_head(&self) -> bool {
        self.head.oid.is_some()
    }

    pub fn stage(&mut self, paths: Vec<Arc<Path>>, cx: &mut Context<Self>) {
        let args = args(&["add", "-A", "--"])
            .into_iter()
            .chain(path_args(&paths))
            .collect();
        self.fire("Staging…", args, cx);
    }

    pub fn unstage(&mut self, paths: Vec<Arc<Path>>, cx: &mut Context<Self>) {
        let base: &[&str] = if self.has_head() {
            &["restore", "--staged", "--"]
        } else {
            &["rm", "--cached", "-q", "--"]
        };
        let args = args(base).into_iter().chain(path_args(&paths)).collect();
        self.fire("Unstaging…", args, cx);
    }

    pub fn stage_all(&mut self, cx: &mut Context<Self>) {
        self.fire("Staging…", args(&["add", "-A"]), cx);
    }

    pub fn unstage_all(&mut self, cx: &mut Context<Self>) {
        let args = if self.has_head() {
            args(&["reset", "-q"])
        } else {
            args(&["rm", "-r", "--cached", "-q", "--", "."])
        };
        self.fire("Unstaging…", args, cx);
    }

    /// Commits with `message` on stdin. While merging, an empty message concludes the merge with
    /// git's prepared message. The task resolves after the refresh; failures are also reported.
    pub fn commit(
        &mut self,
        message: String,
        amend: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let (mut args, stdin) = if self.merging && message.trim().is_empty() {
            (args(&["commit", "--no-edit"]), None)
        } else {
            (args(&["commit", "-F", "-"]), Some(message.into_bytes()))
        };
        if amend {
            args.push("--amend".into());
        }
        self.run_op("Committing…", args, stdin, cx)
    }

    pub fn create_branch(&mut self, name: String, start: Option<Sha>, cx: &mut Context<Self>) {
        let mut args = args(&["switch", "-c"]);
        args.push(name);
        args.extend(start.map(|sha| sha.to_string()));
        self.fire("Creating branch…", args, cx);
    }

    /// Local branch: `switch`; remote branch: `switch --track` (or the same-named local branch);
    /// tag: detached at its commit.
    pub fn checkout(&mut self, git_ref: &GitRef, cx: &mut Context<Self>) {
        let args = match git_ref.kind {
            RefKind::LocalBranch => vec!["switch".into(), git_ref.name.to_string()],
            RefKind::RemoteBranch => {
                let local = git_ref
                    .name
                    .split_once('/')
                    .map_or(git_ref.name.as_ref(), |(_, branch)| branch);
                let exists = self
                    .refs
                    .iter()
                    .any(|r| r.kind == RefKind::LocalBranch && r.name.as_ref() == local);
                if exists {
                    vec!["switch".into(), local.to_string()]
                } else {
                    vec!["switch".into(), "--track".into(), git_ref.name.to_string()]
                }
            }
            RefKind::Tag => vec![
                "switch".into(),
                "--detach".into(),
                git_ref.target.to_string(),
            ],
        };
        self.fire("Checking out…", args, cx);
    }

    /// Detached checkout of a commit.
    pub fn checkout_detached(&mut self, sha: Sha, cx: &mut Context<Self>) {
        let args = vec!["switch".into(), "--detach".into(), sha.to_string()];
        self.fire("Checking out…", args, cx);
    }

    /// Resolves with git's error; the caller decides whether to offer a forced delete. Failures of
    /// a forced delete are also reported as `OperationFailed`.
    pub fn delete_branch(
        &mut self,
        name: String,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let args = vec![
            "branch".into(),
            if force { "-D" } else { "-d" }.into(),
            name,
        ];
        if force {
            self.run_op("Deleting branch…", args, None, cx)
        } else {
            self.mutate("Deleting branch…", args, None, cx)
        }
    }

    /// A merge that stops with conflicts is not reported as an error: the changes panel shows the
    /// conflict banner instead.
    pub fn merge(&mut self, name: String, cx: &mut Context<Self>) {
        let task = self.mutate(
            "Merging…",
            vec!["merge".into(), "--no-edit".into(), name],
            None,
            cx,
        );
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Err(err) = task.await {
                this.update(cx, |repo, cx| {
                    if !repo.merging {
                        cx.emit(GitEvent::OperationFailed(err.to_string().into()));
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn abort_merge(&mut self, cx: &mut Context<Self>) {
        self.fire("Aborting merge…", args(&["merge", "--abort"]), cx);
    }

    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        self.fire("Fetching…", args(&["fetch", "--all", "--prune"]), cx);
    }

    pub fn pull(&mut self, cx: &mut Context<Self>) {
        self.fire("Pulling…", args(&["pull"]), cx);
    }

    /// Pushes to the upstream, or sets one up on `origin` (else the first remote).
    pub fn push(&mut self, cx: &mut Context<Self>) {
        if self.head.upstream.is_some() {
            self.fire("Pushing…", args(&["push"]), cx);
            return;
        }
        if self.busy.is_some() {
            cx.emit(GitEvent::OperationFailed(BUSY_ERROR.into()));
            return;
        }
        let Some(branch) = self.head.branch.clone() else {
            cx.emit(GitEvent::OperationFailed("현재 브랜치가 없습니다".into()));
            return;
        };
        let root = self.root.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let remotes = cx
                .background_spawn(async move { cli::run_ok(&root, &["remote"], None).await })
                .await;
            this.update(cx, |repo, cx| {
                let remote = remotes.map(|out| pick_remote(&String::from_utf8_lossy(&out)));
                match remote {
                    Ok(Some(remote)) => {
                        let args = vec!["push".into(), "-u".into(), remote, branch.to_string()];
                        repo.fire("Pushing…", args, cx);
                    }
                    Ok(None) => cx.emit(GitEvent::OperationFailed("원격 저장소가 없습니다".into())),
                    Err(err) => cx.emit(GitEvent::OperationFailed(err.to_string().into())),
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn stash_push(
        &mut self,
        message: Option<String>,
        include_untracked: bool,
        cx: &mut Context<Self>,
    ) {
        let mut args = args(&["stash", "push"]);
        if include_untracked {
            args.push("-u".into());
        }
        if let Some(message) = message {
            args.push("-m".into());
            args.push(message);
        }
        self.fire("Stashing…", args, cx);
    }

    pub fn stash_apply(&mut self, index: usize, cx: &mut Context<Self>) {
        self.fire("Applying stash…", stash_args("apply", index), cx);
    }

    pub fn stash_pop(&mut self, index: usize, cx: &mut Context<Self>) {
        self.fire("Popping stash…", stash_args("pop", index), cx);
    }

    pub fn stash_drop(&mut self, index: usize, cx: &mut Context<Self>) {
        self.fire("Dropping stash…", stash_args("drop", index), cx);
    }

    /// `git init` in the workspace root, then detects the new repository.
    pub fn init_repository(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            cx.emit(GitEvent::OperationFailed(BUSY_ERROR.into()));
            return;
        }
        self.busy = Some("Initializing…".into());
        cx.notify();
        let root = self.workspace_root.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move { cli::run_ok(&root, &["init"], None).await })
                .await;
            this.update(cx, |repo, cx| {
                repo.busy = None;
                match result {
                    Ok(_) => repo.detect(cx),
                    Err(err) => {
                        cx.notify();
                        cx.emit(GitEvent::OperationFailed(err.to_string().into()));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs a read-only git command in the repository root; non-zero exit codes are errors.
    pub fn read(&self, args: Vec<String>, cx: &mut Context<Self>) -> Task<anyhow::Result<Vec<u8>>> {
        let root = self.root.clone();
        cx.background_spawn(async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            cli::run_ok(&root, &args, None).await
        })
    }

    /// Like [`Self::read`], but returns the raw output whatever the exit code.
    pub fn read_raw(
        &self,
        args: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<GitOutput>> {
        let root = self.root.clone();
        cx.background_spawn(async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            cli::run(&root, &args, None).await
        })
    }
}

fn stash_args(verb: &str, index: usize) -> Vec<String> {
    vec!["stash".into(), verb.into(), format!("stash@{{{index}}}")]
}

/// `origin` when present, otherwise the first listed remote.
fn pick_remote(remotes: &str) -> Option<String> {
    let mut lines = remotes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.clone().next()?;
    Some(if lines.any(|line| line == "origin") {
        "origin".into()
    } else {
        first.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_choice_prefers_origin() {
        assert_eq!(pick_remote(""), None);
        assert_eq!(pick_remote("upstream\norigin\n"), Some("origin".into()));
        assert_eq!(pick_remote("upstream\nfork\n"), Some("upstream".into()));
    }

    #[test]
    fn stash_selector() {
        assert_eq!(stash_args("pop", 2), vec!["stash", "pop", "stash@{2}"]);
    }

    /// Builds the plan's verification repository and checks the parsers and lane layout on it.
    #[test]
    fn verification_repo_parses() {
        let dir =
            std::env::temp_dir().join(format!("redcode-git-repo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Distinct commit dates make `--date-order` (and so the lane layout) deterministic.
        let script = "n=0; tick() { n=$((n+1)); export GIT_COMMITTER_DATE=\"2026-01-01T00:00:0$n +0000\"; \
            export GIT_AUTHOR_DATE=\"$GIT_COMMITTER_DATE\"; }; \
            git init -q -b main \
            && git config user.name Tester && git config user.email tester@example.com \
            && echo a > a.txt && git add . && tick && git commit -qm init \
            && git switch -qc feature && echo b > b.txt && git add . && tick && git commit -qm 'feature work' \
            && git switch -q main && echo c > c.txt && git add . && tick && git commit -qm 'main work' \
            && tick && git merge -q --no-edit feature \
            && git switch -qc conflict HEAD~1 && echo x > a.txt && tick && git commit -qam 'conflict side' \
            && git switch -q main && echo y > a.txt && tick && git commit -qam 'main side'";
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(dir.join("d.txt"), "untracked").unwrap();

        let snapshot = smol::block_on(load_all(&dir));
        let root = smol::block_on(detect_root(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        let snapshot = snapshot.unwrap();
        assert_eq!(root.unwrap(), dir);

        let subjects: Vec<&str> = snapshot
            .commits
            .iter()
            .map(|c| c.subject.as_ref())
            .collect();
        assert_eq!(subjects.len(), 6);
        assert!(subjects.contains(&"init"));
        assert!(subjects.contains(&"Merge branch 'feature'"));
        let merge = snapshot
            .commits
            .iter()
            .find(|c| c.subject.as_ref() == "Merge branch 'feature'")
            .unwrap();
        assert_eq!(merge.parents.len(), 2);
        assert_eq!(snapshot.commits.last().unwrap().subject, "init");
        assert!(snapshot.commits.last().unwrap().parents.is_empty());

        let mut branches: Vec<(&str, bool)> = snapshot
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::LocalBranch)
            .map(|r| (r.name.as_ref(), r.is_head))
            .collect();
        branches.sort();
        assert_eq!(
            branches,
            vec![("conflict", false), ("feature", false), ("main", true)]
        );

        let head = &snapshot.status.head;
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert!(head.oid.is_some());
        assert!(!snapshot.status.merging);
        assert_eq!(
            snapshot.status.status,
            vec![StatusEntry {
                path: Arc::from(Path::new("d.txt")),
                staged: None,
                unstaged: Some(model::FileState::Untracked),
            }]
        );
        assert!(snapshot.stashes.is_empty());
        assert_eq!(snapshot.rows.len(), 6);
        assert_eq!(snapshot.graph_width, 2);
    }
}
