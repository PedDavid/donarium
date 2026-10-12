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
    let h = harness(false).await;
    let res = h
        .app
        .oneshot(
            Request::get("/projects/bigskysoftware-htmx")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/auth/login?next=%2Fprojects%2Fbigskysoftware-htmx"
    );
}

#[tokio::test]
async fn index_lists_projects_and_filters() {
    let h = harness(false).await;
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
    let h = harness(false).await;
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
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .clone()
        .oneshot(get("/projects/bigskysoftware-htmx", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body(res).await.contains("github.com/bigskysoftware/htmx"));

    let res = h.app.oneshot(get("/projects/nope", &cookie)).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_returns_matching_projects() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .oneshot(get("/search?q=serde", &cookie))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("/projects/serde-rs-serde"));
    assert!(!html.contains("/projects/bigskysoftware-htmx"));
}

#[tokio::test]
async fn responses_carry_security_headers() {
    let h = harness(false).await;
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
    let h = harness(false).await;
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
    let h = harness(true).await;
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
    let h = harness(false).await;
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

// --- sources ---

use donarium::model::{Provider, ScanMethod};

fn form_post(path: &str, cookie: &str, origin: Option<&str>, body: &str) -> Request<Body> {
    let mut req = Request::post(path)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(o) = origin {
        req = req.header(header::ORIGIN, o);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

fn add_form(csrf: &str, repo: &str, weight: &str) -> String {
    encode(&[
        ("_csrf", csrf),
        ("provider", "github"),
        ("repo", repo),
        ("weight", weight),
        ("scan_method", "auto"),
    ])
}

#[tokio::test]
async fn sources_page_lists_sources_for_everyone() {
    let h = harness(false).await;
    let reader = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .clone()
        .oneshot(get("/sources", &reader))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body(res).await;
    assert!(html.contains("PedDavid/infra"));
    assert!(html.contains("homelab/dashboards"));
    assert!(html.contains("disabled"));
    assert!(!html.contains("Add a source"), "readers get no add form");

    let admin = session_cookie(&h.key, &session(true));
    let res = h.app.oneshot(get("/sources", &admin)).await.unwrap();
    assert!(body(res).await.contains("Add a source"));
}

#[tokio::test]
async fn admins_add_a_source() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            "/sources",
            &cookie,
            Some(ORIGIN),
            &add_form(
                "csrf-token",
                "https://github.com/PedDavid/Donarium.git",
                "0.5",
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/sources?notice=added");

    let sources = h.store.list_sources().await.unwrap();
    let added = sources
        .iter()
        .find(|s| s.repo == "PedDavid/Donarium")
        .unwrap();
    assert_eq!(added.provider, Provider::Github);
    assert_eq!(added.weight, 0.5);
    assert_eq!(added.scan_method, ScanMethod::Auto);
    assert!(added.enabled);
    assert_eq!(added.created_by.sub, "u1");
    assert_eq!(added.created_by.name, "alice");

    let res = h
        .app
        .oneshot(get("/sources?notice=added", &cookie))
        .await
        .unwrap();
    assert!(body(res).await.contains("Source added."));
}

#[tokio::test]
async fn invalid_sources_are_rejected_and_the_form_is_kept() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(true));
    let before = h.store.list_sources().await.unwrap().len();
    for (repo, weight, message) in [
        ("not-a-repo", "1", "owner/name"),
        ("https://evil.example/a/b", "1", "must be on github.com"),
        ("a/b", "0", "greater than 0"),
        ("a/b", "lots", "must be a number"),
    ] {
        let res = h
            .app
            .clone()
            .oneshot(form_post(
                "/sources",
                &cookie,
                Some(ORIGIN),
                &add_form("csrf-token", repo, weight),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{repo} {weight}"
        );
        let html = body(res).await;
        assert!(html.contains(message), "{repo} {weight}: {html}");
    }
    assert_eq!(h.store.list_sources().await.unwrap().len(), before);
}

#[tokio::test]
async fn adding_an_existing_source_is_a_conflict() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .oneshot(form_post(
            "/sources",
            &cookie,
            Some(ORIGIN),
            &add_form("csrf-token", "peddavid/INFRA", "1"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(body(res).await.contains("already a source"));
}

#[tokio::test]
async fn source_changes_need_admin_csrf_and_origin() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let reader = session_cookie(&h.key, &session(false));
    let before = h.store.list_sources().await.unwrap();
    let id = before[0].id;
    let edit = encode(&[
        ("_csrf", "csrf-token"),
        ("weight", "9"),
        ("scan_method", "sbom"),
    ]);

    // Not an admin.
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}"),
            &reader,
            Some(ORIGIN),
            &edit,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}/delete"),
            &reader,
            Some(ORIGIN),
            "_csrf=csrf-token",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Wrong CSRF token.
    let bad_csrf = encode(&[("_csrf", "nope"), ("weight", "9"), ("scan_method", "sbom")]);
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}"),
            &admin,
            Some(ORIGIN),
            &bad_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Cross-origin.
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}"),
            &admin,
            Some("https://evil.example"),
            &edit,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = h
        .app
        .oneshot(form_post(
            "/sources",
            &admin,
            None,
            &add_form("csrf-token", "a/b", "1"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    assert_eq!(
        h.store.list_sources().await.unwrap(),
        before,
        "nothing changed"
    );
}

#[tokio::test]
async fn admins_edit_and_delete_a_source() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(true));
    let id = h.store.list_sources().await.unwrap()[0].id;

    // Unticked checkbox: the source is disabled.
    let edit = encode(&[
        ("_csrf", "csrf-token"),
        ("weight", "2.5"),
        ("scan_method", "lockfiles"),
    ]);
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}"),
            &cookie,
            Some(ORIGIN),
            &edit,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let s = h.store.get_source(id).await.unwrap().unwrap();
    assert_eq!(
        (s.weight, s.scan_method, s.enabled),
        (2.5, ScanMethod::Lockfiles, false)
    );
    assert_eq!(s.updated_by.name, "alice");

    // Ticked: enabled again. A bad weight is rejected and changes nothing.
    let edit = encode(&[
        ("_csrf", "csrf-token"),
        ("weight", "-1"),
        ("scan_method", "auto"),
        ("enabled", "on"),
    ]);
    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}"),
            &cookie,
            Some(ORIGIN),
            &edit,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(h.store.get_source(id).await.unwrap().unwrap().weight, 2.5);

    let res = h
        .app
        .clone()
        .oneshot(get(&format!("/sources/{id}?notice=saved"), &cookie))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("Changes saved."));
    assert!(html.contains("Delete this source"));

    let res = h
        .app
        .clone()
        .oneshot(form_post(
            &format!("/sources/{id}/delete"),
            &cookie,
            Some(ORIGIN),
            "_csrf=csrf-token",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/sources?notice=deleted");
    assert!(h.store.get_source(id).await.unwrap().is_none());
    let res = h
        .app
        .oneshot(get(&format!("/sources/{id}"), &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notices_never_echo_the_query_string() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .oneshot(get(
            "/sources?notice=%3Cscript%3Ealert(1)%3C/script%3E",
            &cookie,
        ))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(!html.contains("alert(1)"));
}

#[tokio::test]
async fn source_pages_escape_what_they_show() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(true));
    // Rejected input is shown back in the form: it must be escaped.
    let res = h
        .app
        .oneshot(form_post(
            "/sources",
            &cookie,
            Some(ORIGIN),
            &add_form("csrf-token", "\"><script>alert(1)</script>", "1"),
        ))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(!html.contains("<script>alert(1)"), "{html}");
}
