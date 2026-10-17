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

/// Where a source repository is hosted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Github,
    Gitlab,
    /// Gitea, and Forgejo (whose API is compatible).
    Gitea,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Github, Provider::Gitlab, Provider::Gitea];

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Gitlab => "gitlab",
            Provider::Gitea => "gitea",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// The hosts that sources live on. They come from the server's configuration,
/// never from what is typed into a form, so a source cannot make the server
/// send a token (or any request) somewhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeHosts {
    pub gitlab: url::Url,
    /// `None` when no Gitea or Forgejo is configured.
    pub gitea: Option<url::Url>,
}

impl ForgeHosts {
    /// The web address of the provider: `https://github.com`, or the configured one.
    pub fn base(&self, provider: Provider) -> Option<url::Url> {
        match provider {
            Provider::Github => url::Url::parse("https://github.com").ok(),
            Provider::Gitlab => Some(self.gitlab.clone()),
            Provider::Gitea => self.gitea.clone(),
        }
    }

    /// `host` or `host:port`, as it appears in repository URLs.
    pub fn host(&self, provider: Provider) -> Option<String> {
        let base = self.base(provider)?;
        let host = base.host_str()?;
        Some(match base.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        })
    }

    /// The page of a repository on its host.
    pub fn repo_url(&self, provider: Provider, repo: &str) -> Option<String> {
        let base = self.base(provider)?;
        Some(format!("{}/{repo}", base.as_str().trim_end_matches('/')))
    }

    /// The providers that can be used on this server.
    pub fn available(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|p| self.base(*p).is_some())
            .collect()
    }
}

/// How a source's dependencies are collected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    /// Everything that is implemented for the provider.
    Auto,
    /// The container images in Compose and Kubernetes (Flux) files.
    Images,
    DependencyGraph,
    Sbom,
    Lockfiles,
}

impl ScanMethod {
    pub const ALL: [ScanMethod; 5] = [
        ScanMethod::Auto,
        ScanMethod::Images,
        ScanMethod::DependencyGraph,
        ScanMethod::Sbom,
        ScanMethod::Lockfiles,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ScanMethod::Auto => "auto",
            ScanMethod::Images => "images",
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
    /// Whether the last scan worked; `None` before the first one.
    pub last_scan_ok: Option<bool>,
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
