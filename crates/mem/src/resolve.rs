//! Turning raw import specs into file paths. Design: `docs/M2-DESIGN.md` § 8, tier 1.
//!
//! A spec resolves against the *set of paths in the repository*, never the filesystem, so the
//! functions here are pure and a path appearing or disappearing anywhere can change what any
//! file's imports mean. That is why edges are derived by a separate pass over stored specs
//! rather than during extraction.

use std::{collections::HashSet, hash::BuildHasher};

/// Resolves one import of `from` to a repository path, or `None` when it points outside the
/// repository or at something tier 1 does not understand (external crates, bare packages).
///
/// The result may equal `from` (a module importing from itself); callers drop those.
#[must_use]
pub fn resolve<S: BuildHasher>(
    from: &str,
    kind: &str,
    spec: &str,
    paths: &HashSet<String, S>,
) -> Option<String> {
    let extension = from.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    match extension.as_str() {
        "rs" => resolve_rust(from, kind, spec, paths),
        "ts" | "tsx" | "mts" | "cts" => resolve_ts(from, spec, paths),
        _ => None,
    }
}

fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join(dir: &str, tail: &str) -> String {
    if dir.is_empty() {
        tail.to_owned()
    } else {
        format!("{dir}/{tail}")
    }
}

// ----- TypeScript -----

/// Collapses `.` and `..` in `dir/spec`, refusing to climb above the repository root.
fn normalize(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in spec.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn resolve_ts<S: BuildHasher>(
    from: &str,
    spec: &str,
    paths: &HashSet<String, S>,
) -> Option<String> {
    if !(spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let target = normalize(dirname(from), spec)?;
    let found = |candidate: &str| paths.contains(candidate).then(|| candidate.to_owned());

    if let Some(hit) = found(&target) {
        return Some(hit);
    }
    // NodeNext-style imports name the *output* extension: `./util.js` is `util.ts` on disk.
    for (from_ext, to_exts) in [
        (".js", &[".ts", ".tsx"][..]),
        (".jsx", &[".tsx"][..]),
        (".mjs", &[".mts"][..]),
        (".cjs", &[".cts"][..]),
    ] {
        if let Some(stem) = target.strip_suffix(from_ext) {
            for to in to_exts {
                if let Some(hit) = found(&format!("{stem}{to}")) {
                    return Some(hit);
                }
            }
        }
    }
    for extension in [
        ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs",
    ] {
        if let Some(hit) = found(&format!("{target}{extension}")) {
            return Some(hit);
        }
    }
    for index in [
        "index.ts",
        "index.tsx",
        "index.d.ts",
        "index.js",
        "index.jsx",
    ] {
        if let Some(hit) = found(&join(&target, index)) {
            return Some(hit);
        }
    }
    None
}

// ----- Rust -----

/// Where a Rust file sits in its crate: the directory modules are laid out under, the file that
/// is the crate root, and the file's own module path from that root.
struct CratePosition {
    dir: String,
    root_file: String,
    module: Vec<String>,
}

fn locate_in_crate<S: BuildHasher>(from: &str, paths: &HashSet<String, S>) -> CratePosition {
    let own_root = |dir: &str| CratePosition {
        dir: dir.to_owned(),
        root_file: from.to_owned(),
        module: Vec::new(),
    };
    let parent = dirname(from);
    // `tests/x.rs`, `examples/x.rs`, `benches/x.rs` and `src/bin/x.rs` are each their own crate.
    if matches!(basename(parent), "tests" | "examples" | "benches" | "bin") {
        return own_root(parent);
    }
    let mut dir = parent;
    loop {
        for root in ["lib.rs", "main.rs"] {
            let candidate = join(dir, root);
            if paths.contains(&candidate) {
                let relative = from
                    .strip_prefix(dir)
                    .map_or(from, |r| r.trim_start_matches('/'));
                let mut module: Vec<String> = relative.split('/').map(str::to_owned).collect();
                let last = module.pop().unwrap_or_default();
                match last.as_str() {
                    "lib.rs" | "main.rs" if module.is_empty() => {}
                    "mod.rs" => {}
                    other => module.push(other.trim_end_matches(".rs").to_owned()),
                }
                return CratePosition {
                    dir: dir.to_owned(),
                    root_file: candidate,
                    module,
                };
            }
        }
        if dir.is_empty() {
            return own_root(parent);
        }
        dir = dirname(dir);
    }
}

/// The file that defines module `segments` under `crate_dir`, trying `a/b.rs` then `a/b/mod.rs`.
fn module_file<S: BuildHasher>(
    position: &CratePosition,
    segments: &[String],
    paths: &HashSet<String, S>,
) -> Option<String> {
    if segments.is_empty() {
        return Some(position.root_file.clone());
    }
    let base = join(&position.dir, &segments.join("/"));
    [format!("{base}.rs"), format!("{base}/mod.rs")]
        .into_iter()
        .find(|candidate| paths.contains(candidate))
}

fn resolve_rust<S: BuildHasher>(
    from: &str,
    kind: &str,
    spec: &str,
    paths: &HashSet<String, S>,
) -> Option<String> {
    let position = locate_in_crate(from, paths);
    if kind == "mod" {
        let mut target = position.module.clone();
        target.push(spec.to_owned());
        return module_file(&position, &target, paths).filter(|f| f != from);
    }

    let mut parts = spec.split("::");
    let mut absolute: Vec<String> = match parts.next()? {
        "crate" => Vec::new(),
        "self" => position.module.clone(),
        "super" => {
            let mut module = position.module.clone();
            module.pop()?;
            module
        }
        // Anything else is an external crate, the standard library, or a local name in scope:
        // not resolvable from paths alone.
        _ => return None,
    };
    for part in parts {
        if part == "super" {
            absolute.pop()?;
        } else if part != "self" {
            absolute.push(part.to_owned());
        }
    }
    // The tail of a `use` path is usually an item, not a module. Take the longest prefix that is
    // a file; an item defined inline falls back to the file that holds it.
    (0..=absolute.len())
        .rev()
        .find_map(|take| module_file(&position, &absolute[..take], paths))
}
