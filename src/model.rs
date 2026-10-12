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

/// Where a source repository is hosted. The hosts are fixed in code, so user
/// input can never point a scan (and its credentials) somewhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Github,
    Gitlab,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Github, Provider::Gitlab];

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Gitlab => "gitlab",
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Provider::Github => "github.com",
            Provider::Gitlab => "gitlab.com",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// How a source's dependencies are collected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    /// The best method the provider offers.
    Auto,
    DependencyGraph,
    Sbom,
    Lockfiles,
}

impl ScanMethod {
    pub const ALL: [ScanMethod; 4] = [
        ScanMethod::Auto,
        ScanMethod::DependencyGraph,
        ScanMethod::Sbom,
        ScanMethod::Lockfiles,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ScanMethod::Auto => "auto",
            ScanMethod::DependencyGraph => "dependency-graph",
            ScanMethod::Sbom => "sbom",
            ScanMethod::Lockfiles => "lockfiles",
        }
    }

    pub fn parse(s: &str) -> Option<ScanMethod> {
        ScanMethod::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// A repository of yours that is scanned for dependencies.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: i64,
    pub provider: Provider,
    /// `owner/name` (GitLab: `group/subgroup/name`), as the user wrote it.
    pub repo: String,
    /// How much this repository counts towards a project's usage.
    pub weight: f64,
    pub scan_method: ScanMethod,
    pub enabled: bool,
    pub last_scan_at: Option<Timestamp>,
    pub last_scan_status: Option<String>,
    pub created_by: Actor,
    pub created_at: Timestamp,
    pub updated_by: Actor,
    pub updated_at: Timestamp,
}

/// A validated source, ready to be stored (see [`crate::validation`]).
#[derive(Clone, Debug, PartialEq)]
pub struct NewSource {
    pub provider: Provider,
    pub repo: String,
    pub weight: f64,
    pub scan_method: ScanMethod,
}

/// The settings of a source that can be changed after it was added.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceUpdate {
    pub weight: f64,
    pub scan_method: ScanMethod,
    pub enabled: bool,
}

/// Share of a transitive dependency's weight, compared to a direct one.
pub const TRANSITIVE_FACTOR: f64 = 0.25;
