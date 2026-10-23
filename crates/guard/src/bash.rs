use crate::path::{built_in_secret_rule, credential_rule, home_harness_dirs, normalize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

/// Severity-ordered action for a shell command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Escalate,
    Deny,
}

impl Verdict {
    const fn severity(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Escalate => 1,
            Self::Deny => 2,
        }
    }
}

impl Ord for Verdict {
    fn cmp(&self, other: &Self) -> Ordering {
        self.severity().cmp(&other.severity())
    }
}

impl PartialOrd for Verdict {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashDecision {
    pub verdict: Verdict,
    pub rule: &'static str,
}

/// A deterministic full-tree shell classifier.
///
/// Every `command` node is inspected, including nodes nested under lists, pipelines,
/// subshells, and command substitutions. The maximum severity wins. This is a safety gate,
/// not a sandbox: aliases, runtime-generated command names, unusual quoting, and arbitrary
/// interpreter programs can evade static classification and remain documented known gaps.
/// It reduces accidental reads of secrets and harness state; it is not a sandbox.
#[derive(Debug, Default, Clone, Copy)]
pub struct BashClassifier;

impl BashClassifier {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn classify(&self, source: &str) -> BashDecision {
        self.classify_in(source, Path::new("."))
    }

    /// Classifies shell paths relative to the executor's workspace, including symlinks.
    #[must_use]
    pub fn classify_in(&self, source: &str, root: &Path) -> BashDecision {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .is_err()
        {
            return BashDecision {
                verdict: Verdict::Deny,
                rule: "parser-unavailable",
            };
        }
        let Some(tree) = parser.parse(source, None) else {
            return BashDecision {
                verdict: Verdict::Deny,
                rule: "parse-failed",
            };
        };
        if tree.root_node().has_error() {
            return BashDecision {
                verdict: Verdict::Escalate,
                rule: "shell-parse-error",
            };
        }

        let mut decisions = Vec::new();
        collect_decisions(tree.root_node(), source.as_bytes(), root, &mut decisions);
        if decisions.is_empty() {
            return BashDecision {
                verdict: Verdict::Escalate,
                rule: "no-static-command",
            };
        }
        decisions
            .into_iter()
            .max_by_key(|decision| decision.verdict)
            .unwrap_or(BashDecision {
                verdict: Verdict::Escalate,
                rule: "no-static-command",
            })
    }

    /// Static limitations, including runtime paths now enforced by the OS sandbox.
    #[must_use]
    pub const fn known_gaps() -> &'static [&'static str] {
        &[
            "aliases and shell functions resolved at runtime",
            "commands assembled through variable expansion or eval",
            "obfuscated command names and interpreter payloads",
            "filesystem races after classification",
            "shell variables naming a path (`D=.elefante; cat $D/memory.db`): enforced by the OS sandbox",
            "`cd` into a protected directory, then a relative path: enforced by the OS sandbox",
            "interpreters and `sqlite3` reading a path assembled at runtime: enforced by the OS sandbox",
            "`xargs` or command substitution producing a path: enforced by the OS sandbox",
        ]
    }
}

fn collect_decisions(
    node: Node<'_>,
    source: &[u8],
    root: &Path,
    decisions: &mut Vec<BashDecision>,
) {
    if node.kind() == "command" {
        let text = node.utf8_text(source).unwrap_or_default();
        decisions.push(classify_command(text, root));
    } else if node.kind() == "file_redirect"
        && let Some(decision) = classify_file_redirect(node, source, root)
    {
        decisions.push(decision);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_decisions(child, source, root, decisions);
    }
}

fn classify_file_redirect(node: Node<'_>, source: &[u8], root: &Path) -> Option<BashDecision> {
    if node
        .child_by_field_name("destination")
        .and_then(|destination| destination.utf8_text(source).ok())
        .is_some_and(|value| looks_like_secret_path(value.trim_matches(['\'', '"']), root))
    {
        return Some(BashDecision {
            verdict: Verdict::Deny,
            rule: "secret-path-redirect",
        });
    }

    let mut cursor = node.walk();
    let writes = node
        .children(&mut cursor)
        .any(|child| matches!(child.kind(), ">" | ">>" | "&>" | "&>>" | ">&" | ">|" | "<>"));
    writes.then_some(BashDecision {
        verdict: Verdict::Escalate,
        rule: "write-redirection",
    })
}

fn classify_command(command: &str, root: &Path) -> BashDecision {
    let tokens = static_tokens(command);
    let Some(program) = tokens.first().map(String::as_str) else {
        return BashDecision {
            verdict: Verdict::Escalate,
            rule: "dynamic-command",
        };
    };
    let basename = program.rsplit('/').next().unwrap_or(program);

    if is_destructive_program(basename) {
        return BashDecision {
            verdict: Verdict::Deny,
            rule: "destructive-command",
        };
    }
    if matches!(basename, "env" | "printenv" | "set") {
        return BashDecision {
            verdict: Verdict::Deny,
            rule: "environment-disclosure",
        };
    }
    // Directory navigation reads no content. The sandbox enforces the destination even
    // when the path is literal harness state; subsequent content readers are still checked.
    if basename == "cd" && tokens.len() <= 2 {
        return BashDecision {
            verdict: Verdict::Allow,
            rule: "directory-navigation",
        };
    }
    // Every program can transform or forward secrets; restricting this check to `cat`
    // allows `sed`, `head`, or an interpreter to evade output scrubbing.
    if checked_arguments(basename, &tokens).any(|arg| looks_like_secret_path(arg, root)) {
        return BashDecision {
            verdict: Verdict::Deny,
            rule: "secret-path",
        };
    }
    if basename == "git" {
        return classify_git(&tokens);
    }
    if basename == "cargo" {
        return classify_cargo(&tokens);
    }
    if basename == "find" && tokens.iter().any(|token| token == "-delete") {
        return BashDecision {
            verdict: Verdict::Deny,
            rule: "destructive-find",
        };
    }
    if basename == "find"
        && tokens
            .iter()
            .any(|token| matches!(token.as_str(), "-exec" | "-execdir"))
    {
        return BashDecision {
            verdict: Verdict::Escalate,
            rule: "executing-find",
        };
    }
    if basename == "sed"
        && tokens
            .iter()
            .any(|token| token == "-i" || token.starts_with("--in-place"))
    {
        return BashDecision {
            verdict: Verdict::Escalate,
            rule: "in-place-sed",
        };
    }
    if (basename == "sqlite3"
        && tokens.len() == 3
        && !tokens[1].starts_with('-')
        && tokens[2] == ".schema")
        || (basename == "xargs" && tokens.len() == 2 && tokens[1] == "cat")
    {
        return BashDecision {
            verdict: Verdict::Allow,
            rule: "read-only-runtime-path",
        };
    }
    if is_read_only_program(basename) {
        return BashDecision {
            verdict: Verdict::Allow,
            rule: "read-only-allowlist",
        };
    }
    if matches!(
        basename,
        "sudo" | "su" | "dd" | "chmod" | "chown" | "kill" | "pkill" | "killall"
    ) {
        return BashDecision {
            verdict: Verdict::Deny,
            rule: "privileged-or-destructive-command",
        };
    }
    BashDecision {
        verdict: Verdict::Escalate,
        rule: "unclassified-command",
    }
}

fn is_destructive_program(program: &str) -> bool {
    matches!(
        program,
        "rm" | "rmdir"
            | "shred"
            | "unlink"
            | "mkfs"
            | "fdisk"
            | "shutdown"
            | "reboot"
            | "poweroff"
            | "halt"
    ) || program.starts_with("mkfs.")
}

fn is_read_only_program(program: &str) -> bool {
    matches!(
        program,
        "pwd"
            | "ls"
            | "echo"
            | "printf"
            | "true"
            | "false"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "diff"
            | "sed"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "find"
            | "which"
            | "type"
            | "file"
            | "stat"
    )
}

fn classify_git(tokens: &[String]) -> BashDecision {
    let subcommand = tokens
        .iter()
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .map(String::as_str);
    if matches!(
        subcommand,
        Some("status" | "diff" | "log" | "show" | "branch" | "rev-parse" | "ls-files")
    ) {
        BashDecision {
            verdict: Verdict::Allow,
            rule: "read-only-git",
        }
    } else if matches!(subcommand, Some("reset" | "clean")) {
        BashDecision {
            verdict: Verdict::Deny,
            rule: "destructive-git",
        }
    } else {
        BashDecision {
            verdict: Verdict::Escalate,
            rule: "mutating-or-unknown-git",
        }
    }
}

fn classify_cargo(tokens: &[String]) -> BashDecision {
    let subcommand = tokens
        .iter()
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .map(String::as_str);
    if matches!(
        subcommand,
        Some("check" | "test" | "clippy" | "fmt" | "metadata" | "tree")
    ) {
        BashDecision {
            verdict: Verdict::Allow,
            rule: "cargo-development-command",
        }
    } else {
        BashDecision {
            verdict: Verdict::Escalate,
            rule: "mutating-or-unknown-cargo",
        }
    }
}

fn static_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quote != Some('\'') => escaped = true,
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(character),
            c if c.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    while tokens
        .first()
        .is_some_and(|token| token.contains('=') && !token.starts_with('='))
    {
        tokens.remove(0);
    }
    tokens
}

/// The arguments to test for secret paths. `grep --exclude-dir=.elefante` names the directory
/// only to keep a recursive search out of it, which is the safe form, so that value is skipped.
fn checked_arguments<'a>(
    basename: &str,
    tokens: &'a [String],
) -> impl Iterator<Item = &'a String> + use<'a> {
    let grep = matches!(basename, "grep" | "egrep" | "fgrep");
    let mut skip_next = false;
    tokens.iter().skip(1).filter(move |token| {
        if std::mem::take(&mut skip_next) {
            return false;
        }
        if grep && token.as_str() == "--exclude-dir" {
            skip_next = true;
            return false;
        }
        !(grep && token.starts_with("--exclude-dir="))
    })
}

fn expand_home(value: &str) -> Option<PathBuf> {
    let suffix = ["~", "$HOME", "${HOME}"].into_iter().find_map(|prefix| {
        let rest = value.strip_prefix(prefix)?;
        if rest.is_empty() {
            Some("")
        } else {
            rest.strip_prefix('/')
        }
    })?;
    std::env::var_os("HOME").map(|home| Path::new(&home).join(suffix))
}

fn absolute_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn looks_like_secret_path(value: &str, root: &Path) -> bool {
    // Check both a raw argument and the value of an option such as `--file=path`.
    let value = value.split_once('=').map_or(value, |(_, value)| value);
    let path = Path::new(value);
    let home_expanded = expand_home(value);
    // As typed, a relative argument is relative to the workspace root, so the component rule
    // applies to it directly.
    if home_expanded.is_none() && !path.is_absolute() && built_in_secret_rule(path).is_some() {
        return true;
    }
    let root = absolute_root(root);
    let target = normalize(&home_expanded.unwrap_or_else(|| root.join(path)));
    is_secret_target(&target, &root)
        || target.canonicalize().is_ok_and(|canonical| {
            is_secret_target(&canonical, &root)
                || crate::path::canonical_credentials().as_ref() == Some(&canonical)
        })
}

/// `target` is absolute. The harness-state rule applies to the part below the workspace root, so
/// a workspace under some `.elefante/` still works; the user-level directory is denied by prefix.
fn is_secret_target(target: &Path, root: &Path) -> bool {
    credential_rule(target).is_some()
        || home_harness_dirs()
            .iter()
            .any(|dir| target.starts_with(dir))
        || target
            .strip_prefix(root)
            .is_ok_and(|relative| built_in_secret_rule(relative).is_some())
}
