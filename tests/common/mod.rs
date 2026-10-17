//! Shared test harness for the HTTP tests.
#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum_extra::extract::PrivateCookieJar;
use axum_extra::extract::cookie::{Cookie, Key};
use clap::Parser;
use http_body_util::BodyExt;

use donarium::config::Config;
use donarium::demo;
use donarium::metrics::Metrics;
use donarium::model::Provider;
use donarium::scan::fetch::ArchiveFetcher;
use donarium::scan::registry::RegistryClient;
use donarium::scan::run::Scanner;
use donarium::store::Store;
use donarium::web::auth::{AuthMode, SESSION_COOKIE, Session};
use donarium::web::{AppState, ops_router, router};

pub const ORIGIN: &str = "http://localhost:8080";

/// Every relative date in the pages is computed against this.
pub const NOW: &str = "2026-09-26T12:00:00Z";

pub struct Harness {
    pub app: Router,
    pub ops: Router,
    pub store: Arc<Store>,
    pub scanner: Arc<Scanner>,
    pub key: Key,
}

/// The app with the demo projects. With `dev_auth` every visitor is logged in
/// as an admin; without it, OIDC is configured and visitors must log in.
pub async fn harness(dev_auth: bool) -> Harness {
    harness_with(dev_auth, None, &[]).await
}

/// [`harness`] with extra command-line flags, e.g. `--gitea-url`.
pub async fn harness_args(dev_auth: bool, extra: &[&str]) -> Harness {
    harness_with(dev_auth, None, extra).await
}

/// Like [`harness`] with dev auth, but scans read GitLab from `gitlab` and
/// image labels from `registry` (base URLs of mock servers).
pub async fn harness_scanning(gitlab: &str, registry: &str) -> Harness {
    harness_with(true, Some((gitlab.to_string(), registry.to_string())), &[]).await
}

async fn harness_with(dev_auth: bool, mocks: Option<(String, String)>, extra: &[&str]) -> Harness {
    let mut args = vec!["donarium"];
    args.extend_from_slice(extra);
    if dev_auth {
        args.push("--insecure-dev-auth");
    } else {
        args.extend([
            "--oidc-issuer",
            "https://idp.example.com",
            "--oidc-client-id",
            "donarium",
        ]);
    }
    let cfg = Config::try_parse_from(args).unwrap();
    let now: jiff::Timestamp = NOW.parse().unwrap();
    let store = Arc::new(Store::memory().await.unwrap());
    demo::seed(&store, now).await.unwrap();
    let metrics = Metrics::new();
    let auth = AuthMode::from_config(&cfg.auth, &cfg.public_url, false).unwrap();
    let key = Key::generate();
    // Without mocks there is no GitLab fetcher: scans report that they are
    // not available.
    let scanner = Arc::new(match mocks {
        Some((gitlab, registry)) => Scanner::new(
            store.clone(),
            vec![(
                Provider::Gitlab,
                ArchiveFetcher::gitlab(gitlab.parse().unwrap(), None).unwrap(),
            )],
            RegistryClient::for_tests(&registry),
        ),
        None => Scanner::new(store.clone(), vec![], RegistryClient::new(vec![]).unwrap()),
    });
    let state =
        AppState::new(cfg, store.clone(), scanner.clone(), auth, key.clone()).with_fixed_now(now);
    Harness {
        app: router(state),
        ops: ops_router(store.clone(), metrics, "EUR".to_string()),
        store,
        scanner,
        key,
    }
}

pub fn session(admin: bool) -> Session {
    let now = jiff::Timestamp::now().as_second();
    Session {
        sub: "u1".into(),
        name: if admin { "alice" } else { "bob" }.into(),
        admin,
        csrf: "csrf-token".into(),
        auth_time: Some(now),
        exp: now + 3600,
    }
}

/// Encrypts a session exactly as the login callback does.
pub fn session_cookie(key: &Key, session: &Session) -> String {
    let jar = PrivateCookieJar::new(key.clone()).add(Cookie::new(
        SESSION_COOKIE,
        serde_json::to_string(session).unwrap(),
    ));
    let res = (jar, ()).into_response();
    let set_cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

pub async fn body(res: axum::response::Response) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

/// A `.tar.gz` like GitLab's archive endpoint serves: files under one folder.
pub fn archive(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("repo-main-abc/{name}"),
                data.as_bytes(),
            )
            .unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}
