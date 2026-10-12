use std::sync::Arc;

use anyhow::Context;
use axum_extra::extract::cookie::Key;
use base64::Engine as _;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use donarium::config::Config;
use donarium::demo;
use donarium::metrics::Metrics;
use donarium::repo::{MemoryRepository, Repository};
use donarium::web::auth::AuthMode;
use donarium::web::{self, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    init_tracing(cfg.log_json);

    let repo: Arc<dyn Repository> = if cfg.demo {
        warn!("demo mode: serving sample data from memory");
        Arc::new(MemoryRepository::new(demo::sample_projects(
            jiff::Timestamp::now(),
        )))
    } else {
        // Storage (Project/Source resources and the database) arrives with the
        // next milestone.
        warn!("no storage configured yet: serving an empty project list");
        Arc::new(MemoryRepository::default())
    };
    let metrics = Metrics::new(repo.clone());
    let demo_dev_auth = cfg.demo && cfg.auth.oidc_issuer.is_none();
    let auth = AuthMode::from_config(&cfg.auth, &cfg.public_url, demo_dev_auth)?;
    let cookie_key = load_cookie_key(&cfg)?;

    let ops = web::ops_router(repo.clone(), metrics.clone());
    let state = AppState::new(cfg.clone(), repo, metrics, auth, cookie_key);
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
