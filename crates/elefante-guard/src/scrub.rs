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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_all_supported_secret_shapes() {
        let input = concat!(
            "key sk-ant-api03_abcdefghijklmnopqrstuvwxyz\n",
            "router sk-or-v1-0123456789abcdef0123456789abcdef\n",
            "aws AKIAABCDEFGHIJKLMNOP\n",
            "API_TOKEN=top-secret\n",
            "db_password = hunter2\n",
            "QUOTED_SECRET=\"two words remain private\"\n",
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
        );
        let scrubbed = Scrubber::new().scrub(input);
        for secret in [
            "sk-ant-",
            "sk-or-v1-",
            "AKIAABCDEFGHIJKLMNOP",
            "top-secret",
            "hunter2",
            "two words remain private",
            "abc",
        ] {
            assert!(!scrubbed.contains(secret), "secret survived: {secret}");
        }
        assert!(scrubbed.contains("API_TOKEN=[REDACTED]"));
        assert!(scrubbed.contains("[REDACTED PEM]"));
    }

    #[test]
    fn scrubs_openrouter_keys_without_dropping_anthropic_keys() {
        let scrubbed = Scrubber::new().scrub(concat!(
            "OPENROUTER_API_KEY: sk-or-v1-0123456789abcdef0123456789abcdef\n",
            "ANTHROPIC_API_KEY: sk-ant-api03_abcdefghijklmnopqrstuvwxyz\n",
        ));
        assert!(!scrubbed.contains("sk-or-v1-"), "openrouter key survived");
        assert!(!scrubbed.contains("sk-ant-"), "anthropic key survived");
        assert_eq!(scrubbed.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn leaves_normal_output_unchanged() {
        let value = "cargo test: 12 passed";
        assert_eq!(Scrubber::new().scrub(value), value);
    }
}
