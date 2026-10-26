use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const POLICY_NAME: &str = "secret-path-policy";

/// A repository-bound path policy.
///
/// Resolution is lexical first and canonical second. Canonicalizing the nearest existing
/// ancestor lets the policy safely validate new write targets while still following symlinks.
#[derive(Debug, Clone)]
pub struct PathPolicy {
    root: PathBuf,
    extra_denied: GlobSet,
    credentials: Option<PathBuf>,
    home_harness: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("{POLICY_NAME}: path escapes workspace: {path}")]
    OutsideRoot { path: String },
    #[error("{POLICY_NAME}: denied secret path: {path} ({rule})")]
    Denied { path: String, rule: &'static str },
    #[error("{POLICY_NAME}: cannot resolve {path}: {source}")]
    Resolve {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{POLICY_NAME}: invalid custom pattern {pattern}: {message}")]
    InvalidPattern { pattern: String, message: String },
}

impl PathPolicy {
    /// Creates a policy rooted at an existing directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, PolicyError> {
        Self::with_denied_patterns(root, std::iter::empty::<&str>())
    }

    /// Adds repository-relative glob patterns to the built-in secret rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized or a pattern is invalid.
    pub fn with_denied_patterns<I, S>(
        root: impl AsRef<Path>,
        patterns: I,
    ) -> Result<Self, PolicyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let root_input = root.as_ref();
        let root = root_input
            .canonicalize()
            .map_err(|source| PolicyError::Resolve {
                path: root_input.display().to_string(),
                source,
            })?;
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            let glob = Glob::new(pattern).map_err(|error| PolicyError::InvalidPattern {
                pattern: pattern.to_owned(),
                message: error.to_string(),
            })?;
            builder.add(glob);
        }
        let extra_denied = builder
            .build()
            .map_err(|error| PolicyError::InvalidPattern {
                pattern: "<set>".to_owned(),
                message: error.to_string(),
            })?;
        Ok(Self {
            root,
            extra_denied,
            credentials: canonical_credentials(),
            home_harness: home_harness_dirs(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `path` names a built-in secret location, judged by its components alone.
    ///
    /// `path` is taken to be relative to a workspace root, so any `.elefante` component is the
    /// harness's own state and counts, including a bare `.elefante` directory entry.
    ///
    /// Purely lexical: no filesystem access, no symlink resolution, and no root. That makes it
    /// cheap enough to run on every entry of a repository walk, which [`PathPolicy::resolve`]
    /// is not. It is not a substitute for `resolve` at a syscall boundary, which also defeats
    /// symlinks and traversal.
    #[must_use]
    pub fn is_secret_path(path: &Path) -> bool {
        built_in_secret_rule(path).is_some()
    }

    /// Resolves `path` beneath the workspace and rejects known secret locations.
    ///
    /// # Errors
    ///
    /// Returns an error when the path escapes the workspace, resolves through a secret or
    /// escaping symlink, or its nearest existing ancestor cannot be canonicalized.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, PolicyError> {
        let input = path.as_ref();
        let joined = if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.root.join(input)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(&self.root) {
            return Err(PolicyError::OutsideRoot {
                path: input.display().to_string(),
            });
        }

        // Check the lexical path before following symlinks. Otherwise a path whose name is
        // itself protected (for example `.env`) could be made to point at an innocuous target.
        let lexical_relative = normalized.strip_prefix(&self.root).unwrap_or(&normalized);
        self.check_denied(input, &normalized, lexical_relative)?;

        let (ancestor, suffix) =
            closest_existing_ancestor(&normalized).map_err(|source| PolicyError::Resolve {
                path: input.display().to_string(),
                source,
            })?;
        let canonical_ancestor =
            ancestor
                .canonicalize()
                .map_err(|source| PolicyError::Resolve {
                    path: input.display().to_string(),
                    source,
                })?;
        let resolved = normalize(
            &suffix
                .iter()
                .fold(canonical_ancestor, |path, part| path.join(part)),
        );
        if !resolved.starts_with(&self.root) {
            return Err(PolicyError::OutsideRoot {
                path: input.display().to_string(),
            });
        }

        let relative = resolved.strip_prefix(&self.root).unwrap_or(&resolved);
        self.check_denied(input, &resolved, relative)?;
        Ok(resolved)
    }

    /// Resolves a write target, additionally protecting git metadata in any case.
    ///
    /// # Errors
    /// Returns a policy error for secret, escaping, or git metadata paths.
    pub fn resolve_for_write(&self, path: impl AsRef<Path>) -> Result<PathBuf, PolicyError> {
        let input = path.as_ref();
        let resolved = self.resolve(input)?;
        if [
            input,
            resolved.strip_prefix(&self.root).unwrap_or(&resolved),
        ]
        .into_iter()
        .any(|path| {
            path.components().any(
                |part| matches!(part, Component::Normal(name) if name.eq_ignore_ascii_case(".git")),
            )
        }) {
            return Err(PolicyError::Denied {
                path: input.display().to_string(),
                rule: "git-metadata-write",
            });
        }
        Ok(resolved)
    }

    fn check_denied(
        &self,
        input: &Path,
        absolute: &Path,
        relative: &Path,
    ) -> Result<(), PolicyError> {
        if self.credentials.as_deref() == Some(absolute) {
            return Err(PolicyError::Denied {
                path: input.display().to_string(),
                rule: ".elefante/credentials.toml",
            });
        }
        // The user-level harness directory is denied by prefix. The `.elefante` component rule
        // below runs on the root-relative path only, so a workspace that merely lives under some
        // `.elefante/` directory keeps working.
        if self
            .home_harness
            .iter()
            .any(|dir| absolute.starts_with(dir))
        {
            return Err(PolicyError::Denied {
                path: input.display().to_string(),
                rule: "~/.elefante/** (harness state)",
            });
        }
        if let Some(rule) = credential_rule(absolute).or_else(|| built_in_secret_rule(relative)) {
            return Err(PolicyError::Denied {
                path: input.display().to_string(),
                rule,
            });
        }
        if self.extra_denied.is_match(relative) {
            return Err(PolicyError::Denied {
                path: input.display().to_string(),
                rule: "custom-pattern",
            });
        }
        Ok(())
    }
}

pub(crate) fn canonical_credentials() -> Option<PathBuf> {
    std::env::var_os("HOME").and_then(|home| {
        Path::new(&home)
            .join(".elefante/credentials.toml")
            .canonicalize()
            .ok()
    })
}

/// `<home>/.elefante`, as given and canonicalized. Unlike [`canonical_credentials`] it does not
/// need the directory to exist, so a not-yet-created harness directory is still protected.
pub(crate) fn home_harness_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let mut dirs = vec![home.join(".elefante")];
    let canonical = [
        home.canonicalize().ok().map(|home| home.join(".elefante")),
        home.join(".elefante").canonicalize().ok(),
    ];
    for dir in canonical.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn closest_existing_ancestor(
    path: &Path,
) -> Result<(PathBuf, Vec<std::ffi::OsString>), std::io::Error> {
    let mut ancestor = path.to_path_buf();
    let mut suffix = Vec::new();
    // `Path::exists` follows symlinks and reports false for a dangling link. Using it here
    // would incorrectly treat that link as an ordinary nonexistent component and reconstruct
    // it after canonicalization, allowing a later write to follow the link outside the root.
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(name) = ancestor.file_name() {
                    suffix.push(name.to_os_string());
                }
                if !ancestor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    suffix.reverse();
    Ok((ancestor, suffix))
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Every built-in rule, including the harness-state rule for any `.elefante` component. Meant
/// for a path relative to the workspace root (or one typed that way in a shell command).
pub(crate) fn built_in_secret_rule(path: &Path) -> Option<&'static str> {
    secret_rule(path, true)
}

/// The rules that hold wherever the workspace happens to live: credentials and key material, but
/// not the harness-state rule, which depends on where the workspace root is.
pub(crate) fn credential_rule(path: &Path) -> Option<&'static str> {
    secret_rule(path, false)
}

fn secret_rule(path: &Path, harness_state: bool) -> Option<&'static str> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();

    for (index, component) in components.iter().enumerate() {
        if *component == ".ssh" {
            return Some(".ssh/**");
        }
        if *component == ".aws" && components.get(index + 1) == Some(&"credentials") {
            return Some(".aws/credentials");
        }
        if *component == ".cargo"
            && matches!(
                components.get(index + 1),
                Some(&("credentials" | "credentials.toml"))
            )
        {
            return Some(".cargo/credentials or .cargo/credentials.toml");
        }
        if *component == ".elefante" && components.get(index + 1) == Some(&"credentials.toml") {
            return Some(".elefante/credentials.toml");
        }
        // The harness's own state: learned notes, the index and its keys. The agent reaches them
        // through the injected context and `memory_search`, never as files. The directory itself
        // counts too, so listing or recursing into it is refused as well as opening a file in it.
        if harness_state && *component == ".elefante" {
            return Some(".elefante/** (harness state)");
        }
        if *component == ".env" || component.starts_with(".env.") {
            return Some(".env or .env.*");
        }
        if matches!(*component, ".netrc" | ".npmrc" | ".git-credentials") {
            return Some("credential-file");
        }
        if component.starts_with("id_rsa") {
            return Some("id_rsa*");
        }
        if Path::new(component).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pem") || extension.eq_ignore_ascii_case("key")
        }) {
            return Some("*.pem or *.key");
        }
    }
    None
}
