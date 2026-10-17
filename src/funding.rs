//! Ways to donate to a project: the platforms Donarium knows how to link to.

use url::Url;

/// (identifier, display name). `other` takes a full https link instead of a handle.
pub const PLATFORMS: &[(&str, &str)] = &[
    ("github-sponsors", "GitHub Sponsors"),
    ("opencollective", "Open Collective"),
    ("liberapay", "Liberapay"),
    ("kofi", "Ko-fi"),
    ("patreon", "Patreon"),
    ("thanks-dev", "thanks.dev"),
    ("other", "Other (a link)"),
];

/// Where a donation can have been made: the platforms above, a direct payment
/// (bank transfer, PayPal, ...), or something else.
pub fn donation_platforms() -> Vec<(&'static str, &'static str)> {
    let mut all: Vec<_> = PLATFORMS
        .iter()
        .copied()
        .filter(|(id, _)| *id != "other")
        .collect();
    all.push(("direct", "Direct payment (bank, PayPal, …)"));
    all.push(("other", "Other"));
    all
}

pub fn is_donation_platform(platform: &str) -> bool {
    donation_platforms().iter().any(|(id, _)| *id == platform)
}

pub fn is_platform(platform: &str) -> bool {
    PLATFORMS.iter().any(|(id, _)| *id == platform)
}

pub fn display_name(platform: &str) -> &str {
    PLATFORMS
        .iter()
        .find(|(id, _)| *id == platform)
        .map_or_else(
            || {
                if platform == "direct" {
                    "Direct payment"
                } else {
                    platform
                }
            },
            |(_, name)| name,
        )
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct FundingError(pub String);

/// Validates what an admin typed for a platform. Returns the handle to store
/// and, for `other`, the link. Handles are a short word, never a URL, because
/// the link is built from a fixed template.
pub fn parse(platform: &str, input: &str) -> Result<(String, Option<String>), FundingError> {
    let input = input.trim();
    if !is_platform(platform) {
        return Err(FundingError("choose a platform".into()));
    }
    if platform == "other" {
        let url = Url::parse(input)
            .ok()
            .filter(|u| u.scheme() == "https" && u.host_str().is_some())
            .ok_or_else(|| FundingError("enter a link starting with https://".into()))?;
        if input.len() > 300 {
            return Err(FundingError("that link is too long".into()));
        }
        return Ok((url.to_string(), Some(url.to_string())));
    }
    let valid = !input.is_empty()
        && input.len() <= 100
        && input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err(FundingError(format!(
            "enter the {} name only (letters, digits, - _ .), not a link",
            display_name(platform)
        )));
    }
    Ok((input.to_string(), None))
}

/// The page to donate on, if the platform and handle make one.
pub fn link(platform: &str, handle: &str, url: Option<&str>) -> Option<String> {
    if let Some(url) = url {
        // Only https links are ever rendered as links.
        return Url::parse(url)
            .ok()
            .filter(|u| u.scheme() == "https")
            .map(|u| u.to_string());
    }
    Some(match platform {
        "github-sponsors" => format!("https://github.com/sponsors/{handle}"),
        "opencollective" => format!("https://opencollective.com/{handle}"),
        "liberapay" => format!("https://liberapay.com/{handle}"),
        "kofi" => format!("https://ko-fi.com/{handle}"),
        "patreon" => format!("https://www.patreon.com/{handle}"),
        "thanks-dev" => format!("https://thanks.dev/d/gh/{handle}"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_become_links() {
        assert_eq!(
            link("github-sponsors", "tokio-rs", None).as_deref(),
            Some("https://github.com/sponsors/tokio-rs")
        );
        assert_eq!(
            link("opencollective", "tokio", None).as_deref(),
            Some("https://opencollective.com/tokio")
        );
        assert_eq!(
            link("liberapay", "a.b_c", None).as_deref(),
            Some("https://liberapay.com/a.b_c")
        );
        assert_eq!(link("unknown", "x", None), None);
        assert_eq!(
            link("other", "x", Some("https://example.org/donate")).as_deref(),
            Some("https://example.org/donate")
        );
        // Never a link that is not https.
        assert_eq!(link("other", "x", Some("javascript:alert(1)")), None);
        assert_eq!(link("other", "x", Some("http://example.org")), None);
    }

    #[test]
    fn parsing_input() {
        assert_eq!(
            parse("kofi", " someone ").unwrap(),
            ("someone".into(), None)
        );
        assert_eq!(
            parse("other", "https://example.org/donate?x=1").unwrap(),
            (
                "https://example.org/donate?x=1".into(),
                Some("https://example.org/donate?x=1".into())
            )
        );
        for (platform, bad) in [
            ("kofi", ""),
            ("kofi", "https://ko-fi.com/someone"),
            ("kofi", "a/b"),
            ("kofi", "a b"),
            ("kofi", "x?y"),
            ("other", "http://example.org"),
            ("other", "example.org"),
            ("other", "javascript:alert(1)"),
            ("nonsense", "x"),
        ] {
            assert!(parse(platform, bad).is_err(), "{platform} {bad:?}");
        }
        assert!(parse("kofi", &"a".repeat(101)).is_err());
    }

    #[test]
    fn donation_platforms_include_direct_payments() {
        let ids: Vec<&str> = donation_platforms().iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&"github-sponsors") && ids.contains(&"direct") && ids.contains(&"other")
        );
        assert_eq!(ids.iter().filter(|i| **i == "other").count(), 1);
        assert!(is_donation_platform("direct"));
        assert!(!is_donation_platform("paypal"));
        assert_eq!(display_name("direct"), "Direct payment");
    }

    #[test]
    fn names() {
        assert_eq!(display_name("github-sponsors"), "GitHub Sponsors");
        assert_eq!(display_name("mystery"), "mystery");
        assert!(is_platform("liberapay"));
        assert!(!is_platform("paypal"));
    }
}
