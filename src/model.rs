//! Domain types shared by the storage layer and the UI.

use jiff::Timestamp;

/// Who did something. `sub` is the stable OIDC subject and identifies the
/// person; `name` is only a display label and may change or collide.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Actor {
    pub sub: String,
    pub name: String,
}

impl Actor {
    pub fn new(sub: impl Into<String>, name: impl Into<String>) -> Self {
        Actor {
            sub: sub.into(),
            name: name.into(),
        }
    }
}

/// The display name, or the subject when there is none.
impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.name.is_empty() {
            f.write_str(&self.sub)
        } else {
            f.write_str(&self.name)
        }
    }
}

/// An upstream open-source project you depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    /// Stable slug, used in URLs (e.g. `tokio-rs-tokio`).
    pub name: String,
    /// Canonical repository, e.g. `github.com/tokio-rs/tokio`.
    pub repo: String,
    pub usage_score: f64,
    pub last_donation: Option<Timestamp>,
    /// Funding platforms found for the project (e.g. `github-sponsors`).
    pub funding: Vec<String>,
}
