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

/// Where a project came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Found by a scan. Removed again when no source uses it.
    Discovered,
    /// Added by an admin. Stays until an admin deletes it.
    Manual,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Discovered => "discovered",
            Origin::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Origin> {
        match s {
            "discovered" => Some(Origin::Discovered),
            "manual" => Some(Origin::Manual),
            _ => None,
        }
    }
}

/// One way to donate to a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funding {
    pub platform: String,
    pub handle: String,
    pub url: Option<String>,
    /// Added by an admin, as opposed to discovered.
    pub manual: bool,
}

/// An open-source project you depend on, or want to support.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    /// Stable slug, used in URLs (e.g. `tokio-rs-tokio`).
    pub name: String,
    /// Canonical repository, e.g. `github.com/tokio-rs/tokio`.
    pub repo: String,
    pub origin: Origin,
    /// What scans found, plus `manual_usage`.
    pub usage_score: f64,
    /// The part of the score an admin assigned.
    pub manual_usage: f64,
    /// Left out of the ranking, e.g. a company-backed project.
    pub ignored: bool,
    pub notes: String,
    pub last_donation: Option<Timestamp>,
    /// Donations that count (not voided), and what they add up to in the base
    /// currency (those still waiting for a conversion are left out).
    pub donation_count: i64,
    pub donated_base_minor: i64,
    pub funding: Vec<Funding>,
}

/// An admin's settings for a project.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectUpdate {
    pub manual_usage: f64,
    pub ignore: bool,
    pub notes: String,
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

/// Whether a donation was one payment or part of a regular one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DonationKind {
    OneOff,
    Recurring,
}

impl DonationKind {
    pub const ALL: [DonationKind; 2] = [DonationKind::OneOff, DonationKind::Recurring];

    pub fn as_str(self) -> &'static str {
        match self {
            DonationKind::OneOff => "one_off",
            DonationKind::Recurring => "recurring",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DonationKind::OneOff => "one-off",
            DonationKind::Recurring => "recurring",
        }
    }

    pub fn parse(s: &str) -> Option<DonationKind> {
        DonationKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// How often a recurring donation is made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Every {
    #[default]
    Monthly,
    Yearly,
}

impl Every {
    pub const ALL: [Every; 2] = [Every::Monthly, Every::Yearly];

    pub fn as_str(self) -> &'static str {
        match self {
            Every::Monthly => "monthly",
            Every::Yearly => "yearly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Every::Monthly => "every month",
            Every::Yearly => "every year",
        }
    }

    pub fn parse(s: &str) -> Option<Every> {
        Every::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// A recurring donation to set up (already validated).
#[derive(Clone, Debug, PartialEq)]
pub struct NewRecurring {
    pub project: String,
    pub platform: String,
    pub amount_minor: i64,
    pub currency: String,
    pub every: Every,
    pub starts_on: jiff::civil::Date,
    pub note: String,
}

/// A monthly or yearly donation that records itself when it is due.
#[derive(Clone, Debug, PartialEq)]
pub struct Recurring {
    pub id: i64,
    pub project: String,
    pub project_repo: String,
    pub platform: String,
    pub amount_minor: i64,
    pub currency: String,
    pub every: Every,
    pub starts_on: jiff::civil::Date,
    /// The last day it can fall on; `None` while it keeps running.
    pub ends_on: Option<jiff::civil::Date>,
    pub note: String,
    pub created_by: Actor,
    /// Donations it has recorded so far.
    pub recorded: i64,
}

/// Where a conversion rate came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxSource {
    /// The donation was already in the base currency.
    Same,
    /// The European Central Bank's reference rate for that day.
    Ecb,
    /// An admin typed it, e.g. what the card was really charged.
    Manual,
}

impl FxSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FxSource::Same => "same",
            FxSource::Ecb => "ecb",
            FxSource::Manual => "manual",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FxSource::Same => "same currency",
            FxSource::Ecb => "ECB rate",
            FxSource::Manual => "entered by hand",
        }
    }

    pub fn parse(s: &str) -> Option<FxSource> {
        [FxSource::Same, FxSource::Ecb, FxSource::Manual]
            .into_iter()
            .find(|f| f.as_str() == s)
    }
}

/// A donation's amount in the base currency, and how it was worked out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Conversion {
    pub base_minor: i64,
    /// Units of the base currency per unit of the donated one.
    pub rate: f64,
    pub source: FxSource,
}

/// A donation about to be recorded (already validated).
#[derive(Clone, Debug, PartialEq)]
pub struct NewDonation {
    /// The project's name (its slug).
    pub project: String,
    pub platform: String,
    pub amount_minor: i64,
    pub currency: String,
    pub donated_on: jiff::civil::Date,
    pub kind: DonationKind,
    pub note: String,
    /// `None` while the conversion is pending.
    pub conversion: Option<Conversion>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Donation {
    pub id: i64,
    pub project: String,
    pub project_repo: String,
    pub platform: String,
    pub amount_minor: i64,
    pub currency: String,
    pub donated_on: jiff::civil::Date,
    pub conversion: Option<Conversion>,
    pub kind: DonationKind,
    pub note: String,
    pub recorded_by: Actor,
    pub recorded_at: Timestamp,
    /// Who voided it and when, if it was.
    pub voided: Option<(Actor, Timestamp)>,
}

/// What the donations that count add up to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub count: usize,
    /// In the base currency; donations still waiting for a conversion are not in it.
    pub base_minor: i64,
    pub unconverted: usize,
    /// In the currencies they were paid in, largest first.
    pub by_currency: Vec<(String, i64)>,
    /// In the base currency, newest year first.
    pub by_year: Vec<(i32, i64)>,
}

/// A file that was uploaded to the bucket, about to be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUpload {
    pub key: String,
    pub filename: String,
    pub content_type: String,
    pub size: i64,
    pub sha256: String,
}

/// Where a receipt lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptStore {
    /// A document in Paperless-ngx, linked by id.
    Paperless,
    /// A file uploaded to the S3 bucket.
    S3,
}

impl ReceiptStore {
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptStore::Paperless => "paperless",
            ReceiptStore::S3 => "s3",
        }
    }
}

/// What a donation points to as its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: i64,
    pub donation_id: i64,
    pub store: ReceiptStore,
    /// Paperless: the document's id. S3: the object's key.
    pub reference: String,
    /// The document's title, or the uploaded file's name.
    pub title: String,
    pub content_type: Option<String>,
    pub linked_by: String,
}
