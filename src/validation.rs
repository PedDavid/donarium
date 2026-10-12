//! Validation of what admins type into forms. Everything that reaches the
//! store has passed through here.

use crate::model::Provider;

pub const MAX_WEIGHT: f64 = 100.0;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ValidationError(pub String);

fn invalid<T>(msg: impl Into<String>) -> Result<T, ValidationError> {
    Err(ValidationError(msg.into()))
}

/// Accepts `owner/name` (GitLab: also `group/subgroup/name`), or a full URL on
/// the provider's own host, and returns the normalised path.
pub fn parse_repo(provider: Provider, input: &str) -> Result<String, ValidationError> {
    let mut path = input.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = path.strip_prefix(scheme) {
            let prefix = format!("{}/", provider.host());
            let Some(rest) = rest.strip_prefix(prefix.as_str()) else {
                return invalid(format!(
                    "{} repositories must be on {}",
                    provider.as_str(),
                    provider.host()
                ));
            };
            path = rest;
        }
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    let ok = match provider {
        Provider::Github => segments.len() == 2,
        Provider::Gitlab => segments.len() >= 2,
    };
    if !ok {
        return invalid(match provider {
            Provider::Github => "enter the repository as owner/name",
            Provider::Gitlab => "enter the project as group/name (subgroups are allowed)",
        });
    }
    for (i, seg) in segments.iter().enumerate() {
        let is_owner = i == 0 && provider == Provider::Github;
        if !valid_segment(seg, is_owner) {
            return invalid(format!("{seg:?} is not a valid {} name", provider.as_str()));
        }
    }
    Ok(path.to_string())
}

fn valid_segment(seg: &str, github_owner: bool) -> bool {
    if seg.is_empty() || seg == "." || seg == ".." || seg.len() > 100 {
        return false;
    }
    if github_owner {
        // GitHub user and organisation names: up to 39 characters,
        // alphanumerics and hyphens, not starting or ending with a hyphen.
        return seg.len() <= 39
            && !seg.starts_with('-')
            && !seg.ends_with('-')
            && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    }
    seg.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn parse_weight(input: &str) -> Result<f64, ValidationError> {
    let Ok(w) = input.trim().parse::<f64>() else {
        return invalid("the weight must be a number, e.g. 1.0");
    };
    if !w.is_finite() || w <= 0.0 || w > MAX_WEIGHT {
        return invalid(format!(
            "the weight must be greater than 0 and at most {MAX_WEIGHT}"
        ));
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Provider::{Github, Gitlab};

    #[test]
    fn github_repos() {
        assert_eq!(
            parse_repo(Github, " PedDavid/infra ").unwrap(),
            "PedDavid/infra"
        );
        assert_eq!(
            parse_repo(Github, "https://github.com/tokio-rs/tokio.git/").unwrap(),
            "tokio-rs/tokio"
        );
        assert_eq!(parse_repo(Github, "a/b.c_d-e").unwrap(), "a/b.c_d-e");
        for bad in [
            "", "tokio", "a/b/c", "/b", "a/", "-a/b", "a-/b", "a b/c", "a/b c", "a/..", "a/.",
            "a/b?x=1", "a_b/c",
        ] {
            assert!(
                parse_repo(Github, bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn urls_must_be_on_the_providers_host() {
        assert!(parse_repo(Github, "https://evil.example/a/b").is_err());
        assert!(parse_repo(Github, "https://gitlab.com/a/b").is_err());
        assert!(parse_repo(Github, "https://github.com.evil.example/a/b").is_err());
        assert_eq!(
            parse_repo(Gitlab, "https://gitlab.com/g/sub/p").unwrap(),
            "g/sub/p"
        );
    }

    #[test]
    fn gitlab_allows_subgroups() {
        assert_eq!(
            parse_repo(Gitlab, "group/sub/proj").unwrap(),
            "group/sub/proj"
        );
        assert!(parse_repo(Gitlab, "proj").is_err());
        assert!(parse_repo(Gitlab, "a//b").is_err());
    }

    #[test]
    fn weights() {
        assert_eq!(parse_weight(" 0.5 ").unwrap(), 0.5);
        assert_eq!(parse_weight("100").unwrap(), 100.0);
        for bad in ["", "abc", "0", "-1", "100.1", "NaN", "inf"] {
            assert!(parse_weight(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
