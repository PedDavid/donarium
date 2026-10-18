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
async fn the_list_is_ranked_and_the_detail_explains_it() {
    let h = harness(false).await;
    let cookie = session_cookie(&h.key, &session(false));
    let res = h.app.clone().oneshot(get("/", &cookie)).await.unwrap();
    let html = body(res).await;
    assert!(html.contains("Highest priority first"));
    assert!(html.contains("no way to donate yet"));

    let res = h
        .app
        .clone()
        .oneshot(get("/?sort=name", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = h
        .app
        .clone()
        .oneshot(get("/projects/bigskysoftware-htmx", &cookie))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("Ranking"));
    assert!(html.contains("fair share"));
    assert!(html.contains("<progress"));
    assert!(html.contains("Used by"));

    let res = h
        .ops
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(body(res).await.contains("donarium_project_priority{"));
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

// --- image scans ---

/// The demo's GitLab source (the third one added) has an image scan: Jellyfin
/// and the linuxserver images with labels, paperless guessed from its name, and
/// gluetun without any project.
const GITLAB_SOURCE: i64 = 3;

fn post(path: &str, cookie: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    form_post(path, cookie, Some(ORIGIN), &encode(pairs))
}

#[tokio::test]
async fn the_source_page_lists_images_and_what_they_resolve_to() {
    let h = harness(false).await;
    let reader = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .oneshot(get(&format!("/sources/{GITLAB_SOURCE}"), &reader))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body(res).await;
    assert!(html.contains("docker.io/jellyfin/jellyfin"));
    assert!(html.contains("github.com/jellyfin/jellyfin"));
    assert!(
        html.contains("from the image&#39;s source label")
            || html.contains("from the image's source label"),
        "{html}"
    );
    assert!(html.contains("github.com/paperless-ngx/paperless-ngx"));
    assert!(html.contains("guessed from the image name"));
    assert!(html.contains("no project"), "gluetun has none");
    assert!(html.contains("image not found (it may be private)"));
    assert!(html.contains("image reference(s) skipped"));
    assert!(html.contains("PROXY_IMAGE"));
    // Readers see everything but cannot change anything.
    assert!(!html.contains("/image-mappings"));
    assert!(!html.contains("Scan now"));
}

#[tokio::test]
async fn admins_map_an_unresolved_image_and_remove_the_mapping() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let names = |h: &Harness| {
        let store = h.store.clone();
        async move {
            store
                .list_projects()
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        }
    };
    assert!(!names(&h).await.contains(&"qdm12-gluetun".to_string()));

    let source_id = GITLAB_SOURCE.to_string();
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/image-mappings",
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("source_id", &source_id),
                ("registry", "docker.io"),
                ("repository", "qmcgaw/gluetun"),
                ("project_repo", "https://github.com/QDM12/gluetun"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/sources/3?notice=mapped");
    assert!(names(&h).await.contains(&"qdm12-gluetun".to_string()));

    let res = h
        .app
        .clone()
        .oneshot(get("/sources/3?notice=mapped", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("Image mapped."));
    assert!(html.contains("mapped by an admin"));
    assert!(html.contains("Remove mapping"));

    let res = h
        .app
        .clone()
        .oneshot(post(
            "/image-mappings/delete",
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("source_id", &source_id),
                ("registry", "docker.io"),
                ("repository", "qmcgaw/gluetun"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(!names(&h).await.contains(&"qdm12-gluetun".to_string()));
}

#[tokio::test]
async fn a_bad_project_is_rejected_with_a_message() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .oneshot(post(
            "/image-mappings",
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("source_id", "3"),
                ("registry", "docker.io"),
                ("repository", "qmcgaw/gluetun"),
                // Without a host it is ambiguous.
                ("project_repo", "qdm12/gluetun"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(res).await.contains("host/owner/name"));
    assert!(h.store.list_mappings().await.unwrap().is_empty());
}

#[tokio::test]
async fn image_changes_need_admin_csrf_and_origin() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let reader = session_cookie(&h.key, &session(false));
    let map = |csrf: &str| {
        [
            ("_csrf", csrf.to_string()),
            ("source_id", "3".to_string()),
            ("registry", "docker.io".to_string()),
            ("repository", "qmcgaw/gluetun".to_string()),
            ("project_repo", "github.com/qdm12/gluetun".to_string()),
        ]
    };
    let send = |cookie: &str, origin: Option<&str>, csrf: &str, path: &str| {
        let pairs = map(csrf);
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        form_post(path, cookie, origin, &encode(&refs))
    };
    for path in [
        "/image-mappings",
        "/image-mappings/delete",
        "/sources/3/scan",
    ] {
        let r = |req| h.app.clone().oneshot(req);
        assert_eq!(
            r(send(&reader, Some(ORIGIN), "csrf-token", path))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "reader {path}"
        );
        assert_eq!(
            r(send(&admin, Some(ORIGIN), "wrong", path))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "csrf {path}"
        );
        assert_eq!(
            r(send(&admin, None, "csrf-token", path))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "origin {path}"
        );
    }
    assert!(h.store.list_mappings().await.unwrap().is_empty());
}

#[tokio::test]
async fn scans_say_when_they_are_not_available() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(get("/sources/3", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("GitLab is not configured"), "{html}");
    assert!(!html.contains("Scan now"));
    let res = h
        .app
        .oneshot(post("/sources/3/scan", &admin, &[("_csrf", "csrf-token")]))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(body(res).await.contains("GitLab is not configured"));
}

#[tokio::test]
async fn scan_now_runs_in_the_background_and_shows_the_result() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let (gitlab, registry) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("GET"))
        .and(path(
            "/api/v4/projects/homelab%2Fdashboards/repository/archive.tar.gz",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[(
            "stack/compose.yaml",
            "services:\n  grafana:\n    image: grafana/grafana:11\n",
        )])))
        .mount(&gitlab)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/grafana/grafana/manifests/11"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"config": {"digest": "sha256:c"}})),
        )
        .mount(&registry)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/grafana/grafana/blobs/sha256:c"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"config": {"Labels": {"org.opencontainers.image.source": "https://github.com/grafana/grafana"}}})))
        .mount(&registry)
        .await;

    let h = harness_scanning(&gitlab.uri(), &registry.uri()).await;
    let res = h
        .app
        .clone()
        .oneshot(get("/sources/3", "x=y"))
        .await
        .unwrap();
    assert!(body(res).await.contains("Scan now"));

    let res = h
        .app
        .clone()
        .oneshot(
            Request::post("/sources/3/scan")
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("_csrf={}", dev_csrf(&h).await)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/sources/3?notice=scanning"
    );

    // The scan runs on its own; wait for it to finish.
    for _ in 0..200 {
        if !h.scanner.is_running(3)
            && h.store
                .get_source(3)
                .await
                .unwrap()
                .unwrap()
                .last_scan_status
                .as_deref()
                != Some("5 images, 4 projects, 1 unresolved, 1 skipped")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let source = h.store.get_source(3).await.unwrap().unwrap();
    assert_eq!(
        source.last_scan_ok,
        Some(true),
        "{:?}",
        source.last_scan_status
    );
    assert_eq!(
        source.last_scan_status.as_deref(),
        Some("1 images, 1 projects")
    );
    let names: Vec<String> = h
        .store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(names.contains(&"grafana-grafana".to_string()), "{names:?}");
    assert!(
        !names.contains(&"jellyfin-jellyfin".to_string()),
        "the demo's old findings were replaced"
    );

    let res = h.app.oneshot(get("/sources/3", "x=y")).await.unwrap();
    let html = body(res).await;
    assert!(html.contains("docker.io/grafana/grafana"));
    assert!(html.contains("1 images, 1 projects"));
}

/// With dev auth every visitor is an admin; its CSRF token is on any page.
async fn dev_csrf(h: &Harness) -> String {
    let res = h
        .app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body(res).await;
    let marker = "X-CSRF-Token\": \"";
    let start = html.find(marker).expect("the page carries a CSRF token") + marker.len();
    html[start..start + html[start..].find('"').unwrap()].to_string()
}

#[tokio::test]
async fn deleting_a_source_removes_the_projects_only_it_used() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let before: Vec<String> = h
        .store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(before.contains(&"jellyfin-jellyfin".to_string()));
    let res = h
        .app
        .oneshot(form_post(
            "/sources/3/delete",
            &admin,
            Some(ORIGIN),
            "_csrf=csrf-token",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let after: Vec<String> = h
        .store
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(!after.contains(&"jellyfin-jellyfin".to_string()));
    // Projects that other sources use stay.
    assert!(after.contains(&"tokio-rs-tokio".to_string()));
}

// --- Gitea and Forgejo sources ---

#[tokio::test]
async fn gitea_is_offered_only_when_a_server_is_configured() {
    let h = harness(true).await;
    let csrf = dev_csrf(&h).await;
    let res = h
        .app
        .clone()
        .oneshot(Request::get("/sources").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("<option value=\"gitlab\""));
    assert!(!html.contains("<option value=\"gitea\""), "{html}");

    let res = h
        .app
        .oneshot(
            Request::post("/sources")
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(encode(&[
                    ("_csrf", &csrf),
                    ("provider", "gitea"),
                    ("repo", "me/flux"),
                    ("weight", "1"),
                    ("scan_method", "images"),
                ])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(res).await.contains("--gitea-url"));
}

#[tokio::test]
async fn gitea_sources_use_the_configured_host() {
    let h = harness_args(true, &["--gitea-url", "https://git.example.com:3000"]).await;
    let csrf = dev_csrf(&h).await;
    let add = |repo: &str| {
        Request::post("/sources")
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(encode(&[
                ("_csrf", csrf.as_str()),
                ("provider", "gitea"),
                ("repo", repo),
                ("weight", "1"),
                ("scan_method", "images"),
            ])))
            .unwrap()
    };

    let res = h
        .app
        .clone()
        .oneshot(Request::get("/sources").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(body(res).await.contains("<option value=\"gitea\""));

    // Another host, or the same host on another port, is refused.
    for bad in [
        "https://evil.example/me/flux",
        "https://git.example.com/me/flux",
    ] {
        let res = h.app.clone().oneshot(add(bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert!(
            body(res).await.contains("must be on git.example.com:3000"),
            "{bad}"
        );
    }

    let res = h
        .app
        .clone()
        .oneshot(add("https://git.example.com:3000/me/flux.git"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let source = h
        .store
        .list_sources()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.provider == Provider::Gitea)
        .unwrap();
    assert_eq!(source.repo, "me/flux");

    let res = h
        .app
        .oneshot(
            Request::get(format!("/sources/{}", source.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body(res).await;
    assert!(
        html.contains("https://git.example.com:3000/me/flux"),
        "{html}"
    );
    // The scan section names the missing fetcher: the harness reads no server.
    assert!(html.contains("no Gitea or Forgejo server is configured"));
}

// --- projects added by hand ---

fn add_project_form(csrf: &str, repo: &str, usage: &str, notes: &str) -> Vec<(String, String)> {
    [
        ("_csrf", csrf),
        ("repo", repo),
        ("usage", usage),
        ("notes", notes),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn pairs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(k, x)| (k.as_str(), x.as_str())).collect()
}

#[tokio::test]
async fn admins_add_a_project_by_hand() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let form = add_project_form(
        "csrf-token",
        "https://github.com/Immich-App/immich",
        "3",
        "runs my photos",
    );
    let res = h
        .app
        .clone()
        .oneshot(post("/projects", &admin, &pairs(&form)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/projects/immich-app-immich?notice=created"
    );
    let project = h
        .store
        .get_project("immich-app-immich")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(project.repo, "github.com/immich-app/immich");
    assert_eq!(project.usage_score, 3.0);

    let res = h
        .app
        .clone()
        .oneshot(get("/projects/immich-app-immich?notice=created", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("Project added."));
    assert!(html.contains("added by hand"));
    assert!(html.contains("runs my photos"));
    assert!(html.contains("Delete this project"));

    // It is in the list, marked as added by hand.
    let res = h.app.oneshot(get("/", &admin)).await.unwrap();
    let html = body(res).await;
    assert!(html.contains("immich-app-immich"));
    assert!(html.contains("added by hand"));
}

#[tokio::test]
async fn bad_projects_are_rejected_with_a_message() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let before = h.store.list_projects().await.unwrap().len();
    for (repo, usage, notes, message) in [
        ("tokio-rs/tokio", "1", "", "host/owner/name"),
        ("", "1", "", "host/owner/name"),
        ("github.com/a/b", "lots", "", "must be a number"),
        ("github.com/a/b", "-1", "", "between 0 and 1000"),
        ("github.com/a/b", "1", &"x".repeat(1001), "too long"),
    ] {
        let form = add_project_form("csrf-token", repo, usage, notes);
        let res = h
            .app
            .clone()
            .oneshot(post("/projects", &admin, &pairs(&form)))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{repo} {usage}"
        );
        assert!(body(res).await.contains(message), "{repo} {usage}");
    }
    // An existing repository is a conflict, whichever way it is written.
    let form = add_project_form("csrf-token", "GITHUB.com/Tokio-rs/tokio", "1", "");
    let res = h
        .app
        .oneshot(post("/projects", &admin, &pairs(&form)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(h.store.list_projects().await.unwrap().len(), before);
}

#[tokio::test]
async fn project_changes_need_admin_csrf_and_origin() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let reader = session_cookie(&h.key, &session(false));
    let name = "bigskysoftware-htmx";
    let before = h.store.get_project(name).await.unwrap().unwrap();
    let requests: Vec<(String, Vec<(&str, &str)>)> = vec![
        (
            "/projects".into(),
            vec![("repo", "github.com/a/b"), ("usage", "1")],
        ),
        (
            format!("/projects/{name}"),
            vec![("usage", "9"), ("ignore", "on")],
        ),
        (
            format!("/projects/{name}/funding"),
            vec![("platform", "kofi"), ("handle", "x")],
        ),
        (
            format!("/projects/{name}/funding/delete"),
            vec![("platform", "kofi"), ("handle", "x")],
        ),
        (format!("/projects/{name}/delete"), vec![]),
    ];
    for (path, fields) in &requests {
        let with = |csrf: &'static str| {
            let mut all = vec![("_csrf", csrf)];
            all.extend(fields.iter().copied());
            encode(&all)
        };
        let send = |cookie: &str, origin: Option<&str>, csrf: &'static str| {
            form_post(path, cookie, origin, &with(csrf))
        };
        let r = |req| h.app.clone().oneshot(req);
        assert_eq!(
            r(send(&reader, Some(ORIGIN), "csrf-token"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "reader {path}"
        );
        assert_eq!(
            r(send(&admin, Some(ORIGIN), "wrong"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "csrf {path}"
        );
        assert_eq!(
            r(send(&admin, None, "csrf-token")).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "origin {path}"
        );
    }
    assert_eq!(h.store.get_project(name).await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn admins_change_settings_and_ignore_a_project() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let name = "bigskysoftware-htmx";
    let res = h
        .app
        .clone()
        .oneshot(post(
            &format!("/projects/{name}"),
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("usage", "2.5"),
                ("ignore", "on"),
                ("notes", "company-backed"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let project = h.store.get_project(name).await.unwrap().unwrap();
    assert!(project.ignored);
    assert_eq!(project.manual_usage, 2.5);
    assert_eq!(project.notes, "company-backed");

    // An ignored project leaves the list and shows up under "ignored".
    let res = h.app.clone().oneshot(get("/", &admin)).await.unwrap();
    let html = body(res).await;
    assert!(html.contains("1 ignored project(s)"), "{html}");
    let table = html.split("1 ignored project(s)").next().unwrap();
    assert!(
        !table.contains(&format!("/projects/{name}\"")),
        "not in the main list"
    );
    assert!(
        html.contains(&format!("/projects/{name}")),
        "but still reachable"
    );

    // A bad value changes nothing.
    let res = h
        .app
        .oneshot(post(
            &format!("/projects/{name}"),
            &admin,
            &[("_csrf", "csrf-token"), ("usage", "-5"), ("notes", "")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        h.store
            .get_project(name)
            .await
            .unwrap()
            .unwrap()
            .manual_usage,
        2.5
    );
}

#[tokio::test]
async fn admins_add_and_remove_ways_to_donate() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let name = "prometheus-prometheus"; // the demo gives it none
    let add = |platform: &str, handle: &str| {
        post(
            &format!("/projects/{name}/funding"),
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("platform", platform),
                ("handle", handle),
            ],
        )
    };
    let res = h
        .app
        .clone()
        .oneshot(add("opencollective", "prometheus"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = h
        .app
        .clone()
        .oneshot(add("other", "https://prometheus.io/donate"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let res = h
        .app
        .clone()
        .oneshot(get(&format!("/projects/{name}"), &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(
        html.contains("https://opencollective.com/prometheus"),
        "{html}"
    );
    assert!(html.contains("https://prometheus.io/donate"));

    // Duplicates, links that are not https, and links in a handle are refused.
    assert_eq!(
        h.app
            .clone()
            .oneshot(add("opencollective", "prometheus"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    for (platform, handle) in [
        ("other", "javascript:alert(1)"),
        ("other", "http://prometheus.io"),
        ("kofi", "https://ko-fi.com/x"),
        ("paypal", "x"),
        ("kofi", ""),
    ] {
        let res = h.app.clone().oneshot(add(platform, handle)).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{platform} {handle}"
        );
    }

    let res = h
        .app
        .clone()
        .oneshot(post(
            &format!("/projects/{name}/funding/delete"),
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("platform", "opencollective"),
                ("handle", "prometheus"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let funding = h.store.get_project(name).await.unwrap().unwrap().funding;
    assert_eq!(funding.len(), 1);
    assert_eq!(funding[0].platform, "other");
}

#[tokio::test]
async fn only_manual_projects_can_be_deleted() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    // A discovered project: refused, with the reason.
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/projects/bigskysoftware-htmx/delete",
            &admin,
            &[("_csrf", "csrf-token")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(body(res).await.contains("found by a scan"));
    assert!(
        h.store
            .get_project("bigskysoftware-htmx")
            .await
            .unwrap()
            .is_some()
    );

    // One added by hand: gone.
    h.store
        .create_project(
            "github.com/a/b",
            1.0,
            "",
            &donarium::model::Actor::new("u", "u"),
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    let res = h
        .app
        .oneshot(post(
            "/projects/a-b/delete",
            &admin,
            &[("_csrf", "csrf-token")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[header::LOCATION], "/?notice=deleted");
    assert!(h.store.get_project("a-b").await.unwrap().is_none());
}

#[tokio::test]
async fn readers_see_projects_but_no_forms() {
    let h = harness(false).await;
    let reader = session_cookie(&h.key, &session(false));
    let res = h.app.clone().oneshot(get("/", &reader)).await.unwrap();
    assert!(!body(res).await.contains("Add a project"));
    let res = h
        .app
        .oneshot(get("/projects/bigskysoftware-htmx", &reader))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(
        html.contains("https://github.com/sponsors/bigskysoftware-htmx"),
        "{html}"
    );
    assert!(!html.contains("Add a way to donate"));
    assert!(!html.contains("Delete this project"));
}

// --- donations ---

fn donation_form<'a>(csrf: &'a str, fields: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut all = vec![
        ("_csrf", csrf),
        ("project", "tokio-rs-axum"),
        ("platform", "github-sponsors"),
        ("amount", "12.50"),
        ("currency", "EUR"),
        ("date", "2026-09-20"),
        ("kind", "one_off"),
        ("note", ""),
        ("rate", ""),
    ];
    for (k, v) in fields {
        all.retain(|(key, _)| key != k);
        all.push((k, v));
    }
    all
}

#[tokio::test]
async fn the_donations_page_lists_donations_and_totals() {
    let h = harness(false).await;
    let reader = session_cookie(&h.key, &session(false));
    let res = h
        .app
        .clone()
        .oneshot(get("/donations", &reader))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = body(res).await;
    // 25.00 EUR + 5.00 EUR + 9.20 EUR (10 USD at a rate entered by hand).
    assert!(html.contains("39.20 EUR"), "{html}");
    assert!(
        html.contains("in 4 donation(s)"),
        "all donations that count, including the pending one"
    );
    assert!(html.contains("10.00 USD"));
    assert!(html.contains("≈ 9.20 EUR"));
    assert!(html.contains("2000 JPY"));
    assert!(html.contains("conversion pending"));
    assert!(html.contains("1 donation(s) are not in this total yet"));
    assert!(html.contains("what the card was charged"));
    assert!(
        html.contains("As paid"),
        "several currencies are shown as paid"
    );
    assert!(!html.contains("Record a donation"), "readers get no form");
    assert!(!html.contains("/void"));

    // One project's donations.
    let res = h
        .app
        .oneshot(get("/donations?project=serde-rs-serde", &reader))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("10.00 USD"));
    assert!(
        !html.contains("25.00 EUR"),
        "other projects are not listed or counted"
    );
    assert!(html.contains("9.20 EUR"), "the summary is for this project");
    assert!(html.contains("in 1 donation(s)"));
}

#[tokio::test]
async fn admins_record_a_donation_in_the_base_currency() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/donations?notice=recorded&project=tokio-rs-axum"
    );
    let d = &h.store.list_donations(Some("tokio-rs-axum")).await.unwrap()[0];
    assert_eq!(d.amount_minor, 1250);
    assert_eq!(d.currency, "EUR");
    assert_eq!(d.donated_on.to_string(), "2026-09-20");
    let c = d.conversion.unwrap();
    assert_eq!(
        (c.base_minor, c.source),
        (1250, donarium::model::FxSource::Same)
    );
    assert_eq!(d.recorded_by.name, "alice");

    let res = h
        .app
        .oneshot(get(
            "/donations?notice=recorded&project=tokio-rs-axum",
            &admin,
        ))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("Donation recorded."));
    assert!(html.contains("12.50 EUR"));
}

#[tokio::test]
async fn a_rate_entered_by_hand_converts_exactly() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    // 10.00 USD at 0.9255 is exactly 9.255 EUR, which rounds up.
    let res = h
        .app
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form(
                "csrf-token",
                &[("amount", "10.00"), ("currency", "usd"), ("rate", "0,9255")],
            ),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let d = &h.store.list_donations(Some("tokio-rs-axum")).await.unwrap()[0];
    assert_eq!(d.currency, "USD", "the currency is normalised");
    let c = d.conversion.unwrap();
    assert_eq!(
        (c.base_minor, c.source),
        (926, donarium::model::FxSource::Manual)
    );
    assert_eq!(c.rate, 0.9255);
}

#[tokio::test]
async fn a_foreign_donation_without_a_rate_is_pending() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[("amount", "1500"), ("currency", "JPY")]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=pending")
    );
    let d = &h.store.list_donations(Some("tokio-rs-axum")).await.unwrap()[0];
    assert_eq!(
        (d.amount_minor, d.conversion),
        (1500, None),
        "yen have no cents"
    );
}

#[tokio::test]
async fn bad_donations_are_rejected_and_nothing_is_stored() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let before = h.store.list_donations(None).await.unwrap().len();
    let long_note = "x".repeat(501);
    for (fields, message) in [
        (vec![("project", "")], "choose a project"),
        (vec![("project", "no-such-project")], "choose a project"),
        (vec![("platform", "paypal")], "where you donated"),
        (vec![("amount", "")], "enter the amount"),
        (vec![("amount", "0")], "greater than zero"),
        (vec![("amount", "-5")], "enter the amount"),
        (vec![("amount", "1,234.56")], "thousands separators"),
        (vec![("amount", "12.345")], "2 decimal places"),
        (
            vec![("amount", "1.5"), ("currency", "JPY")],
            "0 decimal places",
        ),
        (vec![("currency", "EURO")], "three-letter"),
        (vec![("date", "yesterday")], "YYYY-MM-DD"),
        (vec![("date", "2030-01-01")], "in the future"),
        (vec![("date", "1999-12-31")], "too long ago"),
        (vec![("kind", "monthly")], "one-off or recurring"),
        (vec![("note", &long_note)], "too long"),
        (vec![("rate", "0.9")], "no rate is needed"),
        (vec![("currency", "USD"), ("rate", "-1")], "greater than 0"),
        (
            vec![("currency", "USD"), ("rate", "abc")],
            "must be a number",
        ),
    ] {
        let res = h
            .app
            .clone()
            .oneshot(post(
                "/donations",
                &admin,
                &donation_form("csrf-token", &fields),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{fields:?}");
        let html = body(res).await;
        assert!(
            html.contains(message),
            "{fields:?}: expected {message:?} in {html}"
        );
    }
    assert_eq!(h.store.list_donations(None).await.unwrap().len(), before);
}

#[tokio::test]
async fn a_rejected_donation_keeps_what_was_typed() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form(
                "csrf-token",
                &[("amount", "12.345"), ("note", "for the new release")],
            ),
        ))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("value=\"12.345\""));
    assert!(html.contains("for the new release"));
    assert!(
        html.contains("<option value=\"tokio-rs-axum\" selected"),
        "the project stays chosen"
    );
}

#[tokio::test]
async fn admins_void_a_donation_and_it_stays_in_the_history() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let id = h
        .store
        .list_donations(Some("bigskysoftware-htmx"))
        .await
        .unwrap()[0]
        .id;
    let res = h
        .app
        .clone()
        .oneshot(post(
            &format!("/donations/{id}/void"),
            &admin,
            &[("_csrf", "csrf-token"), ("project", "")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/donations?notice=voided&project="
    );

    let res = h
        .app
        .clone()
        .oneshot(get("/donations", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("voided"));
    assert!(html.contains("alice, 2026-"), "who and when: {html}");
    // The total no longer includes it: 25.00 + 9.20.
    assert!(html.contains("34.20 EUR"), "{html}");

    // Voiding again is refused, with the reason.
    let res = h
        .app
        .oneshot(post(
            &format!("/donations/{id}/void"),
            &admin,
            &[("_csrf", "csrf-token"), ("project", "")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(body(res).await.contains("already voided"));
}

#[tokio::test]
async fn donation_changes_need_admin_csrf_and_origin() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let reader = session_cookie(&h.key, &session(false));
    let before = h.store.list_donations(None).await.unwrap();
    let id = before[0].id;
    for (path, fields) in [
        ("/donations".to_string(), donation_form("csrf-token", &[])),
        (
            format!("/donations/{id}/void"),
            vec![("_csrf", "csrf-token")],
        ),
    ] {
        let with_csrf = |csrf: &str| -> Vec<(String, String)> {
            fields
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        if *k == "_csrf" {
                            csrf.to_string()
                        } else {
                            v.to_string()
                        },
                    )
                })
                .collect()
        };
        let send = |cookie: &str, origin: Option<&str>, csrf: &str| {
            let owned = with_csrf(csrf);
            let refs: Vec<(&str, &str)> = owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            form_post(&path, cookie, origin, &encode(&refs))
        };
        let r = |req| h.app.clone().oneshot(req);
        assert_eq!(
            r(send(&reader, Some(ORIGIN), "csrf-token"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "reader {path}"
        );
        assert_eq!(
            r(send(&admin, Some(ORIGIN), "wrong"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "csrf {path}"
        );
        assert_eq!(
            r(send(&admin, None, "csrf-token")).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "origin {path}"
        );
    }
    assert_eq!(h.store.list_donations(None).await.unwrap(), before);
}

#[tokio::test]
async fn a_stale_login_must_re_authenticate_before_recording() {
    let h = harness_args(false, &["--step-up-max-age", "15m"]).await;
    let mut stale = session(true);
    stale.auth_time = Some(jiff::Timestamp::now().as_second() - 3600);
    let cookie = session_cookie(&h.key, &stale);
    let before = h.store.list_donations(None).await.unwrap().len();
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &cookie,
            &donation_form("csrf-token", &[]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        res.headers()[header::LOCATION],
        "/auth/login?reauth=true&next=%2Fdonations"
    );
    let id = h.store.list_donations(None).await.unwrap()[0].id;
    let res = h
        .app
        .clone()
        .oneshot(post(
            &format!("/donations/{id}/void"),
            &cookie,
            &[("_csrf", "csrf-token")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        h.store.list_donations(None).await.unwrap().len(),
        before,
        "nothing was recorded"
    );

    // A fresh login records.
    let fresh = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .oneshot(post(
            "/donations",
            &fresh,
            &donation_form("csrf-token", &[]),
        ))
        .await
        .unwrap();
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=recorded")
    );
}

#[tokio::test]
async fn the_project_page_shows_its_donations() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(get("/projects/serde-rs-serde", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("1 so far, 9.20 EUR in total."), "{html}");
    assert!(html.contains("10.00 USD"));
    assert!(html.contains("/donations?project=serde-rs-serde#record"));
    // A project with none says so.
    let res = h
        .app
        .oneshot(get("/projects/prometheus-prometheus", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("0 so far, 0.00 EUR in total."));
    assert!(html.contains("No donations recorded yet."));
}

#[tokio::test]
async fn the_project_list_shows_the_last_donation() {
    let h = harness(false).await;
    let reader = session_cookie(&h.key, &session(false));
    let res = h.app.oneshot(get("/", &reader)).await.unwrap();
    let html = body(res).await;
    // The demo's clock is 2026-09-26: htmx 30 days ago, serde 90 days (3 months).
    assert!(html.contains("30d ago"), "{html}");
    assert!(html.contains("3mo ago"));
    assert!(html.contains("never"), "projects nobody donated to");
}

// --- exchange rates ---

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rate_answer(base: &str, day: &str, rate: f64) -> serde_json::Value {
    serde_json::json!({"amount": 1.0, "base": base, "date": day, "rates": {"EUR": rate}})
}

async fn rate_on(server: &MockServer, day: &str, currency: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/{day}")))
        .and(query_param("base", currency))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn first_donation(h: &Harness, project: &str) -> donarium::model::Donation {
    h.store
        .list_donations(Some(project))
        .await
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn a_foreign_donation_is_converted_with_the_ecb_rate() {
    let fx = MockServer::start().await;
    // 2026-09-20 is a Sunday: the service answers with Friday's rate.
    rate_on(
        &fx,
        "2026-09-20",
        "USD",
        ResponseTemplate::new(200).set_body_json(rate_answer("USD", "2026-09-18", 0.8531)),
    )
    .await;
    let h = harness_fx(false, &fx.uri()).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[("amount", "10.00"), ("currency", "USD")]),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=recorded")
    );
    let c = first_donation(&h, "tokio-rs-axum")
        .await
        .conversion
        .unwrap();
    assert_eq!(
        (c.base_minor, c.rate, c.source),
        (853, 0.8531, donarium::model::FxSource::Ecb)
    );

    // The page shows the converted amount and where the rate came from.
    let res = h
        .app
        .oneshot(get("/donations?project=tokio-rs-axum", &admin))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("≈ 8.53 EUR"), "{html}");
    assert!(html.contains("ECB rate: 1 USD = 0.8531 EUR"));
    assert!(!html.contains("conversion pending"));
}

#[tokio::test]
async fn a_rate_typed_by_hand_wins_over_the_service() {
    let fx = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&fx)
        .await;
    let h = harness_fx(false, &fx.uri()).await;
    let admin = session_cookie(&h.key, &session(true));
    h.app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form(
                "csrf-token",
                &[("amount", "10.00"), ("currency", "USD"), ("rate", "0.9")],
            ),
        ))
        .await
        .unwrap();
    let c = first_donation(&h, "tokio-rs-axum")
        .await
        .conversion
        .unwrap();
    assert_eq!(
        (c.base_minor, c.source),
        (900, donarium::model::FxSource::Manual)
    );
}

#[tokio::test]
async fn when_the_service_is_down_the_donation_is_kept_and_converted_later() {
    let fx = MockServer::start().await;
    rate_on(&fx, "2026-09-20", "USD", ResponseTemplate::new(503)).await;
    let h = harness_fx(false, &fx.uri()).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[("amount", "10.00"), ("currency", "USD")]),
        ))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "recording is not blocked"
    );
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=pending-later")
    );
    assert_eq!(first_donation(&h, "tokio-rs-axum").await.conversion, None);

    let res = h
        .app
        .clone()
        .oneshot(get(
            "/donations?notice=pending-later&project=tokio-rs-axum",
            &admin,
        ))
        .await
        .unwrap();
    let html = body(res).await;
    assert!(html.contains("could not be reached"));
    assert!(html.contains("conversion pending"));
    assert!(html.contains("Set rate"));

    // The service comes back, and the background task fills it in.
    fx.reset().await;
    rate_on(
        &fx,
        "2026-09-20",
        "USD",
        ResponseTemplate::new(200).set_body_json(rate_answer("USD", "2026-09-18", 0.8)),
    )
    .await;
    assert_eq!(h.fx.fill_pending(&h.store).await.unwrap(), 1);
    let c = first_donation(&h, "tokio-rs-axum")
        .await
        .conversion
        .unwrap();
    assert_eq!(
        (c.base_minor, c.source),
        (800, donarium::model::FxSource::Ecb)
    );
    let metrics = h
        .ops
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = body(metrics).await;
    assert!(
        text.contains("donarium_fx_requests_total{result=\"ok\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("donarium_fx_requests_total{result=\"error\"} 1"),
        "{text}"
    );
}

#[tokio::test]
async fn an_unsupported_currency_is_set_by_hand_and_only_once() {
    let fx = MockServer::start().await;
    rate_on(&fx, "2026-09-20", "BTC", ResponseTemplate::new(404)).await;
    let h = harness_fx(false, &fx.uri()).await;
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[("amount", "0.01"), ("currency", "BTC")]),
        ))
        .await
        .unwrap();
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=pending-unsupported")
    );
    let donation = first_donation(&h, "tokio-rs-axum").await;
    assert_eq!(donation.conversion, None);
    let id = donation.id;

    let set = |rate: &str| {
        post(
            &format!("/donations/{id}/rate"),
            &admin,
            &[
                ("_csrf", "csrf-token"),
                ("rate", rate),
                ("project", "tokio-rs-axum"),
            ],
        )
    };
    // A bad rate changes nothing.
    for bad in ["", "abc", "0", "-3"] {
        let res = h.app.clone().oneshot(set(bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
    }
    assert_eq!(first_donation(&h, "tokio-rs-axum").await.conversion, None);

    // 0.01 BTC at 50000 EUR is 500.00 EUR.
    let res = h.app.clone().oneshot(set("50000")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=rated")
    );
    let c = first_donation(&h, "tokio-rs-axum")
        .await
        .conversion
        .unwrap();
    assert_eq!(
        (c.base_minor, c.source),
        (50_000, donarium::model::FxSource::Manual)
    );

    // A conversion is never changed afterwards.
    let res = h.app.clone().oneshot(set("1")).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert!(body(res).await.contains("already has its amount"));
    assert_eq!(
        first_donation(&h, "tokio-rs-axum")
            .await
            .conversion
            .unwrap()
            .base_minor,
        50_000
    );

    // The currency is not asked about again for another donation.
    fx.verify().await;
}

#[tokio::test]
async fn rates_off_means_pending_with_a_clear_message() {
    let h = harness(false).await; // no rate service
    let admin = session_cookie(&h.key, &session(true));
    let res = h
        .app
        .clone()
        .oneshot(post(
            "/donations",
            &admin,
            &donation_form("csrf-token", &[("amount", "10.00"), ("currency", "USD")]),
        ))
        .await
        .unwrap();
    assert!(
        res.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("notice=pending-off")
    );
    let res = h
        .app
        .oneshot(get("/donations?notice=pending-off", &admin))
        .await
        .unwrap();
    assert!(body(res).await.contains("Exchange rates are turned off"));
}

#[tokio::test]
async fn setting_a_rate_needs_admin_csrf_and_origin() {
    let h = harness(false).await;
    let admin = session_cookie(&h.key, &session(true));
    let reader = session_cookie(&h.key, &session(false));
    // The demo has one pending donation: 2000 JPY to paperless.
    let pending = h.store.pending_conversions(10).await.unwrap().remove(0);
    let path = format!("/donations/{}/rate", pending.id);
    let send = |cookie: &str, origin: Option<&str>, csrf: &str| {
        form_post(
            &path,
            cookie,
            origin,
            &encode(&[("_csrf", csrf), ("rate", "0.006")]),
        )
    };
    let r = |req| h.app.clone().oneshot(req);
    assert_eq!(
        r(send(&reader, Some(ORIGIN), "csrf-token"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        r(send(&admin, Some(ORIGIN), "wrong"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        r(send(&admin, None, "csrf-token")).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(h.store.pending_conversions(10).await.unwrap().len(), 1);
    // And a donation that does not exist.
    let res = h
        .app
        .oneshot(post(
            "/donations/9999/rate",
            &admin,
            &[("_csrf", "csrf-token"), ("rate", "1")],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

mod receipts {
    use super::*;
    use common::harness_paperless;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn paperless() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .and(header("Authorization", "Token test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 1,
                "results": [{"id": 42, "title": "Receipt <b>Sponsors</b>", "created": "2026-09-26"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/42/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id": 42, "title": "GitHub Sponsors receipt", "created": "2026-09-26"}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/99/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_receipt_is_found_linked_and_unlinked() {
        let server = paperless().await;
        let h = harness_paperless(&server.uri()).await;
        let admin = session_cookie(&h.key, &session(true));
        let id = h
            .store
            .list_donations(Some("bigskysoftware-htmx"))
            .await
            .unwrap()[0]
            .id;

        // Every donation offers to find a receipt.
        let html = body(
            h.app
                .clone()
                .oneshot(get("/donations", &admin))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains(&format!("/donations/{id}/receipt")));

        // Suggestions come from the date, and titles are escaped.
        let res = h
            .app
            .clone()
            .oneshot(get(&format!("/donations/{id}/receipt"), &admin))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body(res).await;
        assert!(html.contains("within a week"));
        assert!(
            html.contains("Receipt &#60;b&#62;Sponsors")
                || html.contains("Receipt &lt;b&gt;Sponsors")
        );
        assert!(!html.contains("<b>Sponsors"));

        // Linking needs the CSRF token, and the document has to exist.
        let bad = [("_csrf", "wrong"), ("document", "42")];
        let res = h
            .app
            .clone()
            .oneshot(post(&format!("/donations/{id}/receipt"), &admin, &bad))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let missing = [("_csrf", "csrf-token"), ("document", "99")];
        let res = h
            .app
            .clone()
            .oneshot(post(&format!("/donations/{id}/receipt"), &admin, &missing))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let junk = [("_csrf", "csrf-token"), ("document", "../x")];
        let res = h
            .app
            .clone()
            .oneshot(post(&format!("/donations/{id}/receipt"), &admin, &junk))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let ok = [("_csrf", "csrf-token"), ("document", "42")];
        let res = h
            .app
            .clone()
            .oneshot(post(&format!("/donations/{id}/receipt"), &admin, &ok))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        // Linking twice keeps one link.
        h.app
            .clone()
            .oneshot(post(&format!("/donations/{id}/receipt"), &admin, &ok))
            .await
            .unwrap();
        assert_eq!(h.store.list_receipts().await.unwrap().len(), 1);

        let html = body(
            h.app
                .clone()
                .oneshot(get("/donations", &admin))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("GitHub Sponsors receipt"));
        assert!(html.contains(&format!("{}/documents/42/details", server.uri())));

        let rid = h.store.list_receipts().await.unwrap()[0].id;
        let res = h
            .app
            .clone()
            .oneshot(post(
                &format!("/receipts/{rid}/delete"),
                &admin,
                &[("_csrf", "csrf-token"), ("project", "")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(h.store.list_receipts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_admins_link_receipts_and_a_search_can_name_what_to_find() {
        let server = paperless().await;
        let h = harness_paperless(&server.uri()).await;
        let user = session_cookie(&h.key, &session(false));
        let res = h
            .app
            .clone()
            .oneshot(get("/donations/1/receipt", &user))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = h
            .app
            .clone()
            .oneshot(post(
                "/donations/1/receipt",
                &user,
                &[("_csrf", "csrf-token"), ("document", "42")],
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .and(query_param("query", "paperless"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .with_priority(1)
            .mount(&server)
            .await;
        let admin = session_cookie(&h.key, &session(true));
        let res = h
            .app
            .oneshot(get("/donations/1/receipt?q=paperless", &admin))
            .await
            .unwrap();
        assert!(body(res).await.contains("Nothing matches"));
    }

    #[tokio::test]
    async fn without_paperless_there_is_nothing_to_link() {
        let h = harness(false).await;
        let admin = session_cookie(&h.key, &session(true));
        let html = body(
            h.app
                .clone()
                .oneshot(get("/donations", &admin))
                .await
                .unwrap(),
        )
        .await;
        assert!(!html.contains("Find receipt"));
        let res = h
            .app
            .oneshot(get("/donations/1/receipt", &admin))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
