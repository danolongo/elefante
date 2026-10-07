use std::collections::HashSet;

use elefante_mem::resolve::resolve;

fn paths(list: &[&str]) -> HashSet<String> {
    list.iter().map(|p| (*p).to_owned()).collect()
}

fn check(paths: &HashSet<String>, table: &[(&str, &str, &str, Option<&str>)]) {
    for (from, kind, spec, expected) in table {
        assert_eq!(
            resolve(from, kind, spec, paths).as_deref(),
            *expected,
            "{from}: {kind} {spec}"
        );
    }
}

fn workspace() -> HashSet<String> {
    paths(&[
        "crates/core/src/lib.rs",
        "crates/core/src/session.rs",
        "crates/core/src/gate/mod.rs",
        "crates/core/src/gate/rules.rs",
        "crates/core/src/util.rs",
        "crates/srv/src/main.rs",
        "crates/srv/src/server.rs",
        "crates/mem/tests/indexer.rs",
        "crates/mem/tests/common/mod.rs",
        "scripts/tool.rs",
    ])
}

#[test]
fn rust_mod_declarations_find_either_file_layout() {
    check(
        &workspace(),
        &[
            (
                "crates/core/src/lib.rs",
                "mod",
                "session",
                Some("crates/core/src/session.rs"),
            ),
            (
                "crates/core/src/lib.rs",
                "mod",
                "gate",
                Some("crates/core/src/gate/mod.rs"),
            ),
            (
                "crates/core/src/gate/mod.rs",
                "mod",
                "rules",
                Some("crates/core/src/gate/rules.rs"),
            ),
            (
                "crates/srv/src/main.rs",
                "mod",
                "server",
                Some("crates/srv/src/server.rs"),
            ),
            ("crates/core/src/lib.rs", "mod", "missing", None),
        ],
    );
}

#[test]
fn rust_use_paths_resolve_to_the_longest_prefix_that_is_a_file() {
    check(
        &workspace(),
        &[
            // A leaf module file.
            (
                "crates/core/src/session.rs",
                "use",
                "crate::util::helper",
                Some("crates/core/src/util.rs"),
            ),
            // Deeper than any file wins over shallower ones.
            (
                "crates/core/src/session.rs",
                "use",
                "crate::gate::rules::Rule",
                Some("crates/core/src/gate/rules.rs"),
            ),
            // The last segment is an item living in `gate/mod.rs`.
            (
                "crates/core/src/session.rs",
                "use",
                "crate::gate::Verdict",
                Some("crates/core/src/gate/mod.rs"),
            ),
            // An item defined in the crate root.
            (
                "crates/core/src/session.rs",
                "use",
                "crate::Session",
                Some("crates/core/src/lib.rs"),
            ),
            (
                "crates/srv/src/server.rs",
                "use",
                "crate::server::run",
                Some("crates/srv/src/server.rs"),
            ),
        ],
    );
}

#[test]
fn rust_self_and_super_are_relative_to_the_files_own_module() {
    check(
        &workspace(),
        &[
            // `session` is `[session]`; `super` is the root.
            (
                "crates/core/src/session.rs",
                "use",
                "super::util",
                Some("crates/core/src/util.rs"),
            ),
            // `gate::rules` is `[gate, rules]`; `super` is `gate`, which lives in `gate/mod.rs`.
            (
                "crates/core/src/gate/rules.rs",
                "use",
                "super::Verdict",
                Some("crates/core/src/gate/mod.rs"),
            ),
            (
                "crates/core/src/gate/rules.rs",
                "use",
                "super::super::util",
                Some("crates/core/src/util.rs"),
            ),
            // An inline module has no file of its own, so it falls back to the file holding it.
            (
                "crates/core/src/gate/rules.rs",
                "use",
                "self::inner::x",
                Some("crates/core/src/gate/rules.rs"),
            ),
            // Climbing above the crate root is not a path.
            ("crates/core/src/lib.rs", "use", "super::x", None),
            ("crates/core/src/lib.rs", "use", "super::super::x", None),
        ],
    );
}

#[test]
fn rust_paths_outside_the_crate_are_not_resolved_in_tier_one() {
    check(
        &workspace(),
        &[
            (
                "crates/core/src/session.rs",
                "use",
                "std::fmt::Display",
                None,
            ),
            (
                "crates/core/src/session.rs",
                "use",
                "tokio::sync::Mutex",
                None,
            ),
            (
                "crates/core/src/session.rs",
                "use",
                "elefante_srv::run",
                None,
            ),
            ("crates/core/src/session.rs", "use", "util::helper", None),
        ],
    );
}

#[test]
fn files_that_are_their_own_crate_resolve_modules_beside_themselves() {
    check(
        &workspace(),
        &[
            (
                "crates/mem/tests/indexer.rs",
                "mod",
                "common",
                Some("crates/mem/tests/common/mod.rs"),
            ),
            ("scripts/tool.rs", "mod", "helpers", None),
        ],
    );
}

fn ui() -> HashSet<String> {
    paths(&[
        "ui/app.tsx",
        "ui/util.ts",
        "ui/util.tsx",
        "ui/lib/index.ts",
        "ui/types.d.ts",
        "ui/legacy.js",
        "ui/data.json",
        "ui/comp/Button.tsx",
        "ui/esm.mts",
        "ui/a/b/deep.ts",
        "top.ts",
    ])
}

#[test]
fn typescript_relative_imports_probe_extensions_and_index_files() {
    check(
        &ui(),
        &[
            ("ui/app.tsx", "import", "./util", Some("ui/util.ts")),
            ("ui/app.tsx", "import", "./lib", Some("ui/lib/index.ts")),
            ("ui/app.tsx", "import", "./types", Some("ui/types.d.ts")),
            ("ui/app.tsx", "import", "./legacy", Some("ui/legacy.js")),
            ("ui/app.tsx", "import", "./data.json", Some("ui/data.json")),
            (
                "ui/app.tsx",
                "import",
                "./comp/Button",
                Some("ui/comp/Button.tsx"),
            ),
            ("ui/app.tsx", "reexport", "./util", Some("ui/util.ts")),
            // `.ts` before `.tsx` when both exist.
            ("ui/app.tsx", "import", "./util", Some("ui/util.ts")),
        ],
    );
}

#[test]
fn typescript_output_extensions_map_back_to_source_files() {
    check(
        &ui(),
        &[
            ("ui/app.tsx", "import", "./util.js", Some("ui/util.ts")),
            ("ui/app.tsx", "import", "./esm.mjs", Some("ui/esm.mts")),
            ("ui/app.tsx", "import", "./legacy.js", Some("ui/legacy.js")),
            ("ui/app.tsx", "import", "./nothing.js", None),
        ],
    );
}

#[test]
fn typescript_parent_references_normalise_and_cannot_escape_the_repository() {
    check(
        &ui(),
        &[
            ("ui/a/b/deep.ts", "import", "../../util", Some("ui/util.ts")),
            ("ui/a/b/deep.ts", "import", "../../../top", Some("top.ts")),
            ("ui/a/b/deep.ts", "import", "../../../../escape", None),
            ("ui/app.tsx", "import", "../ui/util", Some("ui/util.ts")),
            ("ui/app.tsx", "import", "./comp/../util", Some("ui/util.ts")),
            ("ui/app.tsx", "import", ".", None),
        ],
    );
}

#[test]
fn typescript_bare_and_absolute_specifiers_are_not_resolved() {
    check(
        &ui(),
        &[
            ("ui/app.tsx", "import", "react", None),
            ("ui/app.tsx", "import", "@scope/pkg", None),
            ("ui/app.tsx", "import", "/abs/path", None),
            ("ui/app.tsx", "import", "node:fs", None),
        ],
    );
}

#[test]
fn other_languages_have_no_resolver() {
    let p = paths(&["a.md", "b.md", "x.py", "y.py"]);
    check(
        &p,
        &[("a.md", "use", "b", None), ("x.py", "import", "y", None)],
    );
}
