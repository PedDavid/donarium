//! HTTP-level tests of the UI router: auth, Origin checks, pages and the ops port.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use common::*;

fn get(path: &str, cookie: &str) -> Request<Body> {
    Request::get(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn unauthenticated_users_are_sent_to_login() {
    let h = harness(false);
    let res = h
        .app
        .oneshot(Request::get("/projects/htmx").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/auth/login?next=%2Fprojects%2Fhtmx"
    );
}

#[tokio::test]
async fn index_lists_projects_and_filters() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));

    let res = h.app.clone().oneshot(get("/", &cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body(res).await;
    assert!(html.contains("tokio-rs-tokio"), "{html}");
    assert!(html.contains("prometheus-prometheus"));

    let res = h
        .app
        .clone()
        .oneshot(get("/?q=axum", &cookie))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("tokio-rs-axum"));
    assert!(!html.contains("serde-rs-serde"));

    let res = h
        .app
        .oneshot(get("/?q=nothing-like-this", &cookie))
        .await
        .unwrap();
    assert!(body(res).await.contains("No matching projects"));
}

#[tokio::test]
async fn htmx_filter_returns_only_the_table() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .oneshot(
            Request::get("/?q=htmx")
                .header(header::COOKIE, &cookie)
                .header("hx-request", "true")
                .header("hx-target", "projects-table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("bigskysoftware/htmx"));
    assert!(
        !html.contains("<html"),
        "partial must not include the layout"
    );
}

#[tokio::test]
async fn detail_and_not_found() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .clone()
        .oneshot(get("/projects/htmx", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body(res).await.contains("github.com/bigskysoftware/htmx"));

    let res = h.app.oneshot(get("/projects/nope", &cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_returns_matching_projects() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .oneshot(get("/search?q=serde", &cookie))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("/projects/serde-rs-serde"));
    assert!(!html.contains("/projects/htmx"));
}

#[tokio::test]
async fn responses_carry_security_headers() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));
    let res = h.app.oneshot(get("/", &cookie)).await.unwrap();
    let headers = res.headers();
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
}

#[tokio::test]
async fn posts_need_a_same_origin_header() {
    let h = harness(false);
    let cookie = session_cookie(&h.key, &session(false));
    let post = |origin: Option<&str>| {
        let mut req = Request::post("/auth/logout")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(o) = origin {
            req = req.header(header::ORIGIN, o);
        }
        req.body(Body::from("_csrf=csrf-token")).unwrap()
    };
    let res = h.app.clone().oneshot(post(None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = h
        .app
        .clone()
        .oneshot(post(Some("https://evil.example")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = h.app.oneshot(post(Some(ORIGIN))).await.unwrap();
    assert_ne!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn assets_are_served_with_long_cache() {
    let h = harness(true);
    let res = h
        .app
        .oneshot(
            Request::get("/assets/js/app.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
}

#[tokio::test]
async fn ops_port_serves_health_and_metrics() {
    let h = harness(false);
    for (path, status) in [("/healthz", StatusCode::OK), ("/readyz", StatusCode::OK)] {
        let res = h
            .ops
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), status, "{path}");
    }
    let res = h
        .ops
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = body(res).await;
    assert!(text.contains("donarium_project_usage_score"), "{text}");
}
