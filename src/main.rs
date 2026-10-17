use std::sync::Arc;

use anyhow::Context;
use axum_extra::extract::cookie::Key;
use base64::Engine as _;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use donarium::config::Config;
use donarium::demo;
use donarium::metrics::Metrics;
use donarium::model::Provider;
use donarium::scan::fetch::ArchiveFetcher;
use donarium::scan::registry::RegistryClient;
use donarium::scan::run::{Scanner, spawn_scheduler};
use donarium::store::Store;
use donarium::web::auth::AuthMode;
use donarium::web::{self, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    init_tracing(cfg.log_json);

    let store = if cfg.demo {
        warn!("demo mode: serving sample data from memory");
        let store = Store::memory().await?;
        demo::seed(&store, jiff::Timestamp::now()).await?;
        store
    } else {
        info!(database = %cfg.database_url, "opening the database");
        Store::connect(&cfg.database_url)
            .await
            .with_context(|| format!("opening {}", cfg.database_url))?
    };
    store
        .ensure_base_currency(&cfg.base_currency)
        .await
        .context("checking the base currency")?;
    let store = Arc::new(store);
    let scanner = Arc::new(build_scanner(&cfg, store.clone())?);
    if !cfg.demo && cfg.scan_interval.is_positive() {
        spawn_scheduler(
            scanner.clone(),
            std::time::Duration::from_secs(cfg.scan_interval.as_secs() as u64),
        );
    }
    let metrics = Metrics::new();
    let demo_dev_auth = cfg.demo && cfg.auth.oidc_issuer.is_none();
    let auth = AuthMode::from_config(&cfg.auth, &cfg.public_url, demo_dev_auth)?;
    let cookie_key = load_cookie_key(&cfg)?;

    let ops = web::ops_router(store.clone(), metrics, cfg.base_currency.clone());
    let state = AppState::new(cfg.clone(), store, scanner, auth, cookie_key);
    let app = web::router(state);

    let ui_listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    let ops_listener = tokio::net::TcpListener::bind(cfg.metrics_listen).await?;
    info!(ui = %cfg.listen, ops = %cfg.metrics_listen, "listening");
    tokio::select! {
        r = axum::serve(ui_listener, app).with_graceful_shutdown(shutdown()) => r?,
        r = axum::serve(ops_listener, ops).with_graceful_shutdown(shutdown()) => r?,
    }
    Ok(())
}

/// The token in `path`, if one is configured.
fn read_token(path: &Option<std::path::PathBuf>) -> anyhow::Result<Option<String>> {
    path.as_ref()
        .map(|path| {
            std::fs::read_to_string(path)
                .map(|t| t.trim().to_string())
                .with_context(|| format!("reading {}", path.display()))
        })
        .transpose()
}

/// Demo mode never reaches out: it has no fetchers, so scans report that the
/// forge is not configured.
fn build_scanner(cfg: &Config, store: Arc<Store>) -> anyhow::Result<Scanner> {
    let mut fetchers = Vec::new();
    if !cfg.demo {
        fetchers.push((
            Provider::Gitlab,
            ArchiveFetcher::gitlab(cfg.gitlab_url.clone(), read_token(&cfg.gitlab_token_file)?)
                .context("building the GitLab client")?,
        ));
        if let Some(url) = &cfg.gitea_url {
            fetchers.push((
                Provider::Gitea,
                ArchiveFetcher::gitea(url.clone(), read_token(&cfg.gitea_token_file)?)
                    .context("building the Gitea client")?,
            ));
        }
        fetchers.push((
            Provider::Github,
            ArchiveFetcher::github(
                cfg.github_api_url.clone(),
                read_token(&cfg.github_token_file)?,
            )
            .context("building the GitHub client")?,
        ));
    }
    let registry =
        RegistryClient::new(cfg.registry_allow.clone()).context("building the registry client")?;
    Ok(Scanner::new(store, fetchers, registry))
}

fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_env("DONARIUM_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn load_cookie_key(cfg: &Config) -> anyhow::Result<Key> {
    let Some(path) = &cfg.auth.cookie_key_file else {
        warn!("no --cookie-key-file: sessions will not survive a restart");
        return Ok(Key::generate());
    };
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let trimmed = String::from_utf8_lossy(&raw).trim().to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&trimmed)
        .ok()
        .filter(|b| b.len() >= 64)
        .unwrap_or(raw);
    Key::try_from(bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("cookie key must be at least 64 bytes (raw or base64)"))
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
