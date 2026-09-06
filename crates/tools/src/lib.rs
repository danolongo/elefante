//! The permanent M0 tool surface.

use elefante_guard::{PathPolicy, Scrubber};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;

/// Tokio reaps the direct child on cancellation; this guard also terminates descendants.
/// `std` offers process-group creation but no safe group-signal API, so use the system
/// kill utility with a fixed signal and a numeric process-group ID (never a shell).
#[cfg(unix)]
struct BashProcessGroup(Option<u32>);

#[cfg(unix)]
impl Drop for BashProcessGroup {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{id}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Provider-neutral tool metadata. `definitions` returns these in name order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A model-facing result. Errors remain ordinary results so the caller can preserve pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool setup failed: {0}")]
    Setup(String),
}

/// Cloneable, stateless-per-call executor suitable for parallel task dispatch.
#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    policy: PathPolicy,
    scrubber: Scrubber,
}

impl ToolExecutor {
    /// Creates an executor rooted at an existing workspace directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let policy =
            PathPolicy::new(root.as_ref()).map_err(|error| ToolError::Setup(error.to_string()))?;
        Ok(Self {
            root: policy.root().to_path_buf(),
            policy,
            scrubber: Scrubber::new(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Executes one call. Every post-construction failure is represented as an error result,
    /// and every result is scrubbed before it crosses this boundary.
    pub async fn execute(&self, name: &str, input: Value) -> ToolResult {
        let result = match name {
            "read" => self.read(input).await,
            "write" => self.write(input).await,
            "edit" => self.edit(input).await,
            "glob" => self.glob(input).await,
            "grep" => self.grep(input).await,
            "bash" => self.bash(input).await,
            "memory_search" => memory_search(input),
            _ => ToolResult::error(format!("unknown tool: {name}")),
        };
        ToolResult {
            content: self.scrubber.scrub(&result.content),
            is_error: result.is_error,
        }
    }

    async fn read(&self, input: Value) -> ToolResult {
        let input: PathInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let path = match self.resolve(&input.path) {
            Ok(path) => path,
            Err(result) => return result,
        };
        match tokio::fs::read_to_string(path).await {
            Ok(content) => ToolResult::success(content),
            Err(error) => ToolResult::error(format!("read failed: {error}")),
        }
    }

    async fn write(&self, input: Value) -> ToolResult {
        let input: WriteInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let path = match self.resolve(&input.path) {
            Ok(path) => path,
            Err(result) => return result,
        };
        if let Some(parent) = path.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult::error(format!("write failed: {error}"));
        }
        match tokio::fs::write(path, input.content.as_bytes()).await {
            Ok(()) => ToolResult::success("write complete"),
            Err(error) => ToolResult::error(format!("write failed: {error}")),
        }
    }

    async fn edit(&self, input: Value) -> ToolResult {
        let input: EditInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        if input.old_string.is_empty() {
            return ToolResult::error("edit failed: old_string must not be empty");
        }
        let path = match self.resolve(&input.path) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) => return ToolResult::error(format!("edit failed: {error}")),
        };
        let matches = content.match_indices(&input.old_string).count();
        if matches != 1 {
            return ToolResult::error(format!(
                "edit failed: expected exactly one match for old_string, found {matches}"
            ));
        }
        let replaced = content.replacen(&input.old_string, &input.new_string, 1);
        match tokio::fs::write(path, replaced.as_bytes()).await {
            Ok(()) => ToolResult::success("edit complete"),
            Err(error) => ToolResult::error(format!("edit failed: {error}")),
        }
    }

    async fn glob(&self, input: Value) -> ToolResult {
        let input: GlobInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        if !contains_glob_meta(&input.pattern) {
            let direct = Path::new(input.path.as_deref().unwrap_or(".")).join(&input.pattern);
            if let Err(result) = self.resolve(&direct) {
                return result;
            }
        }
        let base = match self.resolve(input.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let matcher = match Glob::new(&input.pattern) {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => {
                return ToolResult::error(format!("glob failed: invalid pattern: {error}"));
            }
        };
        let root = self.root.clone();
        let policy = self.policy.clone();
        match tokio::task::spawn_blocking(move || collect_glob(&root, &base, &policy, &matcher))
            .await
        {
            Ok(Ok(paths)) => ToolResult::success(paths.join("\n")),
            Ok(Err(error)) => ToolResult::error(error),
            Err(error) => ToolResult::error(format!("glob failed: {error}")),
        }
    }

    async fn grep(&self, input: Value) -> ToolResult {
        let input: GrepInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let base = match self.resolve(input.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let regex = match Regex::new(&input.pattern) {
            Ok(regex) => regex,
            Err(error) => {
                return ToolResult::error(format!("grep failed: invalid pattern: {error}"));
            }
        };
        let root = self.root.clone();
        let policy = self.policy.clone();
        match tokio::task::spawn_blocking(move || collect_grep(&root, &base, &policy, &regex)).await
        {
            Ok(Ok(lines)) => ToolResult::success(lines.join("\n")),
            Ok(Err(error)) => ToolResult::error(error),
            Err(error) => ToolResult::error(format!("grep failed: {error}")),
        }
    }

    async fn bash(&self, input: Value) -> ToolResult {
        let input: BashInput = match decode(input) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let mut command = Command::new("bash");
        command
            .kill_on_drop(true)
            .arg("-lc")
            .arg(&input.command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return ToolResult::error(format!("bash failed: {error}")),
        };
        #[cfg(unix)]
        let mut group = BashProcessGroup(child.id());
        let result = child.wait_with_output().await;
        #[cfg(unix)]
        if result.is_ok() {
            group.0 = None;
        }
        let output = match result {
            Ok(output) => output,
            Err(error) => return ToolResult::error(format!("bash failed: {error}")),
        };
        let mut content = String::from_utf8_lossy(&output.stdout).into_owned();
        content.push_str(&String::from_utf8_lossy(&output.stderr));
        if output.status.success() {
            ToolResult::success(content)
        } else {
            ToolResult::error(format!("exit status {}\n{content}", output.status))
        }
    }

    fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolResult> {
        self.policy
            .resolve(path)
            .map_err(|error| ToolResult::error(error.to_string()))
    }
}

/// Returns all permanent tools in deterministic lexical name order.
#[must_use]
pub fn definitions() -> Vec<ToolDefinition> {
    let mut definitions = vec![
        definition(
            "bash",
            "Run a policy-approved shell command in the workspace.",
            json!({
                "type": "object", "properties": { "command": { "type": "string" } },
                "required": ["command"], "additionalProperties": false
            }),
        ),
        definition(
            "edit",
            "Replace exactly one literal string occurrence in a file.",
            json!({
                "type": "object", "properties": {
                    "path": { "type": "string" }, "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                }, "required": ["path", "old_string", "new_string"], "additionalProperties": false
            }),
        ),
        definition(
            "glob",
            "List non-secret workspace files matching a glob.",
            json!({
                "type": "object", "properties": {
                    "pattern": { "type": "string" }, "path": { "type": "string" }
                }, "required": ["pattern"], "additionalProperties": false
            }),
        ),
        definition(
            "grep",
            "Search non-secret workspace files with a regular expression.",
            json!({
                "type": "object", "properties": {
                    "pattern": { "type": "string" }, "path": { "type": "string" }
                }, "required": ["pattern"], "additionalProperties": false
            }),
        ),
        definition(
            "memory_search",
            "Search project memory (available in milestone M2).",
            json!({
                "type": "object", "properties": { "query": { "type": "string" } },
                "required": ["query"], "additionalProperties": false
            }),
        ),
        definition(
            "read",
            "Read a non-secret UTF-8 file in the workspace.",
            json!({
                "type": "object", "properties": { "path": { "type": "string" } },
                "required": ["path"], "additionalProperties": false
            }),
        ),
        definition(
            "write",
            "Write a UTF-8 file in the workspace.",
            json!({
                "type": "object", "properties": {
                    "path": { "type": "string" }, "content": { "type": "string" }
                }, "required": ["path", "content"], "additionalProperties": false
            }),
        ),
    ];
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions
}

fn definition(name: &str, description: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
    }
}

fn decode<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, ToolResult> {
    serde_json::from_value(input)
        .map_err(|error| ToolResult::error(format!("invalid tool input: {error}")))
}

fn memory_search(input: Value) -> ToolResult {
    let _: MemoryInput = match decode(input) {
        Ok(input) => input,
        Err(result) => return result,
    };
    ToolResult::error("memory_search is unavailable until milestone M2")
}

fn contains_glob_meta(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_glob(
    root: &Path,
    base: &Path,
    policy: &PathPolicy,
    matcher: &GlobMatcher,
) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for entry in WalkBuilder::new(base).hidden(false).build() {
        let entry = entry.map_err(|error| format!("glob failed: {error}"))?;
        let path = entry.path();
        if path == base || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if policy.resolve(path).is_err() {
            continue;
        }
        let relative_to_base = path.strip_prefix(base).unwrap_or(path);
        if matcher.is_match(relative_to_base) {
            paths.push(display_relative(root, path));
        }
    }
    paths.sort();
    Ok(paths)
}

fn collect_grep(
    root: &Path,
    base: &Path,
    policy: &PathPolicy,
    regex: &Regex,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    if base.is_file() {
        grep_file(root, base, regex, &mut lines)?;
        return Ok(lines);
    }
    for entry in WalkBuilder::new(base).hidden(false).build() {
        let entry = entry.map_err(|error| format!("grep failed: {error}"))?;
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) || policy.resolve(path).is_err() {
            continue;
        }
        grep_file(root, path, regex, &mut lines)?;
    }
    lines.sort();
    Ok(lines)
}

fn grep_file(
    root: &Path,
    path: &Path,
    regex: &Regex,
    output: &mut Vec<String>,
) -> Result<(), String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => return Ok(()),
        Err(error) => return Err(format!("grep failed reading {}: {error}", path.display())),
    };
    for (index, line) in content.lines().enumerate() {
        if regex.is_match(line) {
            output.push(format!(
                "{}:{}:{line}",
                display_relative(root, path),
                index + 1
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashInput {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryInput {
    #[allow(dead_code)]
    query: String,
}
