use crate::path::{built_in_secret_rule, credential_rule, home_harness_dirs, normalize};
use globset::Glob;
use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};
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

    /// Explicit limitations used by callers and tests to avoid overstating this gate.
    #[must_use]
    pub const fn known_gaps() -> &'static [&'static str] {
        &[
            "aliases and shell functions resolved at runtime",
            "commands assembled through variable expansion or eval",
            "obfuscated command names and interpreter payloads",
            "filesystem races after classification",
            "shell variables naming a path (`D=.elefante; cat $D/memory.db`)",
            "`cd` into a protected directory, then a relative path",
            "interpreters and `sqlite3` reading a path assembled at runtime",
            "`xargs` or command substitution producing a path",
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
    if searches_harness_state(basename, &tokens, root) {
        return BashDecision {
            verdict: Verdict::Escalate,
            rule: "recursive-search-includes-harness-state",
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
    if names_harness_dir_by_glob(value) {
        return true;
    }
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

/// Whether an argument containing glob characters can expand to `.elefante`.
///
/// Every leading-dot component with a literal character after the dot is tested
/// (`.elef*`, `.[e]lefante`). A bare `.*` is only tested as the argument's first component and
/// only when more components follow (`.*/memory.db`), because `.*` as a regex or a `sed`
/// script is common and would otherwise deny ordinary commands.
fn names_harness_dir_by_glob(value: &str) -> bool {
    if !value.contains(['*', '?', '[']) {
        return false;
    }
    let components = Path::new(value)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    components.iter().enumerate().any(|(index, component)| {
        let Some(rest) = component.strip_prefix('.') else {
            return false;
        };
        if !component.contains(['*', '?', '[']) {
            return false;
        }
        let bare = rest.chars().all(|c| matches!(c, '*' | '?' | '.'));
        if bare && (index != 0 || components.len() < 2) {
            return false;
        }
        Glob::new(component).is_ok_and(|glob| glob.compile_matcher().is_match(".elefante"))
    })
}

#[derive(Default)]
struct ScannedArgs {
    short: Vec<(char, Option<String>)>,
    long: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
}

impl ScannedArgs {
    fn has_short(&self, flag: char) -> bool {
        self.short.iter().any(|(c, _)| *c == flag)
    }

    fn has_long(&self, name: &str) -> bool {
        self.long.iter().any(|(n, _)| n == name)
    }

    fn long_value(&self, name: &str) -> Option<&str> {
        self.long
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, value)| value.as_deref())
    }
}

/// Splits arguments into short flags, long flags and positionals. `short_with_arg` lists the
/// short options that take a value; `long_with_arg` the long ones that take it as the next token.
fn scan_args(tokens: &[String], short_with_arg: &str, long_with_arg: &[&str]) -> ScannedArgs {
    let mut scanned = ScannedArgs::default();
    let mut iter = tokens.iter().skip(1);
    while let Some(token) = iter.next() {
        if token == "--" {
            scanned.positionals.extend(iter.by_ref().cloned());
        } else if let Some(long) = token.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None if long_with_arg.contains(&long) => (long, iter.next().cloned()),
                None => (long, None),
            };
            scanned.long.push((name.to_owned(), value));
        } else if token.len() > 1 && token.starts_with('-') {
            let mut chars = token[1..].chars();
            while let Some(flag) = chars.next() {
                if short_with_arg.contains(flag) {
                    let rest = chars.as_str();
                    let value = if rest.is_empty() {
                        iter.next().cloned()
                    } else {
                        Some(rest.to_owned())
                    };
                    scanned.short.push((flag, value));
                    break;
                }
                scanned.short.push((flag, None));
            }
        } else {
            scanned.positionals.push(token.clone());
        }
    }
    scanned
}

const GREP_SHORT_WITH_ARG: &str = "efmABCdD";
const GREP_LONG_WITH_ARG: &[&str] = &[
    "regexp",
    "file",
    "max-count",
    "after-context",
    "before-context",
    "context",
    "directories",
    "devices",
    "exclude",
    "exclude-dir",
    "exclude-from",
    "include",
    "label",
    "binary-files",
];
const RG_SHORT_WITH_ARG: &str = "efgtTmABCjMrE";
const RG_LONG_WITH_ARG: &[&str] = &[
    "regexp",
    "file",
    "glob",
    "iglob",
    "type",
    "type-not",
    "type-add",
    "max-depth",
    "max-count",
    "max-filesize",
    "max-columns",
    "replace",
    "after-context",
    "before-context",
    "context",
    "threads",
    "ignore-file",
    "pre",
    "pre-glob",
    "engine",
    "encoding",
    "sort",
    "sortr",
    "colors",
];

/// A grep that recurses and was not told to leave `.elefante` out.
fn grep_recurses_into_state(scanned: &ScannedArgs) -> bool {
    let recursive = scanned.has_short('r')
        || scanned.has_short('R')
        || scanned
            .short
            .iter()
            .any(|(c, value)| *c == 'd' && value.as_deref() == Some("recurse"))
        || scanned.has_long("recursive")
        || scanned.has_long("dereference-recursive")
        || scanned.long_value("directories") == Some("recurse");
    let excluded = scanned.long.iter().any(|(name, value)| {
        name == "exclude-dir"
            && value
                .as_deref()
                .is_some_and(|v| v.trim_end_matches('/') == ".elefante")
    });
    recursive && !excluded
}

/// A ripgrep-style search that turned off the ignore rules that hide `.elefante`.
fn rg_ignores_state_rules(scanned: &ScannedArgs) -> bool {
    scanned.has_short('u')
        || scanned.has_short('.')
        || scanned.has_long("hidden")
        || scanned.has_long("unrestricted")
        || scanned
            .long
            .iter()
            .any(|(name, _)| name.starts_with("no-ignore"))
}

/// Recursive content readers over the workspace root also read `.elefante/`, whose name never
/// appears in the command. Held for approval rather than denied, like other indeterminate commands.
fn searches_harness_state(basename: &str, tokens: &[String], root: &Path) -> bool {
    let (scanned, reaches_state) = match basename {
        "grep" | "egrep" | "fgrep" => {
            let scanned = scan_args(tokens, GREP_SHORT_WITH_ARG, GREP_LONG_WITH_ARG);
            let reaches = grep_recurses_into_state(&scanned);
            (scanned, reaches)
        }
        "rg" | "ag" | "ack" => {
            let scanned = scan_args(tokens, RG_SHORT_WITH_ARG, RG_LONG_WITH_ARG);
            let reaches = rg_ignores_state_rules(&scanned);
            (scanned, reaches)
        }
        _ => return false,
    };
    if !reaches_state {
        return false;
    }
    let pattern_by_option = scanned.has_short('e')
        || scanned.has_short('f')
        || scanned.has_long("regexp")
        || scanned.has_long("file");
    let paths = if pattern_by_option {
        &scanned.positionals[..]
    } else {
        scanned.positionals.get(1..).unwrap_or_default()
    };
    paths.is_empty() || {
        let root = absolute_root(root);
        paths.iter().any(|path| {
            let target = normalize(&expand_home(path).unwrap_or_else(|| root.join(path)));
            let target = target.canonicalize().unwrap_or(target);
            // The root itself, or any directory above it (or above the user-level harness
            // directory), contains `.elefante`.
            root.starts_with(&target)
                || home_harness_dirs()
                    .iter()
                    .any(|dir| dir.starts_with(&target))
        })
    }
}
