//! Command-line flags, each also settable through a `DONARIUM_*` environment variable.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use jiff::SignedDuration;
use url::Url;

use crate::duration;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "donarium",
    version,
    about = "Track, rank and document donations to the open-source projects you depend on"
)]
pub struct Config {
    /// Address for the UI.
    #[arg(long, env = "DONARIUM_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Address for /metrics, /healthz and /readyz (no auth).
    #[arg(long, env = "DONARIUM_METRICS_LISTEN", default_value = "0.0.0.0:9090")]
    pub metrics_listen: SocketAddr,

    /// Public URL of the UI, e.g. https://donations.example.com. Used for the
    /// OIDC redirect, the Origin check on POSTs and links.
    #[arg(
        long,
        env = "DONARIUM_PUBLIC_URL",
        default_value = "http://localhost:8080"
    )]
    pub public_url: Url,

    /// Database URL. Only SQLite is supported: `sqlite:///data/donarium.db`.
    /// The file is created if it does not exist. Ignored with --demo.
    #[arg(
        long,
        env = "DONARIUM_DATABASE_URL",
        default_value = "sqlite://donarium.db"
    )]
    pub database_url: String,

    /// Base URL of the GitLab instance that GitLab sources live on.
    #[arg(
        long,
        env = "DONARIUM_GITLAB_URL",
        default_value = "https://gitlab.com"
    )]
    pub gitlab_url: Url,

    /// File with a GitLab access token (read_api scope) for private
    /// repositories. Public repositories need none. Sent only to --gitlab-url.
    #[arg(long, env = "DONARIUM_GITLAB_TOKEN_FILE")]
    pub gitlab_token_file: Option<PathBuf>,

    /// Base URL of a Gitea or Forgejo server that Gitea sources live on, e.g.
    /// https://git.example.com. Without it, Gitea sources cannot be added.
    #[arg(long, env = "DONARIUM_GITEA_URL")]
    pub gitea_url: Option<Url>,

    /// File with a Gitea or Forgejo access token (read:repository) for private
    /// repositories. Sent only to --gitea-url.
    #[arg(long, env = "DONARIUM_GITEA_TOKEN_FILE")]
    pub gitea_token_file: Option<PathBuf>,

    /// Base URL of a Paperless-ngx server to find receipts in, e.g.
    /// https://paperless.example.com. Receipts are linked, never copied.
    /// Needs --paperless-token-file.
    #[arg(
        long,
        env = "DONARIUM_PAPERLESS_URL",
        requires = "paperless_token_file"
    )]
    pub paperless_url: Option<Url>,

    /// File with a Paperless-ngx API token. Sent only to --paperless-url.
    #[arg(
        long,
        env = "DONARIUM_PAPERLESS_TOKEN_FILE",
        requires = "paperless_url"
    )]
    pub paperless_token_file: Option<PathBuf>,

    /// Endpoint of an S3-compatible service to keep uploaded receipts in, e.g.
    /// https://s3.eu-west-1.amazonaws.com. Needs the other --s3-* flags.
    #[arg(
        long,
        env = "DONARIUM_S3_ENDPOINT",
        requires_all = ["s3_bucket", "s3_access_key_file", "s3_secret_key_file"]
    )]
    pub s3_endpoint: Option<Url>,

    #[arg(long, env = "DONARIUM_S3_REGION", default_value = "us-east-1")]
    pub s3_region: String,

    #[arg(long, env = "DONARIUM_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// File with the access key id.
    #[arg(long, env = "DONARIUM_S3_ACCESS_KEY_FILE")]
    pub s3_access_key_file: Option<PathBuf>,

    /// File with the secret access key.
    #[arg(long, env = "DONARIUM_S3_SECRET_KEY_FILE")]
    pub s3_secret_key_file: Option<PathBuf>,

    /// Ask the storage to encrypt uploads (server-side encryption, AES256).
    #[arg(long, env = "DONARIUM_S3_SSE")]
    pub s3_sse: bool,

    /// API URL of GitHub.
    #[arg(
        long,
        env = "DONARIUM_GITHUB_API_URL",
        default_value = "https://api.github.com"
    )]
    pub github_api_url: Url,

    /// File with a GitHub token (read-only access to contents) for private
    /// repositories, and a higher rate limit. Sent only to --github-api-url.
    #[arg(long, env = "DONARIUM_GITHUB_TOKEN_FILE")]
    pub github_token_file: Option<PathBuf>,

    /// The currency totals are kept in, as a three-letter code. Fixed for a
    /// database the first time it is used.
    #[arg(long, env = "DONARIUM_BASE_CURRENCY", default_value = "EUR")]
    pub base_currency: String,

    /// Where exchange rates come from: a Frankfurter server, which serves the
    /// European Central Bank's daily rates.
    #[arg(
        long,
        env = "DONARIUM_FX_URL",
        default_value = "https://api.frankfurter.dev"
    )]
    pub fx_url: Url,

    /// Never look up exchange rates. Donations in other currencies then stay
    /// pending until a rate is entered by hand.
    #[arg(long, env = "DONARIUM_FX_DISABLED")]
    pub fx_disabled: bool,

    /// How often all sources are scanned. 0s turns scheduled scans off; scans
    /// can still be started from the UI.
    #[arg(long, env = "DONARIUM_SCAN_INTERVAL", default_value = "24h", value_parser = parse_duration)]
    pub scan_interval: SignedDuration,

    /// Registries to read image labels from, besides the built-in public ones
    /// (Docker Hub, ghcr.io, quay.io, …). Anonymous access only.
    #[arg(long, env = "DONARIUM_REGISTRY_ALLOW", value_delimiter = ',')]
    pub registry_allow: Vec<String>,

    #[command(flatten)]
    pub auth: AuthConfig,

    /// Serve sample data from memory, without a database. Uses
    /// --insecure-dev-auth unless OIDC is configured. For trying out the UI only.
    #[arg(long, env = "DONARIUM_DEMO")]
    pub demo: bool,

    /// Log as JSON.
    #[arg(long, env = "DONARIUM_LOG_JSON")]
    pub log_json: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct AuthConfig {
    /// OIDC issuer URL.
    #[arg(long, env = "DONARIUM_OIDC_ISSUER")]
    pub oidc_issuer: Option<Url>,

    #[arg(long, env = "DONARIUM_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    /// File containing the OIDC client secret.
    #[arg(long, env = "DONARIUM_OIDC_CLIENT_SECRET_FILE")]
    pub oidc_client_secret_file: Option<PathBuf>,

    /// Space-separated scopes to request besides `openid`.
    #[arg(
        long,
        env = "DONARIUM_OIDC_SCOPES",
        default_value = "profile email groups"
    )]
    pub oidc_scopes: String,

    /// ID token claim holding the user's groups or roles.
    #[arg(long, env = "DONARIUM_ADMIN_CLAIM", default_value = "groups")]
    pub admin_claim: String,

    /// Value of the admin claim that grants admin rights.
    #[arg(long, env = "DONARIUM_ADMIN_VALUE", default_value = "donarium-admins")]
    pub admin_value: String,

    /// Session lifetime.
    #[arg(long, env = "DONARIUM_SESSION_TTL", default_value = "8h", value_parser = parse_duration)]
    pub session_ttl: SignedDuration,

    /// If set, recording a donation requires a login no older than
    /// this (step-up auth).
    #[arg(long, env = "DONARIUM_STEP_UP_MAX_AGE", value_parser = parse_duration)]
    pub step_up_max_age: Option<SignedDuration>,

    /// File containing at least 64 random bytes (raw or base64) used to
    /// encrypt session cookies. Random per process if unset, which logs
    /// everyone out on restart.
    #[arg(long, env = "DONARIUM_COOKIE_KEY_FILE")]
    pub cookie_key_file: Option<PathBuf>,

    /// Skip OIDC and treat every visitor as an admin. Never use in production.
    #[arg(long, env = "DONARIUM_INSECURE_DEV_AUTH")]
    pub insecure_dev_auth: bool,
}

fn parse_duration(s: &str) -> Result<SignedDuration, String> {
    duration::parse(s).map_err(|e| e.to_string())
}

impl Config {
    /// Parses flags and environment, and exits with a usage error when they
    /// are inconsistent.
    pub fn load() -> Config {
        let cfg = Config::parse();
        if let Err(e) = cfg.check() {
            Config::command()
                .error(clap::error::ErrorKind::ArgumentConflict, e)
                .exit();
        }
        cfg
    }

    /// Checks between flags that clap cannot express.
    pub fn check(&self) -> Result<(), String> {
        if crate::money::parse_currency(&self.base_currency).as_deref()
            != Ok(self.base_currency.as_str())
        {
            return Err(format!(
                "--base-currency must be a three-letter upper-case code such as EUR (got {:?})",
                self.base_currency
            ));
        }
        for (flag, url) in [
            ("--gitlab-url", Some(&self.gitlab_url)),
            ("--gitea-url", self.gitea_url.as_ref()),
            ("--paperless-url", self.paperless_url.as_ref()),
            ("--s3-endpoint", self.s3_endpoint.as_ref()),
            ("--github-api-url", Some(&self.github_api_url)),
            ("--fx-url", Some(&self.fx_url)),
        ] {
            if let Some(url) = url
                && (!matches!(url.scheme(), "http" | "https") || url.host_str().is_none())
            {
                return Err(format!("{flag} must be an http(s) URL (got {url})"));
            }
        }
        if !self.database_url.starts_with("sqlite:") {
            return Err(format!(
                "--database-url must be a sqlite: URL (got {:?}); other databases are not supported yet",
                self.database_url
            ));
        }
        Ok(())
    }

    /// `scheme://host[:port]` of the public URL, as sent in `Origin` headers.
    pub fn public_origin(&self) -> String {
        self.public_url.origin().ascii_serialization()
    }

    /// Where sources live, for validating repository URLs and linking to them.
    pub fn forge_hosts(&self) -> crate::model::ForgeHosts {
        crate::model::ForgeHosts {
            gitlab: self.gitlab_url.clone(),
            gitea: self.gitea_url.clone(),
        }
    }

    /// Absolute URL for a path under the public URL.
    pub fn url_for(&self, path: &str) -> String {
        let base = self.public_url.as_str().trim_end_matches('/');
        format!("{base}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg = Config::try_parse_from(["donarium"]).unwrap();
        assert!(cfg.check().is_ok());
        assert_eq!(cfg.database_url, "sqlite://donarium.db");
        assert_eq!(cfg.gitlab_url.as_str(), "https://gitlab.com/");
        assert_eq!(cfg.scan_interval, SignedDuration::from_hours(24));
        assert_eq!(cfg.fx_url.as_str(), "https://api.frankfurter.dev/");
        assert!(!cfg.fx_disabled);
        assert!(cfg.registry_allow.is_empty());
        assert_eq!(cfg.public_origin(), "http://localhost:8080");
        assert_eq!(
            cfg.url_for("/projects/a"),
            "http://localhost:8080/projects/a"
        );
    }

    #[test]
    fn the_base_currency_must_be_a_currency_code() {
        let ok = Config::try_parse_from(["donarium", "--base-currency", "USD"]).unwrap();
        assert!(ok.check().is_ok());
        assert_eq!(
            Config::try_parse_from(["donarium"]).unwrap().base_currency,
            "EUR"
        );
        for bad in ["eur", "EURO", "E1R", ""] {
            let cfg = Config::try_parse_from(["donarium", "--base-currency", bad]).unwrap();
            assert!(
                cfg.check().unwrap_err().contains("--base-currency"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn forge_urls_must_be_http() {
        for (flag, value) in [
            ("--gitlab-url", "ftp://gitlab.example.com"),
            ("--gitea-url", "file:///etc"),
            ("--github-api-url", "git://github.com"),
            ("--fx-url", "ftp://rates.example.com"),
        ] {
            let cfg = Config::try_parse_from(["donarium", flag, value]).unwrap();
            assert!(cfg.check().unwrap_err().contains(flag), "{flag}");
        }
        let cfg =
            Config::try_parse_from(["donarium", "--gitea-url", "https://git.example.com:3000"])
                .unwrap();
        assert!(cfg.check().is_ok());
        let hosts = cfg.forge_hosts();
        assert_eq!(
            hosts.host(crate::model::Provider::Gitea).as_deref(),
            Some("git.example.com:3000")
        );
        assert_eq!(
            hosts.host(crate::model::Provider::Github).as_deref(),
            Some("github.com")
        );
        assert_eq!(
            hosts
                .repo_url(crate::model::Provider::Gitea, "me/flux")
                .as_deref(),
            Some("https://git.example.com:3000/me/flux")
        );
        assert_eq!(hosts.available().len(), 3);
        // Without a Gitea URL the provider is not available.
        let none = Config::try_parse_from(["donarium"]).unwrap().forge_hosts();
        assert_eq!(
            none.available(),
            [
                crate::model::Provider::Github,
                crate::model::Provider::Gitlab
            ]
        );
        assert_eq!(none.repo_url(crate::model::Provider::Gitea, "a/b"), None);
    }

    #[test]
    fn only_sqlite_urls_are_accepted() {
        let cfg = Config::try_parse_from(["donarium", "--database-url", "postgres://db/donarium"])
            .unwrap();
        assert!(cfg.check().unwrap_err().contains("sqlite:"));
        let cfg =
            Config::try_parse_from(["donarium", "--database-url", "sqlite:///data/d.db"]).unwrap();
        assert!(cfg.check().is_ok());
    }
}
