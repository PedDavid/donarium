//! Login against a mock OIDC provider that signs real ID tokens: how the
//! callback turns `auth_time` into step-up freshness.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum_extra::extract::PrivateCookieJar;
use axum_extra::extract::cookie::Key;
use chrono::{Duration, Utc};
use clap::Parser;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, IssuerUrl, JsonWebKeyId, Nonce, PrivateSigningKey,
    StandardClaims, SubjectIdentifier,
};
use rsa::RsaPrivateKey;
use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use donarium::config::Config;
use donarium::scan::registry::RegistryClient;
use donarium::scan::run::Scanner;
use donarium::store::Store;
use donarium::web::auth::{AuthMode, SESSION_COOKIE, Session};
use donarium::web::{AppState, router};

const CLIENT_ID: &str = "donarium";

struct Idp {
    server: MockServer,
    key: CoreRsaPrivateSigningKey,
}

impl Idp {
    async fn start() -> Idp {
        let server = MockServer::start().await;
        let pem = RsaPrivateKey::new(&mut rand08::rngs::OsRng, 2048)
            .unwrap()
            .to_pkcs1_pem(LineEnding::LF)
            .unwrap();
        let key =
            CoreRsaPrivateSigningKey::from_pem(&pem, Some(JsonWebKeyId::new("k1".into()))).unwrap();
        let issuer = server.uri();
        Mock::given(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
            })))
            .mount(&server)
            .await;
        Mock::given(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(CoreJsonWebKeySet::new(vec![key.as_verification_key()])),
            )
            .mount(&server)
            .await;
        Idp { server, key }
    }

    /// Answers the next code exchange with an ID token for `nonce`.
    async fn issue(&self, nonce: &str, auth_time: Option<chrono::DateTime<Utc>>) {
        let now = Utc::now();
        let claims = CoreIdTokenClaims::new(
            IssuerUrl::new(self.server.uri()).unwrap(),
            vec![Audience::new(CLIENT_ID.into())],
            now + Duration::minutes(5),
            now,
            StandardClaims::new(SubjectIdentifier::new("user-1".into())),
            EmptyAdditionalClaims {},
        )
        .set_nonce(Some(Nonce::new(nonce.into())))
        .set_auth_time(auth_time);
        let id_token = CoreIdToken::new(
            claims,
            &self.key,
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            None,
            None,
        )
        .unwrap();
        Mock::given(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at",
                "token_type": "Bearer",
                "expires_in": 300,
                "id_token": id_token.to_string(),
            })))
            .mount(&self.server)
            .await;
    }
}

async fn app(idp: &Idp, key: &Key) -> Router {
    let cfg = Config::try_parse_from([
        "donarium",
        "--oidc-issuer",
        &idp.server.uri(),
        "--oidc-client-id",
        CLIENT_ID,
        "--step-up-max-age",
        "15m",
    ])
    .unwrap();
    let store = Arc::new(Store::memory().await.unwrap());
    let auth = AuthMode::from_config(&cfg.auth, &cfg.public_url, false).unwrap();
    let scanner = Arc::new(Scanner::new(
        store.clone(),
        None,
        RegistryClient::new(vec![]).unwrap(),
    ));
    router(AppState::new(cfg, store, scanner, auth, key.clone()))
}

fn set_cookie(res: &axum::response::Response, name: &str) -> String {
    res.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .find(|c| c.starts_with(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} cookie"))
}

/// Runs `/auth/login` then `/auth/callback`, with the provider reporting
/// `auth_time`. Returns the callback response.
async fn login(
    idp: &Idp,
    key: &Key,
    reauth: bool,
    auth_time: Option<chrono::DateTime<Utc>>,
) -> axum::response::Response {
    let app = app(idp, key).await;
    let uri = if reauth {
        "/auth/login?reauth=true&next=/projects/x"
    } else {
        "/auth/login?next=/projects/x"
    };
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let authorize = url::Url::parse(res.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let param = |name: &str| {
        authorize
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    assert_eq!(param("prompt").as_deref(), reauth.then_some("login"));
    idp.issue(&param("nonce").unwrap(), auth_time).await;
    let login_cookie = set_cookie(&res, "donarium_login");
    app.oneshot(
        Request::get(format!(
            "/auth/callback?code=c&state={}",
            param("state").unwrap()
        ))
        .header(header::COOKIE, login_cookie)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap()
}

fn session(res: &axum::response::Response, key: &Key) -> Session {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        set_cookie(res, SESSION_COOKIE).parse().unwrap(),
    );
    let jar = PrivateCookieJar::from_headers(&headers, key.clone());
    serde_json::from_str(jar.get(SESSION_COOKIE).unwrap().value()).unwrap()
}

fn step_up() -> Option<jiff::SignedDuration> {
    Some(jiff::SignedDuration::from_mins(15))
}

#[tokio::test]
async fn login_without_auth_time_is_not_fresh() {
    let (idp, key) = (Idp::start().await, Key::generate());
    let res = login(&idp, &key, false, None).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/projects/x");
    let s = session(&res, &key);
    assert_eq!(s.sub, "user-1");
    assert_eq!(s.auth_time, None);
    // Logged in, but step-up needs a provable recent login.
    assert!(!s.fresh_enough(step_up(), jiff::Timestamp::now()));
}

#[tokio::test]
async fn sso_login_keeps_the_old_auth_time() {
    let (idp, key) = (Idp::start().await, Key::generate());
    let old = Utc::now() - Duration::hours(3);
    let s = session(&login(&idp, &key, false, Some(old)).await, &key);
    assert_eq!(s.auth_time, Some(old.timestamp()));
    assert!(!s.fresh_enough(step_up(), jiff::Timestamp::now()));
}

#[tokio::test]
async fn reauth_without_auth_time_is_refused() {
    let (idp, key) = (Idp::start().await, Key::generate());
    let res = login(&idp, &key, true, None).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !res.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap().starts_with(SESSION_COOKIE))
    );
}

#[tokio::test]
async fn reauth_with_stale_auth_time_is_refused() {
    let (idp, key) = (Idp::start().await, Key::generate());
    let res = login(&idp, &key, true, Some(Utc::now() - Duration::hours(3))).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reauth_with_fresh_auth_time_is_fresh() {
    let (idp, key) = (Idp::start().await, Key::generate());
    let now = Utc::now();
    let res = login(&idp, &key, true, Some(now)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = session(&res, &key);
    assert_eq!(s.auth_time, Some(now.timestamp()));
    assert!(s.fresh_enough(step_up(), jiff::Timestamp::now()));
}
