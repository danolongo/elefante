//! Literal local git writes and pure, environment-cleared invocation descriptions.
use crate::PathPolicy;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

const REFUSAL: &str = "run git write commands on their own with supported literal arguments";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRoute {
    NotHarness,
    Harness(GitWrite),
    Refused(&'static str),
}

/// Validated arguments; construction is restricted to the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWrite {
    args: Vec<String>,
}

impl GitWrite {
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// The caller must clear the child environment before applying `environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
}

/// Subcommands that can change the repository. Every other subcommand is a read.
const WRITE_SUBCOMMANDS: &[&str] = &[
    "add",
    "rm",
    "mv",
    "restore",
    "commit",
    "switch",
    "checkout",
    "branch",
    "stash",
    "tag",
    "merge",
    "cherry-pick",
    "rebase",
    "reset",
    "clean",
    "push",
    "fetch",
    "pull",
    "config",
];

/// Commands that run their arguments as another command.
const WRAPPERS: &[&str] = &[
    "env", "command", "exec", "nice", "nohup", "time", "timeout", "xargs", "sudo", "builtin",
];

/// Reports whether `words`, the words after `git`, form a read. A read is a subcommand outside
/// `WRITE_SUBCOMMANDS`, or a listing form of `branch`, `tag`, `stash` or `config`.
pub(crate) fn is_read(words: &[String]) -> bool {
    let Some((subcommand, args)) = words.split_first() else {
        return false;
    };
    let first = args.first().map(String::as_str);
    match subcommand.as_str() {
        "branch" | "tag" => args
            .iter()
            .all(|arg| matches!(arg.as_str(), "-a" | "-l" | "--list" | "-v")),
        "stash" => matches!(first, Some("list" | "show")),
        "config" => matches!(first, Some("--get" | "--get-all" | "--list" | "-l")),
        "--version" => args.is_empty(),
        other => !other.starts_with('-') && !WRITE_SUBCOMMANDS.contains(&other),
    }
}

fn refuse_if(write: bool) -> GitRoute {
    if write {
        GitRoute::Refused(REFUSAL)
    } else {
        GitRoute::NotHarness
    }
}

/// Routes only whole, static, single git commands. Reads stay in the shell sandbox.
#[must_use]
pub fn route(source: &str, root: &Path) -> GitRoute {
    let Some(words) = literal_words(source) else {
        return refuse_if(contains_write(source));
    };
    if words.first().is_none_or(|word| word != "git") {
        return refuse_if(contains_write(source));
    }
    if !source.trim_start_matches([' ', '\t']).starts_with("git ")
        && !source.trim_start_matches([' ', '\t']).starts_with("git\t")
        && source.trim() != "git"
    {
        return GitRoute::Refused(REFUSAL);
    }
    let rest = &words[1..];
    if rest.is_empty() || is_read(rest) {
        return GitRoute::NotHarness;
    }
    if valid_write(&rest[0], &rest[1..], root) {
        GitRoute::Harness(GitWrite {
            args: rest.to_vec(),
        })
    } else {
        refuse_if(
            rest.iter()
                .any(|word| WRITE_SUBCOMMANDS.contains(&word.as_str())),
        )
    }
}

fn contains_write(source: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return true;
    }
    let Some(tree) = parser.parse(source, None) else {
        return true;
    };
    command_contains_write(tree.root_node(), source)
}

fn command_contains_write(node: tree_sitter::Node<'_>, source: &str) -> bool {
    if node.kind() == "command" {
        let raw_name = node
            .child_by_field_name("name")
            .map(|name| &source[name.byte_range()])
            .unwrap_or_default();
        let name = single_word(raw_name)
            .unwrap_or_else(|| raw_name.trim_matches(['"', '\'', '$']).to_owned());
        let wrapped = WRAPPERS.contains(&name.as_str());
        if name == "git"
            || name.ends_with("/git")
            || wrapped
            || raw_name.contains('$')
            || raw_name.replace(['\\', '\n'], "") == "git"
        {
            let mut cursor = node.walk();
            let mut arguments = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "command_name" | "variable_assignment"))
                .map(|child| single_word(&source[child.byte_range()]))
                .collect::<Vec<_>>();
            if wrapped {
                if let Some(git) = arguments.iter().position(|arg| {
                    arg.as_ref()
                        .is_some_and(|word| word == "git" || word.ends_with("/git"))
                }) {
                    arguments.drain(..=git);
                } else {
                    arguments.clear();
                }
            }
            let read = arguments
                .iter()
                .cloned()
                .collect::<Option<Vec<_>>>()
                .is_some_and(|words| is_read(&words));
            if !read
                && arguments
                    .iter()
                    .flatten()
                    .any(|word| WRITE_SUBCOMMANDS.contains(&word.as_str()))
            {
                return true;
            }
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| command_contains_write(child, source))
}

/// Returns the one literal word in `source`, or `None` for anything else.
fn single_word(source: &str) -> Option<String> {
    literal_words(source)
        .filter(|words| words.len() == 1)
        .map(|mut words| words.swap_remove(0))
}

fn literal_words(source: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in source.trim_matches([' ', '\t']).chars() {
        if c == '\'' {
            quoted = !quoted;
            started = true;
            continue;
        }
        if !quoted && matches!(c, ' ' | '\t') {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
            continue;
        }
        if c.is_control()
            || (!quoted
                && matches!(
                    c,
                    '$' | '"'
                        | '\\'
                        | '~'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '('
                        | ')'
                        | '<'
                        | '>'
                        | '|'
                        | ';'
                        | '&'
                        | '`'
                        | '#'
                ))
        {
            return None;
        }
        word.push(c);
        started = true;
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn valid_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
}

fn valid_write(sub: &str, args: &[String], root: &Path) -> bool {
    match sub {
        "add" | "rm" | "mv" | "restore" => valid_paths(sub, args, root),
        "switch" => one_ref(args) || flag_ref(args, "-c"),
        // A ref that names a file would read as a pathspec and discard that file's changes.
        "checkout" => {
            (one_ref(args) && root.join(&args[0]).symlink_metadata().is_err())
                || flag_ref(args, "-b")
        }
        "branch" => one_ref(args) || flag_ref(args, "-d"),
        "stash" => match args.first().map(String::as_str) {
            Some("pop" | "apply") => args.len() == 1,
            Some("push") => args.len() == 1 || (args.len() == 3 && args[1] == "-m"),
            _ => false,
        },
        "tag" => {
            one_ref(args)
                || (args.len() == 4 && args[0] == "-a" && valid_ref(&args[1]) && args[2] == "-m")
        }
        "commit" => options_and_refs(args, &["-a", "--amend", "--no-edit"], true, false),
        "merge" => {
            control(args, &["--abort"])
                || options_and_refs(args, &["--no-ff", "--ff-only"], false, true)
        }
        "cherry-pick" | "rebase" => one_ref(args) || control(args, &["--abort", "--continue"]),
        _ => false,
    }
}

fn one_ref(args: &[String]) -> bool {
    args.len() == 1 && valid_ref(&args[0])
}
fn flag_ref(args: &[String], flag: &str) -> bool {
    args.len() == 2 && args[0] == flag && valid_ref(&args[1])
}
fn control(args: &[String], flags: &[&str]) -> bool {
    args.len() == 1 && flags.contains(&args[0].as_str())
}

fn options_and_refs(
    args: &[String],
    flags: &[&str],
    require_message: bool,
    require_ref: bool,
) -> bool {
    let mut message = false;
    let mut refs = 0;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-m" {
            if iter.next().is_none() {
                return false;
            }
            message = true;
        } else if flags.contains(&arg.as_str()) {
        } else if require_ref && valid_ref(arg) {
            refs += 1;
        } else {
            return false;
        }
    }
    (!require_message || message) && (!require_ref || refs == 1)
}

fn valid_paths(sub: &str, args: &[String], root: &Path) -> bool {
    let Ok(policy) = PathPolicy::new(root) else {
        return false;
    };
    let mut paths = 0;
    let mut positional = false;
    for arg in args {
        if !positional && arg == "--" && sub != "mv" {
            positional = true;
            continue;
        }
        let flag = match sub {
            "add" => matches!(arg.as_str(), "-A" | "-u"),
            "rm" => matches!(arg.as_str(), "--cached" | "-r"),
            "restore" => matches!(arg.as_str(), "--staged" | "--worktree"),
            _ => false,
        };
        if !positional && flag {
            continue;
        }
        if (!positional && arg.starts_with('-'))
            || arg.is_empty()
            || arg.starts_with(':')
            || arg.contains(['*', '?', '['])
            || policy.resolve_for_write(arg).is_err()
        {
            return false;
        }
        paths += 1;
    }
    match sub {
        "add" => paths > 0 || args.iter().any(|a| matches!(a.as_str(), "-A" | "-u")),
        "mv" => paths == 2,
        _ => paths > 0,
    }
}

/// Base invocation for trusted read-only consumers, with no inherited environment.
///
/// # Panics
/// Panics if `git` is not an absolute executable path.
#[must_use]
pub fn hardened(git: &Path, root: &Path) -> Invocation {
    assert!(git.is_absolute(), "git executable must be absolute");
    let settings = [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "core.attributesFile=/dev/null",
        "core.pager=cat",
        "core.editor=false",
        "sequence.editor=false",
        "protocol.allow=never",
        "credential.helper=",
        "commit.gpgSign=false",
        "tag.gpgSign=false",
        "gpg.program=false",
        "gpg.ssh.program=false",
        "core.sshCommand=false",
        "diff.external=",
        "merge.default=text",
    ];
    let args = settings
        .into_iter()
        .flat_map(|setting| [OsString::from("-c"), OsString::from(setting)])
        .collect();
    let environment = [
        ("PATH", "/usr/bin:/bin"),
        ("LANG", "C.UTF-8"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS", "1"),
        ("GIT_OPTIONAL_LOCKS", "0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .chain([(
        OsString::from("GIT_CEILING_DIRECTORIES"),
        // Git does not apply a ceiling equal to its initial directory. Its parent
        // stops discovery before that parent while still admitting root/.git.
        root.parent().unwrap_or(root).as_os_str().to_owned(),
    )])
    .collect();
    Invocation {
        program: git.to_owned(),
        args,
        cwd: root.to_owned(),
        environment,
    }
}

/// Adds validated write arguments and explicit author/committer identity.
///
/// # Panics
/// Panics if `git` is not an absolute executable path.
#[must_use]
pub fn invocation(git: &Path, root: &Path, write: &GitWrite, identity: &Identity) -> Invocation {
    let mut invocation = hardened(git, root);
    invocation
        .args
        .extend(write.args.iter().map(OsString::from));
    // A trailing `--` makes git read `checkout <ref>` as a tree-ish, never as a pathspec.
    if write.args.len() == 2 && write.args[0] == "checkout" {
        invocation.args.push("--".into());
    }
    for prefix in ["GIT_AUTHOR", "GIT_COMMITTER"] {
        invocation.environment.insert(
            format!("{prefix}_NAME").into(),
            identity.name.clone().into(),
        );
        invocation.environment.insert(
            format!("{prefix}_EMAIL").into(),
            identity.email.clone().into(),
        );
    }
    invocation
}
