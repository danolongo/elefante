use regex::{Captures, Regex};

/// Redacts common credentials from untrusted tool output.
#[derive(Debug, Clone)]
pub struct Scrubber {
    anthropic: Regex,
    openrouter: Regex,
    aws_access_key: Regex,
    pem: Regex,
    assignment: Regex,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Constructs the built-in redaction set.
    ///
    /// # Panics
    ///
    /// Panics only if one of the crate's constant regular expressions is invalid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Kept independently of which provider Elefante itself calls: these are
            // secret shapes that leak from the repositories it edits.
            anthropic: Regex::new(r"sk-ant-[A-Za-z0-9_-]{8,}").expect("static regex"),
            openrouter: Regex::new(r"sk-or-v1-[A-Za-z0-9_-]{8,}").expect("static regex"),
            aws_access_key: Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").expect("static regex"),
            pem: Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*(?:PRIVATE KEY|CERTIFICATE)-----.*?-----END [A-Z0-9 ]*(?:PRIVATE KEY|CERTIFICATE)-----",
            )
            .expect("static regex"),
            assignment: Regex::new(
                r#"(?im)\b(?P<name>[A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD)[A-Z0-9_]*)\s*=\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s]+)"#,
            )
            .expect("static regex"),
        }
    }

    #[must_use]
    pub fn scrub(&self, input: &str) -> String {
        let value = self.pem.replace_all(input, "[REDACTED PEM]");
        let value = self.anthropic.replace_all(&value, "[REDACTED]");
        let value = self.openrouter.replace_all(&value, "[REDACTED]");
        let value = self.aws_access_key.replace_all(&value, "[REDACTED]");
        self.assignment
            .replace_all(&value, |captures: &Captures<'_>| {
                format!("{}=[REDACTED]", &captures["name"])
            })
            .into_owned()
    }
}
