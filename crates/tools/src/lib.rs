//! The permanent M0 tool surface.

use elefante_guard::{BashClassifier, PathPolicy, Scrubber, Verdict};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;

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
    bash_classifier: BashClassifier,
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
            bash_classifier: BashClassifier::new(),
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
        let decision = self.bash_classifier.classify(&input.command);
        match decision.verdict {
            Verdict::Deny => {
                return ToolResult::error(format!("bash denied by policy: {}", decision.rule));
            }
            Verdict::Escalate => {
                // M0 has no approval transport, so escalation is deliberately non-executing.
                return ToolResult::error(format!(
                    "bash requires approval (unavailable in M0): {}",
                    decision.rule
                ));
            }
            Verdict::Allow => {}
        }
        let output = match Command::new("bash")
            .arg("-lc")
            .arg(&input.command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn executor() -> (tempfile::TempDir, ToolExecutor) {
        let temp = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(temp.path()).unwrap();
        (temp, executor)
    }

    #[test]
    fn definitions_are_stable_sorted_and_strict() {
        let first = definitions();
        let second = definitions();
        assert_eq!(first, second);
        let names = first
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "bash",
                "edit",
                "glob",
                "grep",
                "memory_search",
                "read",
                "write"
            ]
        );
        assert!(
            first
                .iter()
                .all(|definition| definition.input_schema["additionalProperties"] == false)
        );
    }

    #[tokio::test]
    async fn write_read_and_exact_edit_work() {
        let (temp, executor) = executor();
        assert!(
            !executor
                .execute("write", json!({"path": "src/a.txt", "content": "one two"}))
                .await
                .is_error
        );
        assert_eq!(
            executor
                .execute("read", json!({"path": "src/a.txt"}))
                .await
                .content,
            "one two"
        );
        assert!(
            !executor
                .execute(
                    "edit",
                    json!({
                        "path": "src/a.txt", "old_string": "one", "new_string": "three"
                    })
                )
                .await
                .is_error
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("src/a.txt")).unwrap(),
            "three two"
        );
    }

    #[tokio::test]
    async fn edit_rejects_missing_and_ambiguous_matches_without_writing() {
        let (temp, executor) = executor();
        fs::write(temp.path().join("a.txt"), "same same").unwrap();
        for old_string in ["absent", "same"] {
            let result = executor
                .execute(
                    "edit",
                    json!({
                        "path": "a.txt", "old_string": old_string, "new_string": "changed"
                    }),
                )
                .await;
            assert!(result.is_error);
            assert_eq!(
                fs::read_to_string(temp.path().join("a.txt")).unwrap(),
                "same same"
            );
        }
    }

    #[tokio::test]
    async fn all_filesystem_tools_enforce_secret_policy() {
        let (temp, executor) = executor();
        fs::write(
            temp.path().join(".env"),
            "API_TOKEN=sk-ant-api03_abcdefghijklmnopqrstuvwxyz",
        )
        .unwrap();
        fs::write(temp.path().join("safe.txt"), "ordinary").unwrap();

        assert!(
            executor
                .execute("read", json!({"path": "x/../.env"}))
                .await
                .is_error
        );
        assert!(
            executor
                .execute("write", json!({"path": ".env.local", "content": "x"}))
                .await
                .is_error
        );
        assert!(
            executor
                .execute(
                    "edit",
                    json!({"path": ".env", "old_string": "x", "new_string": "y"})
                )
                .await
                .is_error
        );
        assert!(
            executor
                .execute("glob", json!({"pattern": ".env"}))
                .await
                .is_error
        );
        assert!(
            executor
                .execute("grep", json!({"pattern": "sk-ant", "path": ".env"}))
                .await
                .is_error
        );

        let broad_glob = executor.execute("glob", json!({"pattern": "*"})).await;
        assert!(!broad_glob.is_error);
        assert!(!broad_glob.content.contains(".env"));
        let broad_grep = executor
            .execute("grep", json!({"pattern": "sk-ant", "path": "."}))
            .await;
        assert!(!broad_grep.is_error);
        assert!(!broad_grep.content.contains("sk-ant"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_denies_symlink_to_secret() {
        use std::os::unix::fs::symlink;
        let (temp, executor) = executor();
        fs::write(temp.path().join(".env"), "secret").unwrap();
        symlink(temp.path().join(".env"), temp.path().join("alias")).unwrap();
        assert!(
            executor
                .execute("read", json!({"path": "alias"}))
                .await
                .is_error
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_does_not_follow_dangling_symlink_outside_workspace() {
        use std::os::unix::fs::symlink;

        let (temp, executor) = executor();
        let outside = tempfile::tempdir().unwrap();
        let missing_directory = outside.path().join("missing");
        symlink(&missing_directory, temp.path().join("link")).unwrap();

        let result = executor
            .execute(
                "write",
                json!({"path": "link/escaped.txt", "content": "must remain inside"}),
            )
            .await;
        assert!(result.is_error);
        assert!(!missing_directory.join("escaped.txt").exists());
        assert!(!missing_directory.exists());
    }

    #[tokio::test]
    async fn bash_gate_denies_and_escalates_without_execution() {
        let (temp, executor) = executor();
        for command in ["cat .env", "rm marker", "env"] {
            assert!(
                executor
                    .execute("bash", json!({"command": command}))
                    .await
                    .is_error
            );
        }
        let escalated = executor
            .execute("bash", json!({"command": "touch marker"}))
            .await;
        assert!(escalated.is_error);
        assert!(escalated.content.contains("approval"));
        assert!(!temp.path().join("marker").exists());

        let redirected = executor
            .execute("bash", json!({"command": "printf changed > redirected"}))
            .await;
        assert!(redirected.is_error);
        assert!(redirected.content.contains("approval"));
        assert!(!temp.path().join("redirected").exists());
    }

    #[tokio::test]
    async fn scrubber_runs_on_success_and_error_results() {
        let (_temp, executor) = executor();
        let result = executor
            .execute(
                "bash",
                json!({
                    "command": "printf 'sk-ant-api03_abcdefghijklmnopqrstuvwxyz'"
                }),
            )
            .await;
        assert!(!result.is_error);
        assert!(!result.content.contains("sk-ant-"));
        assert!(result.content.contains("[REDACTED]"));

        let error = executor
            .execute(
                "unknown",
                json!({
                    "value": "API_TOKEN=sk-ant-api03_abcdefghijklmnopqrstuvwxyz"
                }),
            )
            .await;
        assert!(error.is_error);
        assert!(!error.content.contains("sk-ant-"));
    }

    #[tokio::test]
    async fn glob_and_grep_are_deterministic() {
        let (temp, executor) = executor();
        fs::write(temp.path().join("b.txt"), "needle b").unwrap();
        fs::write(temp.path().join("a.txt"), "needle a").unwrap();
        assert_eq!(
            executor
                .execute("glob", json!({"pattern": "*.txt"}))
                .await
                .content,
            "a.txt\nb.txt"
        );
        assert_eq!(
            executor
                .execute("grep", json!({"pattern": "needle"}))
                .await
                .content,
            "a.txt:1:needle a\nb.txt:1:needle b"
        );
    }

    #[tokio::test]
    async fn memory_search_is_a_deterministic_m2_stub() {
        let (_temp, executor) = executor();
        let first = executor
            .execute("memory_search", json!({"query": "anything"}))
            .await;
        let second = executor
            .execute("memory_search", json!({"query": "anything"}))
            .await;
        assert_eq!(first, second);
        assert!(first.is_error);
        assert_eq!(
            first.content,
            "memory_search is unavailable until milestone M2"
        );
    }

    #[tokio::test]
    async fn cloned_executor_supports_parallel_calls() {
        let (temp, executor) = executor();
        fs::write(temp.path().join("a.txt"), "alpha").unwrap();
        fs::write(temp.path().join("b.txt"), "beta").unwrap();
        let left = executor.clone();
        let right = executor.clone();
        let (a, b) = tokio::join!(
            left.execute("read", json!({"path": "a.txt"})),
            right.execute("read", json!({"path": "b.txt"}))
        );
        assert_eq!(a, ToolResult::success("alpha"));
        assert_eq!(b, ToolResult::success("beta"));
    }
}
