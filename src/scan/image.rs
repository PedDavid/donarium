//! Container image references, normalised the way Docker does it, so that
//! `nginx`, `docker.io/library/nginx:latest` and `index.docker.io/nginx` are
//! the same repository.

use std::fmt;

pub const DOCKER_HUB: &str = "docker.io";

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageRef {
    /// Lower-case registry host, `docker.io` for Docker Hub.
    pub registry: String,
    /// Repository path without the registry, e.g. `library/nginx` or
    /// `linuxserver/sonarr`.
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("empty image reference")]
    Empty,
    #[error("the image name depends on a variable")]
    Templated,
    #[error("not a valid image reference: {0}")]
    Invalid(String),
}

impl ImageRef {
    /// Parses a complete reference such as `ghcr.io/owner/app:1.2@sha256:…`.
    pub fn parse(input: &str) -> Result<ImageRef, ImageError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ImageError::Empty);
        }
        if input.contains("${") || input.contains("{{") || input.contains("$(") {
            return Err(ImageError::Templated);
        }
        let invalid = || ImageError::Invalid(input.to_string());
        if input.chars().any(char::is_whitespace) {
            return Err(invalid());
        }

        let (rest, digest) = match input.split_once('@') {
            Some((rest, digest)) => (rest, Some(digest)),
            None => (input, None),
        };
        if let Some(d) = digest
            && !valid_digest(d)
        {
            return Err(invalid());
        }
        // A tag is after the last ':' that follows the last '/' (an earlier
        // ':' belongs to a registry port).
        let last_slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => (&rest[..last_slash + i], Some(&rest[last_slash + i + 1..])),
            None => (rest, None),
        };
        if let Some(t) = tag
            && !valid_tag(t)
        {
            return Err(invalid());
        }

        let mut parts = name.split('/');
        let first = parts.next().unwrap_or_default();
        let remaining: Vec<&str> = parts.collect();
        let (registry, path): (String, Vec<&str>) = if !remaining.is_empty()
            && (first.contains('.') || first.contains(':') || first == "localhost")
        {
            (first.to_ascii_lowercase(), remaining)
        } else {
            let mut all = vec![first];
            all.extend(remaining);
            (DOCKER_HUB.to_string(), all)
        };
        let registry = match registry.as_str() {
            "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
                DOCKER_HUB.to_string()
            }
            _ => registry,
        };
        if path.is_empty() || !path.iter().all(|c| valid_component(c)) {
            return Err(invalid());
        }
        let mut repository = path.join("/");
        if registry == DOCKER_HUB && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        Ok(ImageRef {
            registry,
            repository,
            tag: tag.map(str::to_string),
            digest: digest.map(str::to_string),
        })
    }

    /// Parses a reference from a compose or manifest file, where variables are
    /// common. `${VAR:-default}` uses its default. A variable in the tag or
    /// digest only drops the tag (the repository is still known); a variable
    /// in the name makes the reference unusable.
    pub fn parse_lenient(input: &str) -> Result<ImageRef, ImageError> {
        let substituted = substitute_defaults(input.trim());
        let Some(var) = substituted.find(['$', '{']) else {
            return ImageRef::parse(&substituted);
        };
        let last_slash = substituted.rfind('/').map_or(0, |i| i + 1);
        // Only a variable after the last '/' and after a ':' or '@' leaves the
        // repository intact.
        let separator = substituted[..var]
            .rfind([':', '@'])
            .filter(|i| *i >= last_slash);
        match separator {
            Some(i) => ImageRef::parse(&substituted[..i]),
            None => Err(ImageError::Templated),
        }
    }

    /// `registry/repository`, without tag or digest.
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// Replaces `${VAR:-default}`, `${VAR-default}` and `${VAR:=default}` by their
/// default. Other variable forms are left for the caller to deal with.
fn substitute_defaults(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let default = [":-", ":=", "-", "="]
            .iter()
            .find_map(|op| body.split_once(op).map(|(_, d)| d));
        match default {
            Some(d) if !d.contains("${") => out.push_str(d),
            _ => out.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn valid_component(c: &str) -> bool {
    // Lower-case alphanumerics separated by single '.', '_' , '__' or runs of '-'.
    !c.is_empty()
        && c.len() <= 128
        && c.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
        && c.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && c.bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && !t.starts_with(['.', '-'])
}

fn valid_digest(d: &str) -> bool {
    match d.split_once(':') {
        Some((algorithm, hex)) => {
            !algorithm.is_empty()
                && algorithm
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'+')
                && hex.len() >= 32
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn docker_hub_defaults() {
        let r = parsed("nginx");
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("docker.io", "library/nginx")
        );
        assert_eq!(r.tag, None);
        assert_eq!(parsed("nginx:1.27").tag.as_deref(), Some("1.27"));
        assert_eq!(
            parsed("linuxserver/sonarr:latest").name(),
            "docker.io/linuxserver/sonarr"
        );
        for same in [
            "docker.io/library/nginx",
            "index.docker.io/library/nginx",
            "registry-1.docker.io/nginx",
            "nginx",
        ] {
            assert_eq!(parsed(same).name(), "docker.io/library/nginx", "{same}");
        }
    }

    #[test]
    fn other_registries() {
        // Repositories are lower-case in a valid reference.
        assert!(ImageRef::parse("ghcr.io/Owner/app:v1.2.3").is_err());
        let r = parsed("ghcr.io/owner/app:v1.2.3");
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("ghcr.io", "owner/app")
        );
        assert_eq!(
            parsed("quay.io/prometheus/prometheus:v2").registry,
            "quay.io"
        );
        assert_eq!(parsed("registry.k8s.io/pause:3.9").repository, "pause");
        assert_eq!(
            parsed("registry.example.com:5000/team/app:1").registry,
            "registry.example.com:5000"
        );
        assert_eq!(parsed("localhost/app").registry, "localhost");
        assert_eq!(
            parsed("registry.gitlab.com/group/sub/app:1").repository,
            "group/sub/app"
        );
    }

    #[test]
    fn digests_and_tags() {
        let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let r = parsed(&format!("ghcr.io/o/a:1.0@{digest}"));
        assert_eq!(r.tag.as_deref(), Some("1.0"));
        assert_eq!(r.digest.as_deref(), Some(digest));
        let r = parsed(&format!("ghcr.io/o/a@{digest}"));
        assert_eq!(r.tag, None);
        assert_eq!(r.to_string(), format!("ghcr.io/o/a@{digest}"));
        assert_eq!(parsed("registry:5000/a:2").tag.as_deref(), Some("2"));
        assert_eq!(parsed("registry.io:5000/a").tag, None);
    }

    #[test]
    fn invalid_references() {
        for bad in [
            "",
            "   ",
            "a b",
            "ghcr.io/",
            "/a",
            "a//b",
            "a:",
            "a:-x",
            "UPPER/case",
            "a@sha256:xyz",
            "a@nodigest",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            ImageRef::parse("${REGISTRY}/app"),
            Err(ImageError::Templated)
        );
        assert_eq!(
            ImageRef::parse("{{ .Values.image }}"),
            Err(ImageError::Templated)
        );
    }

    #[test]
    fn lenient_parsing_of_variables() {
        let lenient = |s: &str| ImageRef::parse_lenient(s);
        assert_eq!(
            lenient("jellyfin/jellyfin:${JF_TAG:-10.9}")
                .unwrap()
                .tag
                .as_deref(),
            Some("10.9")
        );
        assert_eq!(
            lenient("${REG:-ghcr.io}/o/a:1").unwrap().name(),
            "ghcr.io/o/a"
        );
        // An unknown tag still tells us the repository.
        let r = lenient("lscr.io/linuxserver/sonarr:${SONARR_TAG}").unwrap();
        assert_eq!(r.name(), "lscr.io/linuxserver/sonarr");
        assert_eq!(r.tag, None);
        let r = lenient("ghcr.io/o/a@${DIGEST}").unwrap();
        assert_eq!((r.name().as_str(), r.digest), ("ghcr.io/o/a", None));
        // An unknown name does not.
        assert_eq!(lenient("${IMAGE}"), Err(ImageError::Templated));
        assert_eq!(lenient("${REGISTRY}/app:1"), Err(ImageError::Templated));
        assert_eq!(
            lenient("ghcr.io/${OWNER}/app:1"),
            Err(ImageError::Templated)
        );
        assert_eq!(lenient("plain:1").unwrap().tag.as_deref(), Some("1"));
    }
}
