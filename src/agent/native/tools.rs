//! The six workspace tools exposed to the native Anthropic backend.

use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde_json::{Value, json};
use smol::io::AsyncReadExt;

use crate::agent::thread::{ToolContent, ToolKind};

const READ_DEFAULT_LIMIT: u64 = 2000;
const READ_MAX_BYTES: usize = 256 * 1024;
const LIST_MAX: usize = 500;
const SEARCH_MAX: usize = 200;
const SEARCH_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const COMMAND_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const COMMAND_MAX_TIMEOUT_MS: u64 = 600_000;
const COMMAND_OUTPUT_TAIL: usize = 30_000;
/// How long to keep reading output after the shell exits (background jobs may hold the pipe).
const COMMAND_OUTPUT_GRACE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolPreview {
    pub title: String,
    pub kind: ToolKind,
    pub content: Vec<ToolContent>,
    pub locations: Vec<(PathBuf, Option<u32>)>,
}

/// Tool definitions for the Messages API `tools` field.
pub fn schemas() -> Value {
    json!([
        {
            "name": "read_file",
            "description": "Read a UTF-8 text file in the workspace. Returns up to `limit` lines starting at line `offset` (1-based). Output larger than 256KB is truncated.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root or absolute." },
                    "offset": { "type": "integer", "minimum": 1, "description": "1-based line number to start reading from. Default 1." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of lines to return. Default 2000." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "list_files",
            "description": "List workspace files matching a glob pattern (e.g. `src/**/*.rs`; a pattern without `/` matches file names anywhere). Respects .gitignore. Returns at most 500 paths relative to the workspace root.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern." }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "search",
            "description": "Search workspace file contents with a regular expression. Returns matching lines as `path:line: text`, at most 200 matches. Binary files and files over 2MB are skipped.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression (Rust regex syntax)." },
                    "glob": { "type": "string", "description": "Optional glob restricting which files are searched." }
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content. Parent directories are created as needed. Prefer edit_file for existing files.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root or absolute." },
                    "content": { "type": "string", "description": "Full file content." }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "edit_file",
            "description": "Replace an exact string in a file. `old_string` must occur exactly once unless `replace_all` is true.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root or absolute." },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence. Default false." }
                },
                "required": ["path", "old_string", "new_string"]
            }
        },
        {
            "name": "run_command",
            "description": "Run a shell command with `/bin/zsh -lc` in the workspace root. Returns the exit code and the last 30000 bytes of combined stdout and stderr.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command line." },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 600000, "description": "Timeout in milliseconds. Default 120000, maximum 600000." }
                },
                "required": ["command"]
            }
        }
    ])
}

/// Whether the tool modifies the workspace or runs commands and therefore needs user approval.
pub fn requires_approval(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "run_command")
}

/// Resolves `path` (relative to `root`, or absolute) lexically and rejects anything outside `root`.
pub fn resolve_path(root: &Path, path: &str) -> Result<PathBuf, ToolResult> {
    let root = normalize(root);
    let candidate = Path::new(path);
    let resolved = if candidate.is_absolute() {
        normalize(candidate)
    } else {
        normalize(&root.join(candidate))
    };
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(ToolResult::err("path is outside the workspace"))
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

/// Card title, kind, diff content, and locations shown for a tool call before it runs.
pub fn preview(root: &Path, name: &str, input: &Value) -> ToolPreview {
    let field = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let mut preview = ToolPreview {
        title: name.to_string(),
        kind: ToolKind::Other,
        content: Vec::new(),
        locations: Vec::new(),
    };
    match name {
        "read_file" => {
            let path = field("path");
            preview.title = format!("Read {path}");
            preview.kind = ToolKind::Read;
            if let Ok(resolved) = resolve_path(root, path) {
                let line = input
                    .get("offset")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok());
                preview.locations.push((resolved, line));
            }
        }
        "list_files" => {
            preview.title = format!("List {}", field("pattern"));
            preview.kind = ToolKind::Search;
        }
        "search" => {
            preview.title = format!("Search {}", field("pattern"));
            preview.kind = ToolKind::Search;
        }
        "write_file" => {
            let path = field("path");
            preview.title = format!("Write {path}");
            preview.kind = ToolKind::Edit;
            if let Ok(resolved) = resolve_path(root, path)
                && let Some(new) = input.get("content").and_then(Value::as_str)
            {
                let old = std::fs::read_to_string(&resolved).ok();
                preview.content.push(ToolContent::Diff {
                    path: resolved.clone(),
                    old,
                    new: new.to_string(),
                });
                preview.locations.push((resolved, None));
            }
        }
        "edit_file" => {
            let path = field("path");
            preview.title = format!("Edit {path}");
            preview.kind = ToolKind::Edit;
            if let Ok(resolved) = resolve_path(root, path) {
                let replace_all = input
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let old_string = input.get("old_string").and_then(Value::as_str);
                let new_string = input.get("new_string").and_then(Value::as_str);
                if let (Some(old_string), Some(new_string), Ok(current)) =
                    (old_string, new_string, std::fs::read_to_string(&resolved))
                    && let Ok((new, _)) = apply_edit(&current, old_string, new_string, replace_all)
                {
                    preview.content.push(ToolContent::Diff {
                        path: resolved.clone(),
                        old: Some(current),
                        new,
                    });
                }
                preview.locations.push((resolved, None));
            }
        }
        "run_command" => {
            preview.title = format!("$ {}", field("command"));
            preview.kind = ToolKind::Execute;
        }
        _ => {}
    }
    preview
}

/// Runs a tool. Blocking filesystem work happens inline; call this on a background executor.
pub async fn execute(root: &Path, name: &str, input: &Value) -> ToolResult {
    let result = match name {
        "read_file" => read_file(root, input),
        "list_files" => list_files(root, input),
        "search" => search(root, input),
        "write_file" => write_file(root, input),
        "edit_file" => edit_file(root, input),
        "run_command" => run_command(root, input).await,
        _ => Err(ToolResult::err(format!("unknown tool: {name}"))),
    };
    result.unwrap_or_else(|err| err)
}

type Outcome = Result<ToolResult, ToolResult>;

fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolResult> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolResult::err(format!("missing required parameter: {key}")))
}

fn display(root: &Path, path: &Path) -> String {
    let root = normalize(root);
    path.strip_prefix(&root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn read_text(root: &Path, path: &Path) -> Result<String, ToolResult> {
    let bytes = std::fs::read(path)
        .map_err(|err| ToolResult::err(format!("failed to read {}: {err}", display(root, path))))?;
    String::from_utf8(bytes)
        .map_err(|_| ToolResult::err(format!("{} is not a UTF-8 text file", display(root, path))))
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(s: &str, mut index: usize) -> usize {
    while index < s.len() && !s.is_char_boundary(index) {
        index += 1;
    }
    index.min(s.len())
}

fn read_file(root: &Path, input: &Value) -> Outcome {
    let path = resolve_path(root, required_str(input, "path")?)?;
    let text = read_text(root, &path)?;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(READ_DEFAULT_LIMIT);
    let skip = usize::try_from(offset - 1).unwrap_or(usize::MAX);
    let take = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut out: String = text.split_inclusive('\n').skip(skip).take(take).collect();
    if out.len() > READ_MAX_BYTES {
        out.truncate(floor_char_boundary(&out, READ_MAX_BYTES));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[truncated]");
    }
    Ok(ToolResult::ok(out))
}

enum GlobFilter {
    /// Pattern contains `/`: matched against the root-relative path.
    Path(GlobMatcher),
    /// Pattern without `/`: matched against the file name.
    Name(GlobMatcher),
}

impl GlobFilter {
    fn new(pattern: &str) -> Result<Self, ToolResult> {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|err| ToolResult::err(format!("invalid glob pattern: {err}")))?;
        let matcher = glob.compile_matcher();
        Ok(if pattern.contains('/') {
            Self::Path(matcher)
        } else {
            Self::Name(matcher)
        })
    }

    fn matches(&self, relative: &Path) -> bool {
        match self {
            Self::Path(matcher) => matcher.is_match(relative),
            Self::Name(matcher) => relative
                .file_name()
                .is_some_and(|name| matcher.is_match(name)),
        }
    }
}

/// Gitignore-aware walk over the workspace files, sorted by name, skipping `.git`.
fn walk_files(root: &Path) -> impl Iterator<Item = (PathBuf, ignore::DirEntry)> {
    let root = normalize(root);
    let walk = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    walk.filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|ty| ty.is_file()))
        .filter_map(move |entry| {
            let relative = entry.path().strip_prefix(&root).ok()?.to_path_buf();
            Some((relative, entry))
        })
}

fn list_files(root: &Path, input: &Value) -> Outcome {
    let filter = GlobFilter::new(required_str(input, "pattern")?)?;
    let mut paths: Vec<String> = Vec::new();
    let mut truncated = false;
    for (relative, _) in walk_files(root) {
        if !filter.matches(&relative) {
            continue;
        }
        if paths.len() == LIST_MAX {
            truncated = true;
            break;
        }
        paths.push(relative.display().to_string());
    }
    if paths.is_empty() {
        return Ok(ToolResult::ok("No files matched."));
    }
    let mut out = paths.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n[truncated: more than {LIST_MAX} files matched]"
        ));
    }
    Ok(ToolResult::ok(out))
}

fn search(root: &Path, input: &Value) -> Outcome {
    let regex = Regex::new(required_str(input, "pattern")?)
        .map_err(|err| ToolResult::err(format!("invalid regex: {err}")))?;
    let filter = match input
        .get("glob")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
    {
        Some(glob) => Some(GlobFilter::new(glob)?),
        None => None,
    };
    let mut hits: Vec<String> = Vec::new();
    let mut truncated = false;
    'files: for (relative, entry) in walk_files(root) {
        if filter
            .as_ref()
            .is_some_and(|filter| !filter.matches(&relative))
        {
            continue;
        }
        if !entry
            .metadata()
            .is_ok_and(|meta| meta.len() <= SEARCH_MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (ix, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if hits.len() == SEARCH_MAX {
                truncated = true;
                break 'files;
            }
            hits.push(format!("{}:{}: {}", relative.display(), ix + 1, line));
        }
    }
    if hits.is_empty() {
        return Ok(ToolResult::ok("No matches found."));
    }
    let mut out = hits.join("\n");
    if truncated {
        out.push_str(&format!(
            "\n[truncated: showing the first {SEARCH_MAX} matches]"
        ));
    }
    Ok(ToolResult::ok(out))
}

fn write_file(root: &Path, input: &Value) -> Outcome {
    let path = resolve_path(root, required_str(input, "path")?)?;
    let content = required_str(input, "content")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            ToolResult::err(format!("failed to create {}: {err}", display(root, parent)))
        })?;
    }
    std::fs::write(&path, content).map_err(|err| {
        ToolResult::err(format!("failed to write {}: {err}", display(root, &path)))
    })?;
    Ok(ToolResult::ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        display(root, &path)
    )))
}

/// Applies an exact-string edit, returning the new text and the number of replacements.
fn apply_edit(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize), ToolResult> {
    if old.is_empty() {
        return Err(ToolResult::err("old_string must not be empty"));
    }
    let count = text.matches(old).count();
    if count == 0 {
        return Err(ToolResult::err("old_string not found"));
    }
    if count > 1 && !replace_all {
        return Err(ToolResult::err(format!("old_string matches {count} times")));
    }
    let result = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    Ok((result, count))
}

fn edit_file(root: &Path, input: &Value) -> Outcome {
    let path = resolve_path(root, required_str(input, "path")?)?;
    let old = required_str(input, "old_string")?;
    let new = required_str(input, "new_string")?;
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = read_text(root, &path)?;
    let (result, count) = apply_edit(&text, old, new, replace_all)?;
    std::fs::write(&path, result).map_err(|err| {
        ToolResult::err(format!("failed to write {}: {err}", display(root, &path)))
    })?;
    let noun = if count == 1 {
        "replacement"
    } else {
        "replacements"
    };
    Ok(ToolResult::ok(format!(
        "Edited {} ({count} {noun})",
        display(root, &path)
    )))
}

/// Reads the pipe to EOF, keeping only a bounded tail of the output.
async fn read_tail(pipe: &mut smol::Async<std::io::PipeReader>, out: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 2 * COMMAND_OUTPUT_TAIL {
                    let excess = out.len() - COMMAND_OUTPUT_TAIL;
                    out.drain(..excess);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

async fn run_command(root: &Path, input: &Value) -> Outcome {
    let command = required_str(input, "command")?;
    let timeout_ms = input
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(COMMAND_DEFAULT_TIMEOUT_MS)
        .min(COMMAND_MAX_TIMEOUT_MS);
    let spawn_err = |err: std::io::Error| ToolResult::err(format!("failed to run command: {err}"));

    // stdout and stderr share one pipe so their output stays interleaved.
    let (reader, writer) = std::io::pipe().map_err(spawn_err)?;
    let stderr_writer = writer.try_clone().map_err(spawn_err)?;
    let mut child = {
        // The Command owns the pipe's write ends; it is dropped at the end of this block so
        // the reader sees EOF once the child exits.
        let mut cmd = smol::process::Command::new("/bin/zsh");
        cmd.arg("-lc")
            .arg(command)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stdout(writer)
            .stderr(stderr_writer)
            .kill_on_drop(true);
        cmd.spawn().map_err(spawn_err)?
    };
    let mut pipe = smol::Async::new(reader).map_err(spawn_err)?;

    let mut output: Vec<u8> = Vec::new();
    let finished = {
        let (exited_tx, exited_rx) = smol::channel::bounded::<()>(1);
        let reading = smol::future::or(read_tail(&mut pipe, &mut output), async {
            let _ = exited_rx.recv().await;
            smol::Timer::after(COMMAND_OUTPUT_GRACE).await;
        });
        let waiting = async {
            let status = child.status().await;
            let _ = exited_tx.try_send(());
            status
        };
        let work = async { Some(futures::future::join(reading, waiting).await.1) };
        let timeout = async {
            smol::Timer::after(Duration::from_millis(timeout_ms)).await;
            None
        };
        smol::future::or(work, timeout).await
    };

    let status = match finished {
        None => {
            return Err(ToolResult::err(format!(
                "command timed out after {timeout_ms} ms"
            )));
        }
        Some(Err(err)) => {
            return Err(ToolResult::err(format!(
                "failed to wait for command: {err}"
            )));
        }
        Some(Ok(status)) => status,
    };
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let text = String::from_utf8_lossy(&output);
    let start = ceil_char_boundary(&text, text.len().saturating_sub(COMMAND_OUTPUT_TAIL));
    let result = format!("exit code: {code}\n{}", &text[start..]);
    Ok(if code == 0 {
        ToolResult::ok(result)
    } else {
        ToolResult::err(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "redcode-tools-{}-{}-{nanos}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn exec(root: &Path, name: &str, input: Value) -> ToolResult {
        smol::block_on(execute(root, name, &input))
    }

    #[test]
    fn edit_file_match_counts() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("a.txt"), "foo bar foo\n").unwrap();

        let missing = exec(
            &dir.0,
            "edit_file",
            json!({"path": "a.txt", "old_string": "baz", "new_string": "x"}),
        );
        assert_eq!(missing, ToolResult::err("old_string not found"));

        let ambiguous = exec(
            &dir.0,
            "edit_file",
            json!({"path": "a.txt", "old_string": "foo", "new_string": "x"}),
        );
        assert_eq!(ambiguous, ToolResult::err("old_string matches 2 times"));
        assert_eq!(
            std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
            "foo bar foo\n"
        );

        let single = exec(
            &dir.0,
            "edit_file",
            json!({"path": "a.txt", "old_string": "bar", "new_string": "BAR"}),
        );
        assert!(!single.is_error, "{}", single.text);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
            "foo BAR foo\n"
        );

        let all = exec(
            &dir.0,
            "edit_file",
            json!({"path": "a.txt", "old_string": "foo", "new_string": "qux", "replace_all": true}),
        );
        assert!(!all.is_error, "{}", all.text);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("a.txt")).unwrap(),
            "qux BAR qux\n"
        );
    }

    #[test]
    fn paths_outside_root_are_rejected() {
        let dir = TempDir::new();
        let outside = ToolResult::err("path is outside the workspace");
        assert_eq!(
            exec(&dir.0, "read_file", json!({"path": "../x.txt"})),
            outside
        );
        assert_eq!(
            exec(&dir.0, "read_file", json!({"path": "/etc/hosts"})),
            outside
        );
        assert_eq!(
            exec(
                &dir.0,
                "write_file",
                json!({"path": "sub/../../y.txt", "content": "y"})
            ),
            outside
        );
        assert_eq!(
            resolve_path(&dir.0, "sub/../new.txt"),
            Ok(dir.0.join("new.txt"))
        );
        assert_eq!(
            resolve_path(&dir.0, &dir.0.join("./b/c.rs").display().to_string()),
            Ok(dir.0.join("b/c.rs"))
        );
    }

    #[test]
    fn read_file_offset_and_limit() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("lines.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        assert_eq!(
            exec(
                &dir.0,
                "read_file",
                json!({"path": "lines.txt", "offset": 2, "limit": 2})
            ),
            ToolResult::ok("two\nthree\n")
        );
        assert_eq!(
            exec(
                &dir.0,
                "read_file",
                json!({"path": "lines.txt", "offset": 4})
            ),
            ToolResult::ok("four\nfive\n")
        );
        assert_eq!(
            exec(&dir.0, "read_file", json!({"path": "lines.txt"})),
            ToolResult::ok("one\ntwo\nthree\nfour\nfive\n")
        );
    }

    #[test]
    fn input_and_name_errors() {
        let dir = TempDir::new();
        assert_eq!(
            exec(&dir.0, "read_file", json!({})),
            ToolResult::err("missing required parameter: path")
        );
        assert_eq!(
            exec(&dir.0, "frobnicate", json!({})),
            ToolResult::err("unknown tool: frobnicate")
        );
    }

    #[test]
    fn edit_preview_shows_diff() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("a.txt"), "hello\n").unwrap();
        let preview = preview(
            &dir.0,
            "edit_file",
            &json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
        );
        assert_eq!(preview.title, "Edit a.txt");
        assert_eq!(preview.kind, ToolKind::Edit);
        assert_eq!(
            preview.content,
            vec![ToolContent::Diff {
                path: dir.0.join("a.txt"),
                old: Some("hello\n".into()),
                new: "bye\n".into()
            }]
        );
        assert!(requires_approval("edit_file") && !requires_approval("read_file"));
    }
}
