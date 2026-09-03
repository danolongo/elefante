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
        Ok(Self { root, extra_denied })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
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
        self.check_denied(input, lexical_relative)?;

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
        self.check_denied(input, relative)?;
        Ok(resolved)
    }

    fn check_denied(&self, input: &Path, relative: &Path) -> Result<(), PolicyError> {
        if let Some(rule) = built_in_secret_rule(relative) {
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

fn normalize(path: &Path) -> PathBuf {
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

fn built_in_secret_rule(path: &Path) -> Option<&'static str> {
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
