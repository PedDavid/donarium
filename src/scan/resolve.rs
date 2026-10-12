//! Rules for turning an image into the upstream project it comes from, apart
//! from the registry lookup itself (see [`super::registry`]).
//!
//! Projects are identified by a canonical repository, lower-case and without a
//! scheme: `github.com/tokio-rs/tokio`.

use url::Url;

/// Registries that are looked up for image labels. Anything else could be an
/// internal host named in a repository file, and the server must not be
/// steered towards arbitrary addresses by file contents. More can be added
/// with `--registry-allow`.
pub const DEFAULT_REGISTRIES: &[&str] = &[
    "docker.io",
    "ghcr.io",
    "lscr.io",
    "quay.io",
    "registry.gitlab.com",
    "gcr.io",
    "registry.k8s.io",
    "k8s.gcr.io",
    "mcr.microsoft.com",
    "public.ecr.aws",
    "docker.elastic.co",
    "codeberg.org",
];

/// Hosts whose repositories are always `owner/name`: anything after the second
/// path segment is a folder, branch or file inside it.
const TWO_SEGMENT_HOSTS: &[&str] = &["github.com", "codeberg.org", "bitbucket.org", "git.sr.ht"];

/// The canonical repository for a URL found in an image label, or `None` if it
/// is not a repository URL.
pub fn canonical_repo(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("git+").unwrap_or(raw);
    let url = if let Some(rest) = raw.strip_prefix("git@") {
        // scp-style: git@github.com:owner/name.git
        let (host, path) = rest.split_once(':')?;
        Url::parse(&format!("https://{host}/{path}")).ok()?
    } else {
        Url::parse(raw).ok()?
    };
    if !matches!(url.scheme(), "http" | "https" | "git" | "ssh") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" || !host.contains('.') {
        return None;
    }
    let mut segments: Vec<String> = url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if TWO_SEGMENT_HOSTS.contains(&host.as_str()) {
        segments.truncate(2);
    } else if let Some(i) = segments
        .iter()
        .position(|s| matches!(s.as_str(), "-" | "tree" | "blob" | "src"))
    {
        // GitLab-style hosts: `group/sub/project/-/tree/main`.
        segments.truncate(i);
    }
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".git")
    {
        *last = stripped.to_string();
    }
    let valid = |s: &String| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if segments.len() < 2 || !segments.iter().all(valid) {
        return None;
    }
    Some(format!("{host}/{}", segments.join("/")))
}

/// Normalises a repository typed by an admin: `github.com/a/b`,
/// `https://github.com/a/b` and `a/b` on a known host all work, but a bare
/// `owner/name` is ambiguous and is rejected.
pub fn parse_project_repo(input: &str) -> Option<String> {
    let input = input.trim();
    let with_scheme = if input.contains("://") || input.starts_with("git@") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    canonical_repo(&with_scheme)
}

/// A guess from the image's name alone, for when the registry gives no label.
/// `None` where the name says nothing about the source.
pub fn guess_from_name(registry: &str, repository: &str) -> Option<String> {
    let mut parts = repository.split('/');
    let (first, second) = (parts.next()?, parts.next());
    match (registry, first, second) {
        // linuxserver.io names its packaging repositories `docker-<app>`.
        ("lscr.io" | "ghcr.io" | "docker.io", "linuxserver", Some(app)) => {
            Some(format!("github.com/linuxserver/docker-{app}"))
        }
        ("ghcr.io", owner, Some(name)) => Some(format!("github.com/{owner}/{name}")),
        ("registry.gitlab.com", group, Some(project)) => {
            Some(format!("gitlab.com/{group}/{project}"))
        }
        ("codeberg.org", owner, Some(name)) => Some(format!("codeberg.org/{owner}/{name}")),
        _ => None,
    }
}

pub fn registry_allowed(registry: &str, extra: &[String]) -> bool {
    DEFAULT_REGISTRIES.contains(&registry) || extra.iter().any(|r| r == registry)
}

/// The URL slug of a project: its path, lower-case, `/` becoming `-`
/// (`github.com/tokio-rs/tokio` is `tokio-rs-tokio`).
pub fn slug(project_repo: &str, include_host: bool) -> String {
    let source = if include_host {
        project_repo
    } else {
        project_repo
            .split_once('/')
            .map_or(project_repo, |(_, p)| p)
    };
    let mut out = String::new();
    for c in source.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_urls_become_canonical_repositories() {
        let c = |s: &str| canonical_repo(s);
        assert_eq!(
            c("https://github.com/Jellyfin/Jellyfin").as_deref(),
            Some("github.com/jellyfin/jellyfin")
        );
        assert_eq!(
            c("https://github.com/jellyfin/jellyfin.git").as_deref(),
            Some("github.com/jellyfin/jellyfin")
        );
        assert_eq!(
            c("https://github.com/o/r/tree/main/docker").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            c("git+https://github.com/o/r.git").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            c("git@github.com:o/r.git").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            c("https://www.github.com/o/r/").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            c("https://gitlab.com/group/sub/proj/-/tree/main").as_deref(),
            Some("gitlab.com/group/sub/proj")
        );
        assert_eq!(
            c("https://git.example.org/team/app").as_deref(),
            Some("git.example.org/team/app")
        );
        for bad in [
            "",
            "not a url",
            "https://github.com/onlyowner",
            "https://example.com",
            "ftp://github.com/o/r",
            "https://localhost/o/r",
            "http://127.0.0.1/o/r",
            "http://10.0.0.5/o/r",
            "javascript:alert(1)",
            "https://github.com/o/r with space",
            "https://github.com/%3Cscript%3E/r",
        ] {
            assert_eq!(c(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn typed_project_repositories() {
        assert_eq!(
            parse_project_repo("github.com/Tokio-rs/tokio").as_deref(),
            Some("github.com/tokio-rs/tokio")
        );
        assert_eq!(
            parse_project_repo(" https://gitlab.com/g/p ").as_deref(),
            Some("gitlab.com/g/p")
        );
        // Without a host it is ambiguous.
        assert_eq!(parse_project_repo("tokio-rs/tokio"), None);
        assert_eq!(parse_project_repo(""), None);
    }

    #[test]
    fn guesses_from_the_image_name() {
        assert_eq!(
            guess_from_name("ghcr.io", "paperless-ngx/paperless-ngx").as_deref(),
            Some("github.com/paperless-ngx/paperless-ngx")
        );
        assert_eq!(
            guess_from_name("ghcr.io", "o/n/sub").as_deref(),
            Some("github.com/o/n")
        );
        assert_eq!(
            guess_from_name("lscr.io", "linuxserver/sonarr").as_deref(),
            Some("github.com/linuxserver/docker-sonarr")
        );
        assert_eq!(
            guess_from_name("docker.io", "linuxserver/radarr").as_deref(),
            Some("github.com/linuxserver/docker-radarr")
        );
        assert_eq!(
            guess_from_name("registry.gitlab.com", "grp/proj/img").as_deref(),
            Some("gitlab.com/grp/proj")
        );
        assert_eq!(guess_from_name("docker.io", "jellyfin/jellyfin"), None);
        assert_eq!(guess_from_name("quay.io", "prometheus/prometheus"), None);
        assert_eq!(guess_from_name("ghcr.io", "single"), None);
    }

    #[test]
    fn only_known_registries_are_looked_up() {
        assert!(registry_allowed("docker.io", &[]));
        assert!(registry_allowed("ghcr.io", &[]));
        assert!(!registry_allowed("registry.internal:5000", &[]));
        assert!(!registry_allowed("localhost", &[]));
        assert!(!registry_allowed("169.254.169.254", &[]));
        assert!(registry_allowed(
            "registry.example.com",
            &["registry.example.com".to_string()]
        ));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("github.com/tokio-rs/tokio", false), "tokio-rs-tokio");
        assert_eq!(slug("gitlab.com/g/sub/p", false), "g-sub-p");
        assert_eq!(slug("gitlab.com/g/p", true), "gitlab-com-g-p");
        assert_eq!(slug("github.com/o/a.b_c", false), "o-a-b-c");
    }
}
