//! Target-neutral presentation rules.

/// Serializes WAI-ARIA boolean states as the required explicit token.
pub(crate) const fn aria_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

#[cfg(test)]
mod tests {
    use super::aria_bool;

    #[test]
    fn aria_booleans_use_explicit_valid_tokens() {
        assert_eq!(aria_bool(true), "true");
        assert_eq!(aria_bool(false), "false");
    }
}
