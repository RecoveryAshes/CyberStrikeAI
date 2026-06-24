use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const DEFAULT_READ_MAX_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_SEARCH_FILE_MAX_BYTES: u64 = 1024 * 1024;
const DEFAULT_LIST_LIMIT: usize = 200;
const DEFAULT_SEARCH_LIMIT: usize = 100;
const DEFAULT_OUTPUT_MAX_CHARS: usize = 20000;
const SHELL_READ_CHUNK_BYTES: usize = 8192;
const SHELL_DELTA_FLUSH_BYTES: usize = 2048;
const SHELL_DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct FilesystemRuntime {
    enabled: bool,
    workspace_root: Option<PathBuf>,
    timeout: Duration,
}

impl Default for FilesystemRuntime {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace_root: None,
            timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilesystemToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub requires_approval: bool,
}

#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("filesystem tools are not enabled")]
    Disabled,
    #[error("workspace_root is not configured")]
    MissingWorkspaceRoot,
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("path escapes workspace_root: {0}")]
    PathEscapesWorkspace(String),
    #[error("path does not exist: {0}")]
    NotFound(String),
    #[error("expected file path: {0}")]
    ExpectedFile(String),
    #[error("expected directory path: {0}")]
    ExpectedDirectory(String),
    #[error("file is too large: {path} ({size} bytes > {max} bytes)")]
    FileTooLarge { path: String, size: u64, max: u64 },
    #[error("missing required argument: {0}")]
    MissingArgument(&'static str),
    #[error("edit target text was not found")]
    EditTargetNotFound,
    #[error("edit target text occurs multiple times; provide a more specific old_string")]
    EditTargetAmbiguous,
    #[error("command timed out after {0} seconds")]
    CommandTimedOut(u64),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
}

impl FilesystemRuntime {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let enabled = context
            .get("filesystem_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let workspace_root = context
            .get("workspace_root")
            .or_else(|| context.get("agent_runtime_workspace_root"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let timeout_secs = context
            .get("tool_timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .max(1);
        Self {
            enabled,
            workspace_root,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled && self.workspace_root.is_some()
    }

    pub fn tool_specs(&self) -> Vec<FilesystemToolSpec> {
        if !self.is_enabled() {
            return Vec::new();
        }
        vec![
            FilesystemToolSpec {
                name: "ls",
                description: "List files under the configured workspace root.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path under the workspace root."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                    }
                }),
                requires_approval: false,
            },
            FilesystemToolSpec {
                name: "read_file",
                description: "Read a UTF-8/text file under the configured workspace root.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative file path under the workspace root."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Optional zero-based line offset."},
                        "limit": {"type": "integer", "minimum": 1, "description": "Optional maximum number of lines to return."}
                    },
                    "required": ["path"]
                }),
                requires_approval: false,
            },
            FilesystemToolSpec {
                name: "write_file",
                description: "Create or overwrite a file under the configured workspace root.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                requires_approval: true,
            },
            FilesystemToolSpec {
                name: "edit_file",
                description: "Replace exactly one text occurrence in a file under the configured workspace root.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
                requires_approval: true,
            },
            FilesystemToolSpec {
                name: "glob",
                description: "Find paths under the workspace root using a simple glob pattern.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "description": "Optional search root under the workspace."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                    },
                    "required": ["pattern"]
                }),
                requires_approval: false,
            },
            FilesystemToolSpec {
                name: "grep",
                description: "Search text files under the workspace root for a substring or simple pattern.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "query": {"type": "string"},
                        "path": {"type": "string"},
                        "include": {"type": "string", "description": "Optional file glob such as *.go or **/*.rs."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                    }
                }),
                requires_approval: false,
            },
            FilesystemToolSpec {
                name: "execute",
                description: "Run a shell command in the configured workspace root.",
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "cwd": {"type": "string", "description": "Optional relative working directory under the workspace."}
                    },
                    "required": ["command"]
                }),
                requires_approval: true,
            },
        ]
    }

    pub fn execute_call(
        &self,
        tool_name: &str,
        arguments: &str,
    ) -> Result<String, FilesystemError> {
        self.execute_call_with_delta(tool_name, arguments, None)
    }

    pub fn execute_call_with_delta(
        &self,
        tool_name: &str,
        arguments: &str,
        on_delta: Option<&mut dyn FnMut(String)>,
    ) -> Result<String, FilesystemError> {
        if !self.enabled {
            return Err(FilesystemError::Disabled);
        }
        let args: Value = serde_json::from_str(arguments)?;
        let result = match tool_name {
            "ls" | "list_dir" => self.list_dir(&args)?,
            "read_file" | "read" => self.read_file(&args)?,
            "write_file" | "write" => self.write_file(&args)?,
            "edit_file" | "edit" => self.edit_file(&args)?,
            "glob" => self.glob(&args)?,
            "grep" => self.grep(&args)?,
            "execute" | "bash" => self.execute_shell(&args, on_delta)?,
            other => {
                return Err(FilesystemError::InvalidPath(format!(
                    "unsupported filesystem tool {other}"
                )))
            }
        };
        Ok(result.to_string())
    }

    fn list_dir(&self, args: &Value) -> Result<Value, FilesystemError> {
        let requested = optional_string(args, &["path", "dir"]).unwrap_or(".");
        let limit = integer_arg(args, "limit")
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .max(1);
        let path = self.resolve_existing_path(requested)?;
        let meta = fs::metadata(&path)?;
        if !meta.is_dir() {
            return Err(FilesystemError::ExpectedDirectory(requested.to_string()));
        }
        let mut entries = fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let root = self.root()?;
        let mut out = Vec::new();
        let mut truncated = false;
        for entry in entries {
            if out.len() >= limit {
                truncated = true;
                break;
            }
            let metadata = entry.metadata()?;
            out.push(json!({
                "path": rel_from_base(&root, &entry.path()),
                "name": entry.file_name().to_string_lossy(),
                "is_dir": metadata.is_dir(),
                "size": metadata.len()
            }));
        }
        Ok(json!({
            "tool": "ls",
            "path": requested,
            "entries": out,
            "truncated": truncated
        }))
    }

    fn read_file(&self, args: &Value) -> Result<Value, FilesystemError> {
        let requested = required_string(args, &["path", "file_path"])?;
        let path = self.resolve_existing_path(requested)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(FilesystemError::ExpectedFile(requested.to_string()));
        }
        if metadata.len() > DEFAULT_READ_MAX_BYTES {
            return Err(FilesystemError::FileTooLarge {
                path: requested.to_string(),
                size: metadata.len(),
                max: DEFAULT_READ_MAX_BYTES,
            });
        }
        let bytes = fs::read(&path)?;
        let binary = bytes.contains(&0);
        let text = String::from_utf8_lossy(&bytes);
        let offset = integer_arg(args, "offset").unwrap_or(0);
        let limit = integer_arg(args, "limit");
        let all_lines: Vec<&str> = text.lines().collect();
        let selected = if let Some(limit) = limit {
            all_lines
                .iter()
                .skip(offset)
                .take(limit.max(1))
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        } else if offset > 0 {
            all_lines
                .iter()
                .skip(offset)
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.to_string()
        };
        Ok(json!({
            "tool": "read_file",
            "path": requested,
            "content": truncate(&selected, DEFAULT_OUTPUT_MAX_CHARS),
            "line_count": all_lines.len(),
            "truncated": selected.chars().count() > DEFAULT_OUTPUT_MAX_CHARS,
            "binary": binary
        }))
    }

    fn write_file(&self, args: &Value) -> Result<Value, FilesystemError> {
        let requested = required_string(args, &["path", "file_path"])?;
        let content = required_string(args, &["content", "text"])?;
        let path = self.resolve_new_path(requested)?;
        if path.exists() {
            let _ = self.resolve_existing_path(requested)?;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            self.ensure_within_root(&parent.canonicalize()?, requested)?;
        }
        fs::write(&path, content.as_bytes())?;
        Ok(json!({
            "tool": "write_file",
            "path": requested,
            "bytes": content.len(),
            "status": "completed"
        }))
    }

    fn edit_file(&self, args: &Value) -> Result<Value, FilesystemError> {
        let requested = required_string(args, &["path", "file_path"])?;
        let old = required_string(args, &["old_string", "old", "find"])?;
        let new = required_string(args, &["new_string", "new", "replace"])?;
        if old.is_empty() {
            return Err(FilesystemError::MissingArgument("old_string"));
        }
        let path = self.resolve_existing_path(requested)?;
        let original = fs::read_to_string(&path)?;
        let count = original.matches(old).count();
        if count == 0 {
            return Err(FilesystemError::EditTargetNotFound);
        }
        if count > 1 {
            return Err(FilesystemError::EditTargetAmbiguous);
        }
        let edited = original.replacen(old, new, 1);
        fs::write(&path, edited.as_bytes())?;
        Ok(json!({
            "tool": "edit_file",
            "path": requested,
            "replacements": 1,
            "status": "completed"
        }))
    }

    fn glob(&self, args: &Value) -> Result<Value, FilesystemError> {
        let pattern = required_string(args, &["pattern", "glob"])?;
        let requested_path = optional_string(args, &["path"]).unwrap_or(".");
        let limit = integer_arg(args, "limit")
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .max(1);
        let root = self.root()?;
        let start = self.resolve_existing_path(requested_path)?;
        let mut matches = Vec::new();
        let mut truncated = false;
        walk_paths(&start, &mut |path, metadata| {
            if matches.len() >= limit {
                truncated = true;
                return false;
            }
            let rel = rel_from_base(&root, path);
            if simple_glob_match(pattern, &rel) {
                matches.push(json!({
                    "path": rel,
                    "is_dir": metadata.is_dir(),
                    "size": metadata.len()
                }));
            }
            true
        })?;
        Ok(json!({
            "tool": "glob",
            "pattern": pattern,
            "path": requested_path,
            "matches": matches,
            "truncated": truncated
        }))
    }

    fn grep(&self, args: &Value) -> Result<Value, FilesystemError> {
        let pattern = required_string(args, &["pattern", "query", "grep"])?;
        let requested_path = optional_string(args, &["path"]).unwrap_or(".");
        let include = optional_string(args, &["include", "glob"]);
        let limit = integer_arg(args, "limit")
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .max(1);
        let root = self.root()?;
        let start = self.resolve_existing_path(requested_path)?;
        let needle = pattern.to_lowercase();
        let mut matches = Vec::new();
        let mut truncated = false;
        walk_paths(&start, &mut |path, metadata| {
            if matches.len() >= limit {
                truncated = true;
                return false;
            }
            if !metadata.is_file() || metadata.len() > DEFAULT_SEARCH_FILE_MAX_BYTES {
                return true;
            }
            let rel = rel_from_base(&root, path);
            if !include_matches(include, &rel) {
                return true;
            }
            let Ok(bytes) = fs::read(path) else {
                return true;
            };
            if bytes.contains(&0) {
                return true;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (index, line) in text.lines().enumerate() {
                if !line.to_lowercase().contains(&needle) {
                    continue;
                }
                matches.push(json!({
                    "path": rel,
                    "line": index + 1,
                    "text": truncate(line.trim_end(), 300)
                }));
                if matches.len() >= limit {
                    truncated = true;
                    return false;
                }
            }
            true
        })?;
        Ok(json!({
            "tool": "grep",
            "pattern": pattern,
            "path": requested_path,
            "include": include,
            "matches": matches,
            "truncated": truncated
        }))
    }

    fn execute_shell(
        &self,
        args: &Value,
        on_delta: Option<&mut dyn FnMut(String)>,
    ) -> Result<Value, FilesystemError> {
        let command = required_string(args, &["command", "cmd"])?;
        let requested_cwd = optional_string(args, &["cwd", "path"]).unwrap_or(".");
        let cwd = self.resolve_existing_path(requested_cwd)?;
        if !fs::metadata(&cwd)?.is_dir() {
            return Err(FilesystemError::ExpectedDirectory(
                requested_cwd.to_string(),
            ));
        }
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            FilesystemError::Io(io::Error::other("stdout pipe was unexpectedly unavailable"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            FilesystemError::Io(io::Error::other("stderr pipe was unexpectedly unavailable"))
        })?;
        let (tx, rx) = mpsc::channel::<ShellChunk>();
        let stdout_handle = spawn_shell_reader(stdout, tx.clone(), ShellStream::Stdout);
        let stderr_handle = spawn_shell_reader(stderr, tx, ShellStream::Stderr);
        let started = Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut callback = on_delta;
        let mut delta_buffer = String::new();
        let mut last_flush = Instant::now();
        let mut emitted_any_delta = false;
        let flush_delta =
            |buffer: &mut String,
             last_flush: &mut Instant,
             emitted_any_delta: &mut bool,
             callback: &mut Option<&mut dyn FnMut(String)>| {
                if buffer.is_empty() {
                    return;
                }
                if let Some(cb) = callback.as_deref_mut() {
                    cb(std::mem::take(buffer));
                    *emitted_any_delta = true;
                } else {
                    buffer.clear();
                }
                *last_flush = Instant::now();
            };
        let status = loop {
            while let Ok(chunk) = rx.try_recv() {
                match chunk.stream {
                    ShellStream::Stdout => stdout.push_str(&chunk.text),
                    ShellStream::Stderr => stderr.push_str(&chunk.text),
                }
                delta_buffer.push_str(&chunk.text);
                if !emitted_any_delta
                    || delta_buffer.len() >= SHELL_DELTA_FLUSH_BYTES
                    || last_flush.elapsed() >= SHELL_DELTA_FLUSH_INTERVAL
                {
                    flush_delta(
                        &mut delta_buffer,
                        &mut last_flush,
                        &mut emitted_any_delta,
                        &mut callback,
                    );
                }
            }

            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                return Err(FilesystemError::CommandTimedOut(self.timeout.as_secs()));
            }
            thread::sleep(Duration::from_millis(25));
        };
        for chunk in rx {
            match chunk.stream {
                ShellStream::Stdout => stdout.push_str(&chunk.text),
                ShellStream::Stderr => stderr.push_str(&chunk.text),
            }
            delta_buffer.push_str(&chunk.text);
            if !emitted_any_delta
                || delta_buffer.len() >= SHELL_DELTA_FLUSH_BYTES
                || last_flush.elapsed() >= SHELL_DELTA_FLUSH_INTERVAL
            {
                flush_delta(
                    &mut delta_buffer,
                    &mut last_flush,
                    &mut emitted_any_delta,
                    &mut callback,
                );
            }
        }
        let _ = stdout_handle.join();
        let _ = stderr_handle.join();
        flush_delta(
            &mut delta_buffer,
            &mut last_flush,
            &mut emitted_any_delta,
            &mut callback,
        );
        Ok(json!({
            "tool": "execute",
            "command": command,
            "cwd": requested_cwd,
            "exit_code": status.code(),
            "success": status.success(),
            "stdout": truncate(&stdout, DEFAULT_OUTPUT_MAX_CHARS),
            "stderr": truncate(&stderr, DEFAULT_OUTPUT_MAX_CHARS),
            "truncated": stdout.chars().count() > DEFAULT_OUTPUT_MAX_CHARS || stderr.chars().count() > DEFAULT_OUTPUT_MAX_CHARS
        }))
    }

    fn root(&self) -> Result<PathBuf, FilesystemError> {
        let Some(root) = &self.workspace_root else {
            return Err(FilesystemError::MissingWorkspaceRoot);
        };
        root.canonicalize().map_err(FilesystemError::Io)
    }

    fn resolve_existing_path(&self, requested: &str) -> Result<PathBuf, FilesystemError> {
        let path = self.resolve_new_path(requested)?;
        if !path.exists() {
            return Err(FilesystemError::NotFound(requested.to_string()));
        }
        let canonical = path.canonicalize()?;
        self.ensure_within_root(&canonical, requested)?;
        Ok(canonical)
    }

    fn resolve_new_path(&self, requested: &str) -> Result<PathBuf, FilesystemError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(FilesystemError::InvalidPath("empty path".to_string()));
        }
        let raw = Path::new(requested);
        if raw.is_absolute() {
            return Err(FilesystemError::InvalidPath(
                "absolute paths are not allowed".to_string(),
            ));
        }
        if raw.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        }) {
            return Err(FilesystemError::InvalidPath(requested.to_string()));
        }
        let root = self.root()?;
        let joined = root.join(raw);
        if !joined.starts_with(&root) {
            return Err(FilesystemError::PathEscapesWorkspace(requested.to_string()));
        }
        Ok(joined)
    }

    fn ensure_within_root(&self, canonical: &Path, requested: &str) -> Result<(), FilesystemError> {
        let root = self.root()?;
        if !canonical.starts_with(&root) {
            return Err(FilesystemError::PathEscapesWorkspace(requested.to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum ShellStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct ShellChunk {
    stream: ShellStream,
    text: String,
}

fn spawn_shell_reader<R>(
    mut reader: R,
    tx: mpsc::Sender<ShellChunk>,
    stream: ShellStream,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buf = [0u8; SHELL_READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n])
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                    if tx.send(ShellChunk { stream, text }).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn required_string<'a>(
    args: &'a Value,
    names: &[&'static str],
) -> Result<&'a str, FilesystemError> {
    optional_string(args, names).ok_or(FilesystemError::MissingArgument(names[0]))
}

fn optional_string<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| args.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn integer_arg(args: &Value, name: &str) -> Option<usize> {
    args.get(name)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn walk_paths(
    start: &Path,
    visit: &mut impl FnMut(&Path, &fs::Metadata) -> bool,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(start)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !visit(start, &metadata) {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(start)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        walk_paths(&entry.path(), visit)?;
    }
    Ok(())
}

fn rel_from_base(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn include_matches(include: Option<&str>, path: &str) -> bool {
    let Some(include) = include.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    simple_glob_match(include, path)
}

fn simple_glob_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().replace('\\', "/");
    let value = value.replace('\\', "/");
    if pattern == "*" || pattern == "**/*" {
        return true;
    }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return value.ends_with(&format!(".{ext}"));
    }
    if let Some(rest) = pattern.strip_prefix("**/") {
        return simple_glob_match(rest, &value)
            || value.ends_with(&format!("/{rest}"))
            || wildcard_match(rest, &value);
    }
    wildcard_match(&pattern, &value)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let s = value.as_bytes();
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_i = 0usize;
    while si < s.len() {
        if pi < p.len() && (p[pi] == s[si] || p[pi] == b'?') {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            match_i = si;
            pi += 1;
        } else if let Some(star_i) = star {
            pi = star_i + 1;
            match_i += 1;
            si = match_i;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-fs-runtime-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn runtime(root: &Path) -> FilesystemRuntime {
        let mut context = Map::new();
        context.insert("filesystem_enabled".to_string(), json!(true));
        context.insert(
            "workspace_root".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        FilesystemRuntime::from_context(&context)
    }

    #[test]
    fn read_file_and_path_guard() {
        let root = test_root("read");
        fs::write(root.join("README.md"), "alpha\nbeta\n").unwrap();
        let runtime = runtime(&root);
        let result = runtime
            .execute_call("read_file", r#"{"path":"README.md","offset":1,"limit":1}"#)
            .unwrap();
        assert!(result.contains("\"content\":\"beta\""));
        let err = runtime
            .execute_call("read_file", r#"{"path":"../secret.txt"}"#)
            .unwrap_err();
        assert!(matches!(err, FilesystemError::InvalidPath(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_and_edit_file() {
        let root = test_root("write-edit");
        let runtime = runtime(&root);
        runtime
            .execute_call(
                "write_file",
                r#"{"path":"notes/a.txt","content":"hello world"}"#,
            )
            .unwrap();
        runtime
            .execute_call(
                "edit_file",
                r#"{"path":"notes/a.txt","old_string":"world","new_string":"runtime"}"#,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("notes/a.txt")).unwrap(),
            "hello runtime"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_and_grep_files() {
        let root = test_root("search");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\nneedle\n").unwrap();
        fs::write(root.join("src/lib.go"), "needle but excluded\n").unwrap();
        let runtime = runtime(&root);
        let glob = runtime
            .execute_call("glob", r#"{"pattern":"src/*.rs"}"#)
            .unwrap();
        assert!(glob.contains("src/main.rs"));
        assert!(!glob.contains("src/lib.go"));
        let grep = runtime
            .execute_call(
                "grep",
                r#"{"pattern":"needle","path":"src","include":"*.rs"}"#,
            )
            .unwrap();
        assert!(grep.contains("src/main.rs"));
        assert!(!grep.contains("src/lib.go"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_runs_in_workspace() {
        let root = test_root("execute");
        let runtime = runtime(&root);
        let result = runtime
            .execute_call("execute", r#"{"command":"pwd && printf done"}"#)
            .unwrap();
        assert!(result.contains("\"success\":true"));
        assert!(result.contains("done"));
        assert!(result.contains(&root.to_string_lossy().to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn execute_streams_output_delta_before_completion() {
        let root = test_root("execute-stream");
        let runtime = runtime(&root);
        let started = Instant::now();
        let mut first_delta_at: Option<Duration> = None;
        let mut deltas = String::new();
        let result = runtime
            .execute_call_with_delta(
                "execute",
                r#"{"command":"printf first; sleep 0.4; printf second"}"#,
                Some(&mut |delta| {
                    if first_delta_at.is_none() {
                        first_delta_at = Some(started.elapsed());
                    }
                    deltas.push_str(&delta);
                }),
            )
            .unwrap();

        assert!(result.contains("\"success\":true"));
        assert!(result.contains("first"));
        assert!(result.contains("second"));
        assert!(deltas.contains("first"));
        assert!(deltas.contains("second"));
        let first = first_delta_at.expect("expected at least one streamed delta");
        assert!(
            first < Duration::from_millis(350),
            "first delta arrived too late: {first:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
