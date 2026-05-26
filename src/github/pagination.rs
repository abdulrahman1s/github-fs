use reqwest::header::HeaderValue;

/// Extract the URL of the `rel="next"` entry from a GitHub-style `Link` header.
///
/// GitHub format example:
/// ```text
/// Link: <https://api.github.com/user/repos?page=2>; rel="next",
///       <https://api.github.com/user/repos?page=8>; rel="last"
/// ```
pub fn parse_next_link(header: Option<&HeaderValue>) -> Option<String> {
    let raw = header?.to_str().ok()?;
    raw.split(',').find_map(|entry| {
        let entry = entry.trim();
        let mut parts = entry.split(';');
        let url_part = parts.next()?.trim();
        let url = url_part.strip_prefix('<')?.strip_suffix('>')?;
        let is_next = parts.any(|attr| attr.trim() == r#"rel="next""#);
        is_next.then(|| url.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn none_when_header_absent() {
        assert_eq!(parse_next_link(None), None);
    }

    #[test]
    fn extracts_single_next() {
        let v = hv(r#"<https://api.github.com/user/repos?page=2>; rel="next""#);
        assert_eq!(
            parse_next_link(Some(&v)),
            Some("https://api.github.com/user/repos?page=2".to_string())
        );
    }

    #[test]
    fn skips_non_next_and_finds_next() {
        let v = hv(
            r#"<https://x/last>; rel="last", <https://x/next>; rel="next", <https://x/first>; rel="first""#,
        );
        assert_eq!(
            parse_next_link(Some(&v)),
            Some("https://x/next".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_next_relation() {
        let v = hv(r#"<https://x/last>; rel="last", <https://x/prev>; rel="prev""#);
        assert_eq!(parse_next_link(Some(&v)), None);
    }

    #[test]
    fn tolerates_extra_params_after_rel() {
        let v = hv(r#"<https://x/next>; rel="next"; foo="bar""#);
        assert_eq!(
            parse_next_link(Some(&v)),
            Some("https://x/next".to_string())
        );
    }

    #[test]
    fn returns_none_on_malformed_entry() {
        let v = hv(r#"no-brackets; rel="next""#);
        assert_eq!(parse_next_link(Some(&v)), None);
    }
}
