//! Git data model and pure parsers for the CLI output formats used by redcode.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use gpui::SharedString;
use smallvec::SmallVec;

pub const LOG_ARGS: &[&str] = &[
    "log",
    "--all",
    "--date-order",
    "-z",
    "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%s",
];
pub const REFS_ARGS: &[&str] = &[
    "for-each-ref",
    "--format=%(objectname)%1f%(*objectname)%1f%(refname)%1f%(upstream:short)%1f%(upstream:track)%1f%(HEAD)",
];
pub const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v2",
    "--branch",
    "-z",
    "--untracked-files=all",
];
pub const STASH_ARGS: &[&str] = &["stash", "list", "-z", "--format=%gd%x1f%H%x1f%gs%x1f%ct"];

const FIELD_SEP: char = '\x1f';

pub type Sha = Arc<str>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: Sha,
    pub parents: SmallVec<[Sha; 2]>,
    pub author: SharedString,
    pub email: SharedString,
    pub time: i64,
    pub subject: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRef {
    pub kind: RefKind,
    /// "main", "origin/main", "v1.0"
    pub name: SharedString,
    /// For annotated tags this is the peeled commit.
    pub target: Sha,
    pub upstream: Option<SharedString>,
    pub is_head: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    /// `None` when HEAD is detached.
    pub branch: Option<SharedString>,
    /// `None` on an unborn branch.
    pub oid: Option<Sha>,
    pub upstream: Option<SharedString>,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl FileState {
    pub fn letter(self) -> &'static str {
        match self {
            FileState::Added => "A",
            FileState::Modified => "M",
            FileState::Deleted => "D",
            FileState::Renamed => "R",
            FileState::Untracked => "?",
            FileState::Conflicted => "U",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// Relative to the repository root.
    pub path: Arc<Path>,
    pub staged: Option<FileState>,
    pub unstaged: Option<FileState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub id: Sha,
    pub message: SharedString,
    pub time: i64,
}

/// Parses [`LOG_ARGS`] output: NUL-separated records, 0x1f-separated fields.
pub fn parse_log(bytes: &[u8]) -> Vec<Commit> {
    let mut commits = Vec::new();
    for record in bytes.split(|&b| b == 0) {
        let record = String::from_utf8_lossy(record);
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(6, FIELD_SEP);
        let id = fields.next().unwrap_or_default().trim();
        if id.is_empty() {
            continue;
        }
        let parents = fields
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .map(Sha::from)
            .collect();
        let author = fields.next().unwrap_or_default();
        let email = fields.next().unwrap_or_default();
        let time = fields
            .next()
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let subject = fields.next().unwrap_or_default();
        commits.push(Commit {
            id: Sha::from(id),
            parents,
            author: SharedString::new(author),
            email: SharedString::new(email),
            time,
            subject: SharedString::new(subject),
        });
    }
    commits
}

/// Parses [`REFS_ARGS`] output. Keeps only `refs/heads/`, `refs/remotes/`
/// (except `<remote>/HEAD`) and `refs/tags/`.
pub fn parse_refs(bytes: &[u8]) -> Vec<GitRef> {
    let text = String::from_utf8_lossy(bytes);
    let mut refs = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(FIELD_SEP).collect();
        if fields.len() < 3 {
            continue;
        }
        let object = fields[0].trim();
        let peeled = fields[1].trim();
        let refname = fields[2];
        let upstream = fields.get(3).map(|s| s.trim()).unwrap_or_default();
        let head = fields.get(5).map(|s| s.trim()).unwrap_or_default();

        let (kind, name) = if let Some(name) = refname.strip_prefix("refs/heads/") {
            (RefKind::LocalBranch, name)
        } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
            if name.ends_with("/HEAD") {
                continue;
            }
            (RefKind::RemoteBranch, name)
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            (RefKind::Tag, name)
        } else {
            continue;
        };
        let target = if peeled.is_empty() { object } else { peeled };
        if name.is_empty() || target.is_empty() {
            continue;
        }
        refs.push(GitRef {
            kind,
            name: SharedString::new(name),
            target: Sha::from(target),
            upstream: (!upstream.is_empty()).then(|| SharedString::new(upstream)),
            is_head: head == "*",
        });
    }
    refs
}

/// Parses [`STATUS_ARGS`] output (porcelain v2, `--branch`, `-z`).
pub fn parse_status(bytes: &[u8]) -> (Head, Vec<StatusEntry>) {
    let mut head = Head::default();
    let mut entries = Vec::new();
    let mut tokens = bytes.split(|&b| b == 0);
    while let Some(token) = tokens.next() {
        let Some(&kind) = token.first() else {
            continue;
        };
        match kind {
            b'#' => {
                let header = String::from_utf8_lossy(token);
                if let Some(oid) = header.strip_prefix("# branch.oid ") {
                    head.oid = (oid != "(initial)").then(|| Sha::from(oid));
                } else if let Some(branch) = header.strip_prefix("# branch.head ") {
                    head.branch = (branch != "(detached)").then(|| SharedString::new(branch));
                } else if let Some(upstream) = header.strip_prefix("# branch.upstream ") {
                    head.upstream = Some(SharedString::new(upstream));
                } else if let Some(ab) = header.strip_prefix("# branch.ab ") {
                    for part in ab.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            head.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            head.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
            }
            b'1' | b'2' => {
                let fields = if kind == b'1' { 8 } else { 9 };
                let Some(path) = after_fields(token, fields) else {
                    continue;
                };
                if kind == b'2' {
                    // The original path of a rename/copy is the next NUL token.
                    tokens.next();
                }
                entries.push(StatusEntry {
                    path: path_from_bytes(path),
                    staged: token.get(2).copied().and_then(status_letter),
                    unstaged: token.get(3).copied().and_then(status_letter),
                });
            }
            b'u' => {
                let Some(path) = after_fields(token, 10) else {
                    continue;
                };
                entries.push(StatusEntry {
                    path: path_from_bytes(path),
                    staged: Some(FileState::Conflicted),
                    unstaged: Some(FileState::Conflicted),
                });
            }
            b'?' => {
                let Some(path) = token.strip_prefix(b"? ") else {
                    continue;
                };
                entries.push(StatusEntry {
                    path: path_from_bytes(path),
                    staged: None,
                    unstaged: Some(FileState::Untracked),
                });
            }
            _ => {}
        }
    }
    (head, entries)
}

/// Parses [`STASH_ARGS`] output: NUL-separated records of `stash@{i}`, SHA,
/// reflog subject and commit time.
pub fn parse_stashes(bytes: &[u8]) -> Vec<Stash> {
    let mut stashes = Vec::new();
    for record in bytes.split(|&b| b == 0) {
        let record = String::from_utf8_lossy(record);
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(4, FIELD_SEP);
        let selector = fields.next().unwrap_or_default();
        let id = fields.next().unwrap_or_default().trim();
        let message = fields.next().unwrap_or_default();
        let time = fields
            .next()
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let Some(index) = selector
            .split_once('{')
            .and_then(|(_, rest)| rest.strip_suffix('}'))
            .and_then(|n| n.parse().ok())
        else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        stashes.push(Stash {
            index,
            id: Sha::from(id),
            message: SharedString::new(message),
            time,
        });
    }
    stashes
}

/// Parses `git show --name-status -z` output: `STATUS\0PATH\0`, or
/// `R<score>\0OLD\0NEW\0` / `C<score>\0OLD\0NEW\0` (reported with the new path).
pub fn parse_name_status(bytes: &[u8]) -> Vec<(FileState, Arc<Path>)> {
    let mut changes = Vec::new();
    let mut tokens = bytes.split(|&b| b == 0);
    while let Some(token) = tokens.next() {
        let status = trim_newlines(token);
        let Some(&letter) = status.first() else {
            continue;
        };
        let state = match letter {
            b'A' => FileState::Added,
            b'D' => FileState::Deleted,
            b'R' | b'C' => FileState::Renamed,
            b'U' => FileState::Conflicted,
            _ => FileState::Modified,
        };
        let Some(mut path) = tokens.next() else {
            break;
        };
        if matches!(letter, b'R' | b'C') {
            let Some(new_path) = tokens.next() else {
                break;
            };
            path = new_path;
        }
        if path.is_empty() {
            continue;
        }
        changes.push((state, path_from_bytes(path)));
    }
    changes
}

fn status_letter(b: u8) -> Option<FileState> {
    match b {
        b'.' => None,
        b'A' => Some(FileState::Added),
        b'D' => Some(FileState::Deleted),
        b'R' | b'C' => Some(FileState::Renamed),
        b'U' => Some(FileState::Conflicted),
        _ => Some(FileState::Modified),
    }
}

/// Returns what follows the first `n` space-separated fields of `token`.
fn after_fields(token: &[u8], n: usize) -> Option<&[u8]> {
    let mut rest = token;
    for _ in 0..n {
        let space = rest.iter().position(|&b| b == b' ')?;
        rest = &rest[space + 1..];
    }
    (!rest.is_empty()).then_some(rest)
}

fn trim_newlines(mut bytes: &[u8]) -> &[u8] {
    while let [b'\n' | b'\r', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b'\n' | b'\r'] = bytes {
        bytes = rest;
    }
    bytes
}

fn path_from_bytes(bytes: &[u8]) -> Arc<Path> {
    Arc::from(Path::new(OsStr::from_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nul(parts: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for part in parts {
            out.extend_from_slice(part.as_bytes());
            out.push(0);
        }
        out
    }

    fn path(p: &str) -> Arc<Path> {
        Arc::from(Path::new(p))
    }

    #[test]
    fn log_parents_zero_one_two() {
        let input = nul(&[
            "m1\x1fa1 b1\x1fAlice\x1falice@x.dev\x1f1700000300\x1fMerge branch 'feature'",
            "b1\x1fr1\x1fBob\x1fbob@x.dev\x1f1700000200\x1ffeature work",
            "a1\x1fr1\x1fAlice\x1falice@x.dev\x1f1700000100\x1fsubject \x1f with separator",
            "r1\x1f\x1fAlice\x1falice@x.dev\x1f1700000000\x1finit",
        ]);
        let commits = parse_log(&input);
        assert_eq!(commits.len(), 4);

        assert_eq!(&*commits[0].id, "m1");
        assert_eq!(
            commits[0].parents.as_slice(),
            &[Sha::from("a1"), Sha::from("b1")]
        );
        assert_eq!(commits[0].author, "Alice");
        assert_eq!(commits[0].email, "alice@x.dev");
        assert_eq!(commits[0].time, 1700000300);
        assert_eq!(commits[0].subject, "Merge branch 'feature'");

        assert_eq!(commits[1].parents.as_slice(), &[Sha::from("r1")]);
        assert_eq!(commits[2].subject, "subject \x1f with separator");
        assert!(commits[3].parents.is_empty());
        assert_eq!(commits[3].subject, "init");
    }

    #[test]
    fn log_empty_input() {
        assert!(parse_log(b"").is_empty());
    }

    #[test]
    fn refs_kinds_peel_head_and_filters() {
        let input = concat!(
            "aaa\x1f\x1frefs/heads/main\x1forigin/main\x1f[ahead 1]\x1f*\n",
            "bbb\x1f\x1frefs/heads/feature\x1f\x1f\x1f \n",
            "ccc\x1f\x1frefs/remotes/origin/HEAD\x1f\x1f\x1f \n",
            "ccc\x1f\x1frefs/remotes/origin/main\x1f\x1f\x1f \n",
            "tagobj\x1fddd\x1frefs/tags/v1.0\x1f\x1f\x1f \n",
            "eee\x1f\x1frefs/tags/light\x1f\x1f\x1f \n",
            "fff\x1f\x1frefs/stash\x1f\x1f\x1f \n",
            "ggg\x1f\x1frefs/notes/commits\x1f\x1f\x1f \n",
        );
        let refs = parse_refs(input.as_bytes());
        assert_eq!(
            refs,
            vec![
                GitRef {
                    kind: RefKind::LocalBranch,
                    name: "main".into(),
                    target: Sha::from("aaa"),
                    upstream: Some("origin/main".into()),
                    is_head: true,
                },
                GitRef {
                    kind: RefKind::LocalBranch,
                    name: "feature".into(),
                    target: Sha::from("bbb"),
                    upstream: None,
                    is_head: false,
                },
                GitRef {
                    kind: RefKind::RemoteBranch,
                    name: "origin/main".into(),
                    target: Sha::from("ccc"),
                    upstream: None,
                    is_head: false,
                },
                GitRef {
                    kind: RefKind::Tag,
                    name: "v1.0".into(),
                    target: Sha::from("ddd"),
                    upstream: None,
                    is_head: false,
                },
                GitRef {
                    kind: RefKind::Tag,
                    name: "light".into(),
                    target: Sha::from("eee"),
                    upstream: None,
                    is_head: false,
                },
            ]
        );
    }

    #[test]
    fn status_initial_branch() {
        let (head, entries) = parse_status(&nul(&[
            "# branch.oid (initial)",
            "# branch.head main",
            "? new file.txt",
        ]));
        assert_eq!(
            head,
            Head {
                branch: Some("main".into()),
                oid: None,
                upstream: None,
                ahead: 0,
                behind: 0
            }
        );
        assert_eq!(
            entries,
            vec![StatusEntry {
                path: path("new file.txt"),
                staged: None,
                unstaged: Some(FileState::Untracked)
            }]
        );
    }

    #[test]
    fn status_detached() {
        let (head, entries) =
            parse_status(&nul(&["# branch.oid 0123abc", "# branch.head (detached)"]));
        assert_eq!(head.branch, None);
        assert_eq!(head.oid, Some(Sha::from("0123abc")));
        assert!(entries.is_empty());
    }

    #[test]
    fn status_entries() {
        let input = nul(&[
            "# branch.oid 0123abc",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 M. N... 100644 100644 100644 h1 h2 a.txt",
            "1 .M N... 100644 100644 100644 h1 h1 dir/b c.txt",
            "2 R. N... 100644 100644 100644 h1 h1 R100 new.txt",
            "old.txt",
            "u UU N... 100644 100644 100644 100644 h1 h2 h3 conf.txt",
            "? x",
            "! ignored.log",
        ]);
        let (head, entries) = parse_status(&input);
        assert_eq!(
            head,
            Head {
                branch: Some("main".into()),
                oid: Some(Sha::from("0123abc")),
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
            }
        );
        assert_eq!(
            entries,
            vec![
                StatusEntry {
                    path: path("a.txt"),
                    staged: Some(FileState::Modified),
                    unstaged: None
                },
                StatusEntry {
                    path: path("dir/b c.txt"),
                    staged: None,
                    unstaged: Some(FileState::Modified)
                },
                StatusEntry {
                    path: path("new.txt"),
                    staged: Some(FileState::Renamed),
                    unstaged: None
                },
                StatusEntry {
                    path: path("conf.txt"),
                    staged: Some(FileState::Conflicted),
                    unstaged: Some(FileState::Conflicted),
                },
                StatusEntry {
                    path: path("x"),
                    staged: None,
                    unstaged: Some(FileState::Untracked)
                },
            ]
        );
    }

    #[test]
    fn stashes() {
        let input = nul(&[
            "stash@{0}\x1fabc\x1fWIP on main: 123 msg\x1f1700000000",
            "stash@{1}\x1fdef\x1fOn main: saved\x1f1690000000",
        ]);
        assert_eq!(
            parse_stashes(&input),
            vec![
                Stash {
                    index: 0,
                    id: Sha::from("abc"),
                    message: "WIP on main: 123 msg".into(),
                    time: 1700000000
                },
                Stash {
                    index: 1,
                    id: Sha::from("def"),
                    message: "On main: saved".into(),
                    time: 1690000000
                },
            ]
        );
        assert!(parse_stashes(b"").is_empty());
    }

    #[test]
    fn name_status() {
        let mut input = b"\n".to_vec();
        input.extend(nul(&[
            "M", "a.txt", "A", "b.txt", "D", "c.txt", "R100", "old.txt", "new.txt", "C075",
            "src.txt", "copy.txt", "T", "link",
        ]));
        assert_eq!(
            parse_name_status(&input),
            vec![
                (FileState::Modified, path("a.txt")),
                (FileState::Added, path("b.txt")),
                (FileState::Deleted, path("c.txt")),
                (FileState::Renamed, path("new.txt")),
                (FileState::Renamed, path("copy.txt")),
                (FileState::Modified, path("link")),
            ]
        );
    }

    #[test]
    fn letters() {
        let all = [
            FileState::Added,
            FileState::Modified,
            FileState::Deleted,
            FileState::Renamed,
            FileState::Untracked,
            FileState::Conflicted,
        ];
        let letters: Vec<&str> = all.iter().map(|s| s.letter()).collect();
        assert_eq!(letters, ["A", "M", "D", "R", "?", "U"]);
    }
}
