use std::cmp::Ordering;
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
#[derive(Debug, Default, Clone, Copy)]
pub struct BashClassifier;

impl BashClassifier {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn classify(&self, source: &str) -> BashDecision {
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
        collect_decisions(tree.root_node(), source.as_bytes(), &mut decisions);
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

    /// Explicit limitations used by callers and tests to avoid overstating this gate.
    #[must_use]
    pub const fn known_gaps() -> &'static [&'static str] {
        &[
            "aliases and shell functions resolved at runtime",
            "commands assembled through variable expansion or eval",
            "obfuscated command names and interpreter payloads",
            "filesystem races after classification",
        ]
    }
}

fn collect_decisions(node: Node<'_>, source: &[u8], decisions: &mut Vec<BashDecision>) {
    if node.kind() == "command" {
        let text = node.utf8_text(source).unwrap_or_default();
        decisions.push(classify_command(text));
    } else if node.kind() == "file_redirect"
        && let Some(decision) = classify_file_redirect(node, source)
    {
        decisions.push(decision);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_decisions(child, source, decisions);
    }
}

fn classify_file_redirect(node: Node<'_>, source: &[u8]) -> Option<BashDecision> {
    if node
        .child_by_field_name("destination")
        .and_then(|destination| destination.utf8_text(source).ok())
        .is_some_and(looks_like_secret_path)
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

fn classify_command(command: &str) -> BashDecision {
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
    if matches!(basename, "cat" | "source" | ".")
        && tokens.iter().skip(1).any(|arg| looks_like_secret_path(arg))
    {
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

fn looks_like_secret_path(value: &str) -> bool {
    value.split('/').any(|part| {
        part == ".env"
            || part.starts_with(".env.")
            || part == ".netrc"
            || part == ".npmrc"
            || part == ".git-credentials"
            || part.starts_with("id_rsa")
            || std::path::Path::new(part)
                .extension()
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("pem") || extension.eq_ignore_ascii_case("key")
                })
    }) || value.contains(".ssh/")
        || value.ends_with(".aws/credentials")
}
