use std::fmt;

#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.0;
        if s.is_empty() {
            return f.write_str("Token(<empty>)");
        }
        // Only show a short prefix when the token is clearly longer than the
        // prefix — otherwise we'd expose the whole secret. Real GitHub tokens
        // are >=40 chars, so anything shorter is either a placeholder or noise
        // and gets fully redacted.
        if s.len() <= 8 {
            return write!(f, "Token(***[{} chars])", s.len());
        }
        write!(f, "Token({}***[{} chars])", &s[..4], s.len())
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_full_token() {
        let t = Token::new("ghp_abcdef1234567890");
        let d = format!("{t:?}");
        assert!(!d.contains("abcdef"), "Debug leaked secret: {d}");
        assert!(!d.contains("1234567890"), "Debug leaked secret: {d}");
        assert!(
            d.contains("ghp_"),
            "Debug should keep short prefix for debugging: {d}"
        );
        assert!(d.contains("20"), "Debug should report length: {d}");
    }

    #[test]
    fn debug_handles_empty() {
        let t = Token::new("");
        let d = format!("{t:?}");
        assert!(d.contains("empty"), "got {d}");
    }

    #[test]
    fn debug_fully_redacts_short_tokens() {
        // Anything <= 8 chars never shows even a prefix — short strings are
        // typically placeholders or test values; protecting them costs us
        // nothing and avoids accidental leaks.
        let t = Token::new("ab");
        let d = format!("{t:?}");
        assert!(!d.contains("ab"), "short token leaked: {d}");
        assert!(d.contains("2 chars"));
    }

    #[test]
    fn debug_realistic_token_keeps_prefix_only() {
        // 40-char fake — typical PAT length.
        let t = Token::new("ghp_abcdefghijklmnopqrstuvwxyz0123456789AB");
        let d = format!("{t:?}");
        assert!(d.contains("ghp_"));
        assert!(!d.contains("abcdefghij"), "secret leaked: {d}");
        assert!(!d.contains("0123456789"), "secret leaked: {d}");
    }

    #[test]
    fn display_fully_redacts() {
        let t = Token::new("ghp_secret_value");
        let d = format!("{t}");
        assert!(!d.contains("secret"), "Display leaked secret: {d}");
        assert!(!d.contains("ghp_"), "Display leaked secret: {d}");
        assert_eq!(d, "<redacted>");
    }

    #[test]
    fn expose_returns_underlying_value() {
        let t = Token::new("ghp_x");
        assert_eq!(t.expose(), "ghp_x");
    }
}
