//! Sample data for `--demo`, written through the same store as real data.

use jiff::Timestamp;

use crate::model::{Actor, NewSource, Provider, ScanMethod, SourceUpdate};
use crate::scan::image::ImageRef;
use crate::scan::manifests::{ImageUse, Skipped};
use crate::scan::usage::Lookup;
use crate::store::{Store, StoreError};

/// (name, repo, usage per source index, direct?, funding platforms)
type Sample = (
    &'static str,
    &'static str,
    &'static [(usize, bool)],
    &'static [&'static str],
);

const PROJECTS: &[Sample] = &[
    (
        "tokio-rs-tokio",
        "github.com/tokio-rs/tokio",
        &[(0, true), (1, true), (2, false)],
        &["github-sponsors", "opencollective"],
    ),
    (
        "tokio-rs-axum",
        "github.com/tokio-rs/axum",
        &[(0, true), (1, true)],
        &["github-sponsors"],
    ),
    (
        "serde-rs-serde",
        "github.com/serde-rs/serde",
        &[(0, true), (1, false), (2, false)],
        &["github-sponsors"],
    ),
    (
        "bigskysoftware-htmx",
        "github.com/bigskysoftware/htmx",
        &[(0, true)],
        &["github-sponsors"],
    ),
    (
        "prometheus-prometheus",
        "github.com/prometheus/prometheus",
        &[(2, true)],
        &[],
    ),
];

pub async fn seed(store: &Store, now: Timestamp) -> Result<(), StoreError> {
    let demo = Actor::new("demo", "demo");
    let mut ids = Vec::new();
    for (provider, repo, weight, enabled) in [
        (Provider::Github, "PedDavid/infra", 1.0, true),
        (Provider::Github, "PedDavid/clavium", 0.6, true),
        (Provider::Gitlab, "homelab/dashboards", 0.3, true),
        (Provider::Github, "PedDavid/old-experiments", 0.1, false),
    ] {
        let source = store
            .create_source(
                &NewSource {
                    provider,
                    repo: repo.into(),
                    weight,
                    scan_method: ScanMethod::Auto,
                },
                &demo,
                now,
            )
            .await?;
        if !enabled {
            store
                .update_source(
                    source.id,
                    &SourceUpdate {
                        weight,
                        scan_method: ScanMethod::Auto,
                        enabled: false,
                    },
                    &demo,
                    now,
                )
                .await?;
        }
        ids.push(source.id);
    }

    let pool = store.pool();
    let stamp = now.to_string();
    for (name, repo, usage, funding) in PROJECTS {
        let project_id: i64 = sqlx::query_scalar(
            "INSERT INTO project (name, repo, first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(name)
        .bind(repo)
        .bind(&stamp)
        .bind(&stamp)
        .fetch_one(pool)
        .await?;
        for (source, direct) in *usage {
            sqlx::query(
                "INSERT INTO usage (source_id, project_id, via, kind) VALUES (?, ?, 'dependencies', ?)",
            )
                .bind(ids[*source])
                .bind(project_id)
                .bind(if *direct { "direct" } else { "transitive" })
                .execute(pool)
                .await?;
        }
        for platform in *funding {
            sqlx::query(
                "INSERT INTO project_funding (project_id, platform, handle) VALUES (?, ?, ?)",
            )
            .bind(project_id)
            .bind(platform)
            .bind(name)
            .execute(pool)
            .await?;
        }
    }
    seed_images(store, ids[2], now).await
}

/// What an image scan of the GitLab source would have found, including the
/// cases that need an admin: an image without a label and a guessed project.
async fn seed_images(store: &Store, source_id: i64, now: Timestamp) -> Result<(), StoreError> {
    let used = |image: &str, file: &str, origin: &str| ImageUse {
        image: ImageRef::parse(image).expect("a valid demo image"),
        file: file.into(),
        origin: origin.into(),
    };
    let images = [
        used(
            "jellyfin/jellyfin:10.9.11",
            "media/compose.yaml",
            "compose service jellyfin",
        ),
        used(
            "lscr.io/linuxserver/sonarr:latest",
            "media/compose.yaml",
            "compose service sonarr",
        ),
        used(
            "lscr.io/linuxserver/radarr:latest",
            "media/compose.yaml",
            "compose service radarr",
        ),
        used(
            "qmcgaw/gluetun:v3",
            "media/compose.yaml",
            "compose service vpn",
        ),
        used(
            "ghcr.io/paperless-ngx/paperless-ngx:2.13",
            "docs/compose.yaml",
            "compose service paperless",
        ),
    ];
    let skipped = [Skipped {
        file: "media/compose.yaml".into(),
        value: "${PROXY_IMAGE}".into(),
        reason: "the image name depends on a variable".into(),
    }];
    store.store_scan(source_id, &images, &skipped).await?;
    let label = |upstream: Option<&str>, error: Option<&str>| Lookup {
        upstream: upstream.map(str::to_string),
        label: upstream.map(|u| format!("https://{u}")),
        error: error.map(str::to_string),
        retry: false,
    };
    for (registry, repository, lookup) in [
        (
            "docker.io",
            "jellyfin/jellyfin",
            label(Some("github.com/jellyfin/jellyfin"), None),
        ),
        (
            "lscr.io",
            "linuxserver/sonarr",
            label(Some("github.com/linuxserver/docker-sonarr"), None),
        ),
        (
            "lscr.io",
            "linuxserver/radarr",
            label(Some("github.com/linuxserver/docker-radarr"), None),
        ),
        (
            "docker.io",
            "qmcgaw/gluetun",
            label(None, Some("image not found (it may be private)")),
        ),
        ("ghcr.io", "paperless-ngx/paperless-ngx", label(None, None)),
    ] {
        store
            .save_lookup(registry, repository, &lookup, now)
            .await?;
    }
    store.rebuild_image_usage(now).await?;
    store
        .finish_scan(
            source_id,
            now,
            true,
            "5 images, 4 projects, 1 unresolved, 1 skipped",
        )
        .await?;
    Ok(())
}
