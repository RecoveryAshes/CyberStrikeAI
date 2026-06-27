use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};
use thiserror::Error;

const DEFAULT_MAX_RESOURCE_BYTES: u64 = 128 * 1024;
const DEFAULT_FILE_LIST_LIMIT: usize = 4000;
const DEFAULT_FILE_LIST_DEPTH: usize = 24;
const DEFAULT_SEARCH_LIMIT: usize = 100;
const DEFAULT_SEARCH_FILE_MAX_BYTES: u64 = 512 * 1024;

#[derive(Debug, Default, Clone)]
pub struct SkillRuntime {
    skills: HashMap<String, SkillPackage>,
}

#[derive(Debug, Clone, Default)]
struct SkillPackage {
    name: String,
    description: String,
    content: String,
    base_dir: String,
    files: Vec<SkillFile>,
    resources: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct SkillFile {
    path: String,
    size: u64,
    is_dir: bool,
}

#[derive(Debug, Clone, Default)]
struct SkillSearchResult {
    source: String,
    pattern: String,
    path: String,
    include: Option<String>,
    matches: Vec<SkillSearchMatch>,
    truncated: bool,
}

#[derive(Debug, Clone, Default)]
struct SkillSearchMatch {
    path: String,
    line: u64,
    text: String,
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("invalid skill tool arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("skill name is required")]
    MissingName,
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("skill resource not found: {skill}/{path}")]
    ResourceNotFound { skill: String, path: String },
    #[error("skill base_dir is not configured: {0}")]
    MissingBaseDir(String),
    #[error("invalid skill resource path: {0}")]
    InvalidPath(String),
    #[error("skill resource is a directory: {skill}/{path}")]
    ResourceIsDirectory { skill: String, path: String },
    #[error("read skill resource {skill}/{path}: {source}")]
    ReadResource {
        skill: String,
        path: String,
        source: io::Error,
    },
}

impl From<SkillError> for crate::tool_runtime::ToolError {
    fn from(value: SkillError) -> Self {
        crate::tool_runtime::ToolError::Skill(value.to_string())
    }
}

impl SkillRuntime {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let mut skills = HashMap::new();
        let skills_enabled = context
            .get("skills_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !skills_enabled {
            return Self { skills };
        }
        let skills_source = context
            .get("skills_source")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let mut attempted_dir_load = false;
        if skills_enabled && skills_source != "go_context" {
            if let Some(root) = context
                .get("skills_dir")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                attempted_dir_load = true;
                let allowlist = parse_skills_allowlist(context);
                load_skill_packages_from_dir(Path::new(root), &allowlist, &mut skills);
            }
        }
        if skills_source == "go_context" || (!attempted_dir_load && skills.is_empty()) {
            load_skill_packages_from_context(context, &mut skills);
        }
        Self { skills }
    }

    pub fn selected_skills(&self) -> Vec<String> {
        let mut names: Vec<String> = self.skills.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn execute_call(&self, arguments: &str) -> Result<String, SkillError> {
        let args: Value = serde_json::from_str(arguments)?;
        let name = args
            .get("name")
            .or_else(|| args.get("skill_name"))
            .or_else(|| args.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(SkillError::MissingName)?;
        let content = self
            .skills
            .get(name)
            .ok_or_else(|| SkillError::NotFound(name.to_string()))?;
        let requested_resources = args
            .get("resources")
            .or_else(|| args.get("resource_paths"))
            .or_else(|| args.get("paths"))
            .or_else(|| args.get("files"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let list_pattern = args
            .get("file_pattern")
            .or_else(|| args.get("pattern"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty());
        let file_limit = args
            .get("file_limit")
            .or_else(|| args.get("limit"))
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, DEFAULT_FILE_LIST_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_FILE_LIST_LIMIT);
        let search_pattern = args
            .get("grep")
            .or_else(|| args.get("search"))
            .or_else(|| args.get("content_pattern"))
            .or_else(|| args.get("ripgrep_pattern"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty());
        let search_path = args
            .get("path")
            .or_else(|| args.get("search_path"))
            .or_else(|| args.get("grep_path"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let include = args
            .get("include")
            .or_else(|| args.get("glob"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(ToOwned::to_owned);
        let search_limit = args
            .get("search_limit")
            .or_else(|| args.get("grep_limit"))
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, DEFAULT_SEARCH_LIMIT as u64) as usize)
            .unwrap_or(DEFAULT_SEARCH_LIMIT);

        let mut resources = Map::new();
        for path in requested_resources {
            let body = content.read_resource(&path)?;
            resources.insert(path, Value::String(body.clone()));
        }
        let package_files = content.package_files(list_pattern, file_limit);
        let search_result = match search_pattern {
            Some(pattern) => {
                content.search(pattern, search_path, include.as_deref(), search_limit)?
            }
            None => SkillSearchResult::default(),
        };

        Ok(json!({
            "tool": "skill",
            "name": name,
            "display_name": content.name,
            "description": content.description,
            "content": content.content,
            "base_dir": content.base_dir,
            "file_listing": {
                "source": if content.base_dir.is_empty() { "context_package_files" } else { "filesystem" },
                "pattern": list_pattern,
                "limit": file_limit,
                "count": package_files.len()
            },
            "package_files": package_files.iter().map(|file| {
                json!({
                    "path": file.path,
                    "size": file.size,
                    "is_dir": file.is_dir
                })
            }).collect::<Vec<_>>(),
            "resources": resources,
            "search": {
                "source": search_result.source,
                "pattern": search_result.pattern,
                "path": search_result.path,
                "include": search_result.include,
                "count": search_result.matches.len(),
                "truncated": search_result.truncated,
                "matches": search_result.matches.iter().map(|item| {
                    json!({
                        "path": item.path,
                        "line": item.line,
                        "text": item.text
                    })
                }).collect::<Vec<_>>()
            }
        })
        .to_string())
    }
}

impl SkillPackage {
    fn read_resource(&self, path: &str) -> Result<String, SkillError> {
        let rel_path = normalize_resource_path(path)?;
        if let Some(body) = self.resources.get(&rel_path) {
            return Ok(body.clone());
        }
        if self.base_dir.trim().is_empty() {
            return Err(SkillError::MissingBaseDir(self.name.clone()));
        }
        let base_dir = PathBuf::from(self.base_dir.trim());
        let abs = safe_join(&base_dir, &rel_path)?;
        let metadata = fs::metadata(&abs).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                SkillError::ResourceNotFound {
                    skill: self.name.clone(),
                    path: rel_path.clone(),
                }
            } else {
                SkillError::ReadResource {
                    skill: self.name.clone(),
                    path: rel_path.clone(),
                    source,
                }
            }
        })?;
        if metadata.is_dir() {
            return Err(SkillError::ResourceIsDirectory {
                skill: self.name.clone(),
                path: rel_path,
            });
        }
        let max_bytes = DEFAULT_MAX_RESOURCE_BYTES;
        let bytes = if metadata.len() > max_bytes {
            let file = fs::File::open(&abs).map_err(|source| SkillError::ReadResource {
                skill: self.name.clone(),
                path: rel_path.clone(),
                source,
            })?;
            let mut reader = file.take(max_bytes);
            let mut bytes = Vec::with_capacity(max_bytes as usize);
            std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|source| {
                SkillError::ReadResource {
                    skill: self.name.clone(),
                    path: rel_path.clone(),
                    source,
                }
            })?;
            bytes
        } else {
            fs::read(&abs).map_err(|source| SkillError::ReadResource {
                skill: self.name.clone(),
                path: rel_path.clone(),
                source,
            })?
        };
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    fn package_files(&self, pattern: Option<&str>, limit: usize) -> Vec<SkillFile> {
        let mut files = if self.base_dir.trim().is_empty() {
            self.files.clone()
        } else {
            list_package_files(Path::new(self.base_dir.trim()), limit)
                .unwrap_or_else(|_| self.files.clone())
        };
        if let Some(pattern) = pattern {
            let pattern = pattern.to_lowercase();
            files.retain(|file| file.path.to_lowercase().contains(&pattern));
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.truncate(limit);
        files
    }

    fn search(
        &self,
        pattern: &str,
        search_path: &str,
        include: Option<&str>,
        limit: usize,
    ) -> Result<SkillSearchResult, SkillError> {
        if self.base_dir.trim().is_empty() {
            return Ok(search_embedded_resources(
                &self.resources,
                pattern,
                search_path,
                include,
                limit,
            ));
        }
        let base_dir = PathBuf::from(self.base_dir.trim());
        let base = base_dir
            .canonicalize()
            .map_err(|_| SkillError::MissingBaseDir(self.name.clone()))?;
        let search_root = safe_search_path(&base, search_path, &self.name)?;
        Ok(search_filesystem(
            &base,
            &search_root,
            pattern,
            search_path,
            include,
            limit,
        ))
    }
}

fn load_skill_packages_from_context(
    context: &Map<String, Value>,
    skills: &mut HashMap<String, SkillPackage>,
) {
    if let Some(obj) = context.get("skills").and_then(Value::as_object) {
        for (name, value) in obj {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if let Some(content) = value.as_str() {
                skills.insert(
                    name.to_string(),
                    SkillPackage {
                        name: name.to_string(),
                        content: content.to_string(),
                        ..SkillPackage::default()
                    },
                );
                continue;
            }
            if let Some(obj) = value.as_object() {
                skills.insert(name.to_string(), parse_skill_package(name, obj));
            }
        }
    }
}

fn parse_skills_allowlist(context: &Map<String, Value>) -> Option<Vec<String>> {
    let items = context.get("skills_allowlist")?.as_array()?;
    let mut out = Vec::new();
    for item in items {
        let Some(value) = item
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        out.push(value.to_lowercase());
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn load_skill_packages_from_dir(
    skills_root: &Path,
    allowlist: &Option<Vec<String>>,
    skills: &mut HashMap<String, SkillPackage>,
) {
    let Ok(entries) = fs::read_dir(skills_root) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if dir_name.starts_with('.') {
            continue;
        }
        if let Some(allowlist) = allowlist {
            let lower = dir_name.to_lowercase();
            if !allowlist.iter().any(|item| item == &lower) {
                continue;
            }
        }
        if let Some(package) = load_skill_package_from_dir(&entry.path(), &dir_name) {
            skills.insert(dir_name, package);
        }
    }
}

fn load_skill_package_from_dir(skill_dir: &Path, dir_name: &str) -> Option<SkillPackage> {
    let base_dir = skill_dir.canonicalize().ok()?;
    let raw = fs::read_to_string(base_dir.join("SKILL.md")).ok()?;
    let parsed = parse_skill_markdown(&raw)?;
    let name = if parsed.name.trim().is_empty() {
        dir_name.to_string()
    } else {
        parsed.name
    };
    Some(SkillPackage {
        name,
        description: parsed.description,
        content: parsed.body,
        base_dir: base_dir.to_string_lossy().to_string(),
        files: list_package_files(&base_dir, DEFAULT_FILE_LIST_LIMIT).unwrap_or_default(),
        resources: HashMap::new(),
    })
}

#[derive(Debug, Clone, Default)]
struct ParsedSkillMarkdown {
    name: String,
    description: String,
    body: String,
}

fn parse_skill_markdown(raw: &str) -> Option<ParsedSkillMarkdown> {
    let text = raw.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    if in_frontmatter {
        return None;
    }
    let mut parsed = ParsedSkillMarkdown {
        body: body.join("\n").trim().to_string(),
        ..ParsedSkillMarkdown::default()
    };
    for line in frontmatter {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = trim_yaml_scalar(value);
        match key.trim() {
            "name" => parsed.name = value,
            "description" => parsed.description = value,
            _ => {}
        }
    }
    Some(parsed)
}

fn trim_yaml_scalar(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

fn parse_skill_package(name: &str, obj: &Map<String, Value>) -> SkillPackage {
    let files = obj
        .get("package_files")
        .or_else(|| obj.get("files"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_skill_file).collect())
        .unwrap_or_default();
    let resources = obj
        .get("resources")
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .filter_map(|(path, value)| {
                    value
                        .as_str()
                        .map(|content| (path.clone(), content.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    SkillPackage {
        name: obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(name)
            .to_string(),
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content: obj
            .get("content")
            .or_else(|| obj.get("skill_md"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        base_dir: obj
            .get("base_dir")
            .or_else(|| obj.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        files,
        resources,
    }
}

fn parse_skill_file(value: &Value) -> Option<SkillFile> {
    let obj = value.as_object()?;
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())?
        .to_string();
    Some(SkillFile {
        path,
        size: obj.get("size").and_then(Value::as_u64).unwrap_or_default(),
        is_dir: obj
            .get("is_dir")
            .or_else(|| obj.get("isDir"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn normalize_resource_path(path: &str) -> Result<String, SkillError> {
    let trimmed = path.trim().trim_start_matches('/').replace('\\', "/");
    if trimmed.is_empty() || trimmed == "." {
        return Err(SkillError::InvalidPath(path.to_string()));
    }
    let normalized = Path::new(&trimmed);
    for component in normalized.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SkillError::InvalidPath(path.to_string()));
        }
    }
    if trimmed.split('/').any(|segment| segment == "..") {
        return Err(SkillError::InvalidPath(path.to_string()));
    }
    Ok(trimmed)
}

fn safe_join(base_dir: &Path, rel_path: &str) -> Result<PathBuf, SkillError> {
    let normalized = normalize_resource_path(rel_path)?;
    let base = base_dir
        .canonicalize()
        .map_err(|_| SkillError::MissingBaseDir(base_dir.to_string_lossy().to_string()))?;
    let candidate = base.join(Path::new(&normalized));
    let parent = candidate
        .parent()
        .unwrap_or(&base)
        .canonicalize()
        .map_err(|source| SkillError::ReadResource {
            skill: base.to_string_lossy().to_string(),
            path: normalized.clone(),
            source,
        })?;
    if !parent.starts_with(&base) {
        return Err(SkillError::InvalidPath(rel_path.to_string()));
    }
    Ok(candidate)
}

fn safe_search_path(base: &Path, rel_path: &str, skill: &str) -> Result<PathBuf, SkillError> {
    let trimmed = rel_path.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(base.to_path_buf());
    }
    let normalized = normalize_resource_path(trimmed)?;
    let candidate = base.join(Path::new(&normalized));
    let canonical = candidate
        .canonicalize()
        .map_err(|source| SkillError::ReadResource {
            skill: skill.to_string(),
            path: normalized.clone(),
            source,
        })?;
    if !canonical.starts_with(base) {
        return Err(SkillError::InvalidPath(rel_path.to_string()));
    }
    Ok(canonical)
}

fn search_filesystem(
    base: &Path,
    search_root: &Path,
    pattern: &str,
    requested_path: &str,
    include: Option<&str>,
    limit: usize,
) -> SkillSearchResult {
    if let Some(result) =
        search_with_ripgrep(base, search_root, pattern, requested_path, include, limit)
    {
        return result;
    }
    search_with_rust_walk(base, search_root, pattern, requested_path, include, limit)
}

fn search_with_ripgrep(
    base: &Path,
    search_root: &Path,
    pattern: &str,
    requested_path: &str,
    include: Option<&str>,
    limit: usize,
) -> Option<SkillSearchResult> {
    let mut command = Command::new("rg");
    command
        .arg("--json")
        .arg("--hidden")
        .arg("--no-follow")
        .arg("--line-number")
        .arg("--max-count")
        .arg(limit.to_string());
    if let Some(include) = include {
        command.arg("--glob").arg(include);
    }
    command.arg(pattern).arg(search_root);
    let output = command.output().ok()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    let mut truncated = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let Some(data) = value.get("data") else {
            continue;
        };
        let path = data
            .get("path")
            .and_then(|path| path.get("text"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .and_then(|path| rel_from_base(base, &path))
            .unwrap_or_default();
        let line_number = data
            .get("line_number")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let text = data
            .get("lines")
            .and_then(|lines| lines.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        matches.push(SkillSearchMatch {
            path,
            line: line_number,
            text,
        });
        if matches.len() >= limit {
            truncated = true;
            break;
        }
    }
    Some(SkillSearchResult {
        source: "ripgrep".to_string(),
        pattern: pattern.to_string(),
        path: requested_path.to_string(),
        include: include.map(ToOwned::to_owned),
        matches,
        truncated,
    })
}

fn search_with_rust_walk(
    base: &Path,
    search_root: &Path,
    pattern: &str,
    requested_path: &str,
    include: Option<&str>,
    limit: usize,
) -> SkillSearchResult {
    let mut matches = Vec::new();
    let mut truncated = false;
    let needle = pattern.to_lowercase();
    search_with_rust_walk_inner(
        base,
        search_root,
        include,
        limit,
        &needle,
        &mut matches,
        &mut truncated,
    );
    SkillSearchResult {
        source: "filesystem_substring_fallback".to_string(),
        pattern: pattern.to_string(),
        path: requested_path.to_string(),
        include: include.map(ToOwned::to_owned),
        matches,
        truncated,
    }
}

fn search_with_rust_walk_inner(
    base: &Path,
    path: &Path,
    include: Option<&str>,
    limit: usize,
    needle: &str,
    matches: &mut Vec<SkillSearchMatch>,
    truncated: &mut bool,
) {
    if matches.len() >= limit {
        *truncated = true;
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            search_with_rust_walk_inner(
                base,
                &entry.path(),
                include,
                limit,
                needle,
                matches,
                truncated,
            );
            if *truncated {
                return;
            }
        }
        return;
    }
    if metadata.len() > DEFAULT_SEARCH_FILE_MAX_BYTES {
        return;
    }
    let rel = rel_from_base(base, path).unwrap_or_default();
    if !include_matches(include, &rel) {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    if bytes.contains(&0) {
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    for (idx, line) in text.lines().enumerate() {
        if !line.to_lowercase().contains(needle) {
            continue;
        }
        matches.push(SkillSearchMatch {
            path: rel.clone(),
            line: (idx + 1) as u64,
            text: truncate(line.trim_end(), 300),
        });
        if matches.len() >= limit {
            *truncated = true;
            return;
        }
    }
}

fn search_embedded_resources(
    resources: &HashMap<String, String>,
    pattern: &str,
    requested_path: &str,
    include: Option<&str>,
    limit: usize,
) -> SkillSearchResult {
    let mut entries = resources.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let needle = pattern.to_lowercase();
    let mut matches = Vec::new();
    let mut truncated = false;
    let requested_prefix = requested_path.trim().trim_matches('/');
    for (path, body) in entries {
        if !requested_prefix.is_empty()
            && requested_prefix != "."
            && !path.starts_with(requested_prefix)
        {
            continue;
        }
        if !include_matches(include, path) {
            continue;
        }
        for (idx, line) in body.lines().enumerate() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            matches.push(SkillSearchMatch {
                path: path.clone(),
                line: (idx + 1) as u64,
                text: truncate(line.trim_end(), 300),
            });
            if matches.len() >= limit {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    SkillSearchResult {
        source: "context_resources_substring".to_string(),
        pattern: pattern.to_string(),
        path: requested_path.to_string(),
        include: include.map(ToOwned::to_owned),
        matches,
        truncated,
    }
}

fn rel_from_base(base: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(base)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .filter(|rel| !rel.is_empty())
}

fn include_matches(include: Option<&str>, path: &str) -> bool {
    let Some(include) = include.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    simple_glob_match(include, path)
}

fn simple_glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**/*" {
        return true;
    }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return value.ends_with(&format!(".{ext}"));
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    value == pattern || value.ends_with(&format!("/{pattern}"))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn list_package_files(base_dir: &Path, limit: usize) -> io::Result<Vec<SkillFile>> {
    let base = base_dir.canonicalize()?;
    let mut out = Vec::new();
    list_package_files_inner(&base, &base, 0, limit, &mut out)?;
    Ok(out)
}

fn list_package_files_inner(
    base: &Path,
    dir: &Path,
    depth: usize,
    limit: usize,
    out: &mut Vec<SkillFile>,
) -> io::Result<()> {
    if depth > DEFAULT_FILE_LIST_DEPTH || out.len() >= limit {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if out.len() >= limit {
            break;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata = entry.metadata()?;
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(SkillFile {
            path: rel,
            size: metadata.len(),
            is_dir: metadata.is_dir(),
        });
        if metadata.is_dir() {
            list_package_files_inner(base, &path, depth + 1, limit, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    #[test]
    fn loads_skills_from_context_and_executes() {
        let mut context = Map::new();
        context.insert(
            "skills".to_string(),
            json!({"demo": "Use this demo skill instruction."}),
        );
        let runtime = SkillRuntime::from_context(&context);
        assert_eq!(runtime.selected_skills(), vec!["demo".to_string()]);

        let result = runtime.execute_call(r#"{"name":"demo"}"#).unwrap();
        assert!(result.contains("Use this demo skill instruction."));
    }

    #[test]
    fn loads_skill_package_and_progressive_resources() {
        let mut context = Map::new();
        context.insert(
            "skills".to_string(),
            json!({
                "demo": {
                    "name": "Demo",
                    "description": "Demo skill",
                    "content": "Read references/guide.md when needed.",
                    "base_dir": "/skills/demo",
                    "package_files": [
                        {"path": "SKILL.md", "size": 100},
                        {"path": "references/guide.md", "size": 200}
                    ],
                    "resources": {
                        "references/guide.md": "Guide body"
                    }
                }
            }),
        );
        let runtime = SkillRuntime::from_context(&context);
        let result = runtime
            .execute_call(r#"{"name":"demo","resources":["references/guide.md"]}"#)
            .unwrap();

        assert!(result.contains("Read references/guide.md"));
        assert!(result.contains("references/guide.md"));
        assert!(result.contains("Guide body"));
        assert!(result.contains("/skills/demo"));
    }

    #[test]
    fn lazily_reads_skill_resources_from_base_dir() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-skill-runtime-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("SKILL.md"), "Use references/guide.md").unwrap();
        std::fs::write(
            root.join("references").join("guide.md"),
            "Guide body from disk",
        )
        .unwrap();
        let mut context = Map::new();
        context.insert(
            "skills".to_string(),
            json!({
                "demo": {
                    "name": "Demo",
                    "description": "Demo skill",
                    "content": "Read references/guide.md when needed.",
                    "base_dir": root.to_string_lossy().to_string(),
                    "package_files": [{"path": "SKILL.md", "size": 100}]
                }
            }),
        );
        let runtime = SkillRuntime::from_context(&context);
        let result = runtime
            .execute_call(
                r#"{"name":"demo","resources":["references/guide.md"],"file_pattern":"guide"}"#,
            )
            .unwrap();

        assert!(result.contains("Guide body from disk"));
        assert!(result.contains("\"source\":\"filesystem\""));
        assert!(result.contains("references/guide.md"));
        let err = runtime
            .execute_call(r#"{"name":"demo","resources":["../secret.txt"]}"#)
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidPath(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scans_skill_packages_from_skills_dir() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-skill-runtime-dir-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let skill_dir = root.join("demo");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nUse references/guide.md when needed.\n",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("references").join("guide.md"),
            "Guide body from scanned package",
        )
        .unwrap();
        let mut context = Map::new();
        context.insert("skills_enabled".to_string(), json!(true));
        context.insert(
            "skills_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );

        let runtime = SkillRuntime::from_context(&context);
        assert_eq!(runtime.selected_skills(), vec!["demo".to_string()]);
        let result = runtime
            .execute_call(
                r#"{"name":"demo","resources":["references/guide.md"],"file_pattern":"guide"}"#,
            )
            .unwrap();

        assert!(result.contains("Use references/guide.md"));
        assert!(result.contains("Demo skill"));
        assert!(result.contains("Guide body from scanned package"));
        assert!(result.contains("\"source\":\"filesystem\""));
        let err = runtime
            .execute_call(r#"{"name":"demo","resources":["../secret.txt"]}"#)
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidPath(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scans_skill_packages_with_allowlist() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-skill-runtime-allowlist-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["demo", "other"] {
            let skill_dir = root.join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name}\n---\n{name} body\n"),
            )
            .unwrap();
        }
        let mut context = Map::new();
        context.insert("skills_enabled".to_string(), json!(true));
        context.insert(
            "skills_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        context.insert("skills_allowlist".to_string(), json!(["demo"]));

        let runtime = SkillRuntime::from_context(&context);
        assert_eq!(runtime.selected_skills(), vec!["demo".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skills_enabled_false_disables_directory_and_context_skills() {
        let mut context = Map::new();
        context.insert("skills_enabled".to_string(), json!(false));
        context.insert(
            "skills".to_string(),
            json!({"demo": "Use this demo skill instruction."}),
        );

        let runtime = SkillRuntime::from_context(&context);
        assert!(runtime.selected_skills().is_empty());
    }

    #[test]
    fn searches_skill_files_by_content_with_path_guard() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-skill-runtime-search-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("SKILL.md"), "Use references/guide.md").unwrap();
        std::fs::write(
            root.join("references").join("guide.md"),
            "alpha\nneedle appears here\nomega\n",
        )
        .unwrap();
        let mut context = Map::new();
        context.insert(
            "skills".to_string(),
            json!({
                "demo": {
                    "name": "Demo",
                    "content": "Read references/guide.md when needed.",
                    "base_dir": root.to_string_lossy().to_string()
                }
            }),
        );
        let runtime = SkillRuntime::from_context(&context);
        let result = runtime
            .execute_call(r#"{"name":"demo","grep":"needle","path":"references","include":"*.md"}"#)
            .unwrap();

        assert!(result.contains("\"search\""));
        assert!(result.contains("references/guide.md"));
        assert!(result.contains("needle appears here"));
        assert!(
            result.contains("\"source\":\"ripgrep\"")
                || result.contains("\"source\":\"filesystem_substring_fallback\"")
        );
        let err = runtime
            .execute_call(r#"{"name":"demo","grep":"needle","path":"../"}"#)
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidPath(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn searches_embedded_skill_resources() {
        let mut context = Map::new();
        context.insert(
            "skills".to_string(),
            json!({
                "demo": {
                    "name": "Demo",
                    "content": "Read references/guide.md when needed.",
                    "resources": {
                        "references/guide.md": "embedded needle\nother line",
                        "scripts/run.sh": "needle but excluded"
                    }
                }
            }),
        );
        let runtime = SkillRuntime::from_context(&context);
        let result = runtime
            .execute_call(
                r#"{"name":"demo","search":"needle","path":"references","include":"*.md"}"#,
            )
            .unwrap();

        assert!(result.contains("\"source\":\"context_resources_substring\""));
        assert!(result.contains("references/guide.md"));
        assert!(result.contains("embedded needle"));
        assert!(!result.contains("scripts/run.sh"));
    }
}
