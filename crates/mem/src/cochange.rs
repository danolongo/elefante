//! Files that change together. See `docs/PLAN.md` § Memory subsystem, and this crate's README for the contract.
//!
//! Coupling that imports miss — a handler and its schema, a test and its fixture — is visible in
//! history for the price of one `git log`. Everything here is a pure function of that log except
//! the subprocess runner, so the weights a fixture repository produces can be computed by hand.

use std::{
    collections::{HashMap, HashSet},
    hash::BuildHasher,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const MAX_COMMITS: usize = 500;
/// A commit touching more files than this pairs everything with everything and says nothing about
/// coupling: a mass reformat, a lockfile bump, a vendor import.
pub const MAX_COMMIT_FILES: usize = 50;
/// A pair seen once is coincidence.
pub const MIN_SUPPORT: u32 = 2;
/// Each file keeps its heaviest edges; an edge survives if it is in either endpoint's list.
pub const TOP_K: usize = 20;
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// A repository with enormous commits could emit hundreds of megabytes. Past this, the newest
/// commits already read are used and the rest ignored.
pub const MAX_LOG_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, Debug)]
pub struct CoChangeConfig {
    pub max_commits: usize,
    pub max_commit_files: usize,
    pub min_support: u32,
    pub top_k: usize,
}

impl Default for CoChangeConfig {
    fn default() -> Self {
        Self {
            max_commits: MAX_COMMITS,
            max_commit_files: MAX_COMMIT_FILES,
            min_support: MIN_SUPPORT,
            top_k: TOP_K,
        }
    }
}

/// One path in a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Added, modified, deleted, copied-to, or type-changed.
    Touch(String),
    /// Renamed. History before this commit refers to `from`, which is now `to`.
    Rename { from: String, to: String },
}

/// One commit's changes. Commits are held newest first, as `git log` emits them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    pub changes: Vec<Change>,
}

fn is_hash(token: &str) -> bool {
    matches!(token.len(), 40 | 64) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses `git log --name-status -M -z --format=%x00%H` output.
///
/// With `-z` every field is NUL-terminated, a commit starts with an empty token followed by its
/// hash, and the first status after a hash carries a stray leading newline. So it is tokenised on
/// NUL rather than split on lines. A rename or copy carries two paths, which is exactly what a
/// `--name-only` parser would lose. Paths that are not valid UTF-8 are skipped.
#[must_use]
pub fn parse_log(raw: &[u8]) -> Vec<Commit> {
    let tokens: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    let text = |token: &[u8]| String::from_utf8(token.to_vec()).ok();
    let mut commits: Vec<Commit> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if token.is_empty()
            && tokens
                .get(index + 1)
                .and_then(|t| text(t))
                .is_some_and(|t| is_hash(t.trim()))
        {
            commits.push(Commit::default());
            index += 2;
            continue;
        }
        let Some(commit) = commits.last_mut() else {
            index += 1;
            continue;
        };
        let status = text(token).unwrap_or_default();
        let status = status.trim_start_matches('\n');
        if status.is_empty() {
            index += 1;
            continue;
        }
        let paths = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        let fields: Vec<Option<String>> = (1..=paths)
            .map(|offset| tokens.get(index + offset).and_then(|t| text(t)))
            .collect();
        index += 1 + paths;
        match (status.as_bytes()[0], fields.as_slice()) {
            (b'R', [Some(from), Some(to)]) => {
                commit.changes.push(Change::Rename {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            (b'C', [_, Some(to)]) | (_, [Some(to)]) => {
                commit.changes.push(Change::Touch(to.clone()));
            }
            _ => {}
        }
    }
    commits
}

/// A weighted pair of files, `a < b`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub weight: u32,
}

/// Counts co-changes over `commits` (newest first) between files in `current`.
///
/// Walking newest to oldest, a rename `from -> to` makes any *older* reference to `from` mean
/// `to`. A commit newer than the rename is resolved before the alias exists, so a different file
/// later created at the old name is not confused with the original.
#[must_use]
pub fn compute<S: BuildHasher>(
    commits: &[Commit],
    current: &HashSet<String, S>,
    config: &CoChangeConfig,
) -> Vec<Pair> {
    let mut alias: HashMap<&str, String> = HashMap::new();
    let mut counts: HashMap<(String, String), u32> = HashMap::new();

    let resolve = |alias: &HashMap<&str, String>, path: &str| -> String {
        let mut path = path.to_owned();
        // Chains (a -> b -> c) resolve fully; the bound guards against a cycle in odd histories.
        for _ in 0..64 {
            match alias.get(path.as_str()) {
                Some(next) if *next != path => path = next.clone(),
                _ => break,
            }
        }
        path
    };

    for commit in commits.iter().take(config.max_commits) {
        let mut files: Vec<String> = Vec::new();
        for change in &commit.changes {
            match change {
                Change::Touch(path) => files.push(resolve(&alias, path)),
                Change::Rename { to, .. } => files.push(resolve(&alias, to)),
            }
        }
        let raw_count = commit.changes.len();
        // Aliases take effect for older commits only, so they are recorded after this one.
        for change in &commit.changes {
            if let Change::Rename { from, to } = change {
                let target = resolve(&alias, to);
                alias.insert(from.as_str(), target);
            }
        }
        if raw_count > config.max_commit_files {
            continue;
        }
        files.retain(|f| current.contains(f));
        files.sort();
        files.dedup();
        for (i, a) in files.iter().enumerate() {
            for b in &files[i + 1..] {
                *counts.entry((a.clone(), b.clone())).or_insert(0) += 1;
            }
        }
    }

    let supported: Vec<(String, String, u32)> = counts
        .into_iter()
        .filter(|(_, weight)| *weight >= config.min_support)
        .map(|((a, b), weight)| (a, b, weight))
        .collect();

    // Each file's heaviest `top_k` edges, ties broken by the other path so the result is stable.
    let mut by_file: HashMap<&str, Vec<(u32, &str)>> = HashMap::new();
    for (a, b, weight) in &supported {
        by_file.entry(a).or_default().push((*weight, b));
        by_file.entry(b).or_default().push((*weight, a));
    }
    let mut keep: HashSet<(&str, &str)> = HashSet::new();
    for (file, mut edges) in by_file {
        edges.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(y.1)));
        for (_, other) in edges.into_iter().take(config.top_k) {
            keep.insert(if file < other {
                (file, other)
            } else {
                (other, file)
            });
        }
    }
    let mut pairs: Vec<Pair> = supported
        .iter()
        .filter(|(a, b, _)| keep.contains(&(a.as_str(), b.as_str())))
        .map(|(a, b, weight)| Pair {
            a: a.clone(),
            b: b.clone(),
            weight: *weight,
        })
        .collect();
    pairs.sort_by(|x, y| (&x.a, &x.b).cmp(&(&y.a, &y.b)));
    pairs
}

/// Runs `command` without a shell, returning stdout, or `None` if it could not start, exited
/// unsuccessfully, or ran past `timeout`.
///
/// At most `max_bytes` are read; a command that produces more is killed and what was read so far
/// is returned, so one pathological repository cannot exhaust memory.
#[must_use]
pub fn run_bounded(command: &mut Command, timeout: Duration, max_bytes: usize) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let mut capped = false;
        while let Ok(read) = stdout.read(&mut chunk) {
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read.min(max_bytes.saturating_sub(out.len()))]);
            if out.len() >= max_bytes {
                capped = true;
                break;
            }
        }
        (out, capped)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, capped) = reader.join().ok()?;
    if capped {
        let _ = child.kill();
        let _ = child.wait();
        return Some(out);
    }
    status
        .filter(std::process::ExitStatus::success)
        .map(|_| out)
}

fn git(root: &Path) -> Command {
    // Keep the standalone indexer independent of the caller's PATH and git environment.
    let invocation = elefante_guard::git::hardened(Path::new("/usr/bin/git"), root);
    let mut command = Command::new(invocation.program);
    command
        .args(invocation.args)
        .args(["-c", "core.quotepath=false"])
        .current_dir(invocation.cwd)
        .env_clear()
        .envs(invocation.environment);
    command
}

/// The current commit, or `None` for a non-repository, an unborn branch, or no `git`.
#[must_use]
pub fn head(root: &Path) -> Option<String> {
    let out = run_bounded(
        git(root).args(["rev-parse", "--verify", "-q", "HEAD"]),
        GIT_TIMEOUT,
        4096,
    )?;
    let head = String::from_utf8(out).ok()?.trim().to_owned();
    is_hash(&head).then_some(head)
}

/// Reads and parses the recent history, newest first.
#[must_use]
pub fn log(root: &Path, config: &CoChangeConfig, timeout: Duration) -> Option<Vec<Commit>> {
    let limit = config.max_commits.to_string();
    let raw = run_bounded(
        git(root).args([
            "log",
            "--no-ext-diff",
            "--no-textconv",
            "--no-merges",
            "-n",
            &limit,
            "--name-status",
            "-M",
            "-z",
            "--format=%x00%H",
        ]),
        timeout,
        MAX_LOG_BYTES,
    )?;
    Some(parse_log(&raw))
}
