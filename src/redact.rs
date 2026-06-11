//! Two-layer secret masking (spec INV-7): layer 1 decides which env values are
//! secret by their key name; layer 2 masks those literal values anywhere they
//! appear in emitted output — so a secret echoed inside captured stderr is masked.

const MASK: &str = "***";
const SECRET_MARKERS: [&str; 5] = ["SECRET", "PASSWORD", "KEY", "TOKEN", "CREDENTIAL"];

/// Layer 1: a config/env key whose name marks its value as sensitive.
pub fn is_secret_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|marker| upper.contains(marker))
}

/// Layer 2: masks the literal secret values it was built with.
#[derive(Default, Clone)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactor({} secrets)", self.secrets.len())
    }
}

impl Redactor {
    /// Longest values first so an overlapping shorter secret cannot leave a tail.
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = values.into_iter().filter(|v| !v.is_empty()).collect();
        secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
        secrets.dedup();
        Redactor { secrets }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), MASK);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_heuristic_is_case_insensitive() {
        assert!(is_secret_key("MAXMIND_LICENSE_KEY"));
        assert!(is_secret_key("db_password"));
        assert!(is_secret_key("Api_Token"));
        assert!(is_secret_key("REGISTRY_CREDENTIAL"));
        assert!(!is_secret_key("APP_ENV"));
        assert!(!is_secret_key("REGISTRY"));
    }

    #[test]
    fn masks_value_anywhere_including_stderr() {
        let r = Redactor::new(["s3cr3t-license".to_string()]);
        let stderr = "docker: error pulling with key s3cr3t-license at registry";
        assert_eq!(
            r.apply(stderr),
            "docker: error pulling with key *** at registry"
        );
    }

    #[test]
    fn empty_values_never_mask() {
        let r = Redactor::new(["".to_string()]);
        assert_eq!(r.apply("anything"), "anything");
    }

    #[test]
    fn overlapping_secrets_fully_masked() {
        let r = Redactor::new(["abc".to_string(), "abcdef".to_string()]);
        assert_eq!(r.apply("x abcdef y"), "x *** y");
    }
}
