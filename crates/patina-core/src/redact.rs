//! Removing configured secret values from text before it is stored or shown: command output, tool results and
//! dev server logs. A command cannot read Patina's secrets, but a project can hold the same key in a file, and a
//! tool can echo what it was given.
use std::borrow::Cow;

/// What a secret value is replaced with.
pub const REDACTED: &str = "[secret removed by Patina]";

/// Values shorter than this are not redacted: they would match ordinary text and say nothing about the secret.
pub const MIN_LEN: usize = 8;

/// A set of secret values to remove from text.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a value that contains another is removed whole.
    values: Vec<String>,
}

impl Redactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Redactor {
        let mut values: Vec<String> = values
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| v.chars().count() >= MIN_LEN)
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Redactor { values }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `text` with every secret value replaced.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = Cow::Owned(out.replace(v.as_str(), REDACTED));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_values_are_replaced_wherever_they_appear() {
        let r = Redactor::new([
            "sk-live-0123456789".to_string(),
            "sk-live-0123456789abcdef".to_string(),
            "short".to_string(),
        ]);
        assert_eq!(
            r.redact("key=sk-live-0123456789abcdef and sk-live-0123456789."),
            format!("key={REDACTED} and {REDACTED}.")
        );
        // Short values would match ordinary words, so they are left alone.
        assert_eq!(r.redact("a short note"), "a short note");
        assert!(matches!(r.redact("nothing here"), Cow::Borrowed(_)));
        assert!(Redactor::new([]).is_empty());
    }
}
