//! Deciding which project an image belongs to, from what is known about it.

use super::resolve::guess_from_name;

/// What the registry lookup for an image's repository found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    /// Canonical project repository from the label.
    pub upstream: Option<String>,
    pub label: Option<String>,
    /// Why there is no label, when there is not.
    pub error: Option<String>,
    /// The failure is temporary (a rate limit): ask again at the next scan.
    pub retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// An admin said so.
    Mapping,
    /// The image's own `org.opencontainers.image.source` label.
    Label,
    /// Guessed from the image's name (e.g. `ghcr.io/owner/name`).
    Guess,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::Mapping => "mapped by an admin",
            How::Label => "from the image's source label",
            How::Guess => "guessed from the image name",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub project: Option<String>,
    pub how: Option<How>,
    /// When there is no project, or only a guess: what is missing.
    pub note: Option<String>,
}

/// An admin's mapping wins, then the label, then a guess from the name.
pub fn resolve(
    registry: &str,
    repository: &str,
    mapping: Option<&str>,
    lookup: Option<&Lookup>,
) -> Resolved {
    if let Some(project) = mapping {
        return Resolved {
            project: Some(project.to_string()),
            how: Some(How::Mapping),
            note: None,
        };
    }
    if let Some(project) = lookup.and_then(|l| l.upstream.as_deref()) {
        return Resolved {
            project: Some(project.to_string()),
            how: Some(How::Label),
            note: None,
        };
    }
    let why = match lookup {
        None => "not looked up yet".to_string(),
        Some(l) => l
            .error
            .clone()
            .unwrap_or_else(|| "the image has no source label".to_string()),
    };
    match guess_from_name(registry, repository) {
        Some(project) => Resolved {
            project: Some(project),
            how: Some(How::Guess),
            note: Some(why),
        },
        None => Resolved {
            project: None,
            how: None,
            note: Some(why),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(upstream: Option<&str>, error: Option<&str>) -> Lookup {
        Lookup {
            upstream: upstream.map(str::to_string),
            label: upstream.map(|u| format!("https://{u}")),
            error: error.map(str::to_string),
            retry: false,
        }
    }

    #[test]
    fn mapping_beats_label_beats_guess() {
        let l = lookup(Some("github.com/jellyfin/jellyfin"), None);
        let r = resolve(
            "ghcr.io",
            "linuxserver/jellyfin",
            Some("github.com/me/pick"),
            Some(&l),
        );
        assert_eq!(
            (r.project.as_deref(), r.how),
            (Some("github.com/me/pick"), Some(How::Mapping))
        );
        let r = resolve("ghcr.io", "linuxserver/jellyfin", None, Some(&l));
        assert_eq!(
            (r.project.as_deref(), r.how),
            (Some("github.com/jellyfin/jellyfin"), Some(How::Label))
        );
        let none = lookup(None, None);
        let r = resolve("ghcr.io", "linuxserver/jellyfin", None, Some(&none));
        assert_eq!(
            (r.project.as_deref(), r.how),
            (
                Some("github.com/linuxserver/docker-jellyfin"),
                Some(How::Guess)
            )
        );
        assert_eq!(r.note.as_deref(), Some("the image has no source label"));
    }

    #[test]
    fn unresolved_images_say_why() {
        let r = resolve("docker.io", "qmcgaw/gluetun", None, None);
        assert_eq!(r.project, None);
        assert_eq!(r.note.as_deref(), Some("not looked up yet"));
        let failed = lookup(None, Some("image not found (it may be private)"));
        let r = resolve("docker.io", "qmcgaw/gluetun", None, Some(&failed));
        assert_eq!(r.project, None);
        assert_eq!(
            r.note.as_deref(),
            Some("image not found (it may be private)")
        );
    }
}
