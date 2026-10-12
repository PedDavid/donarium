//! Sample data for `--demo`, written through the same store as real data.

use jiff::Timestamp;

use crate::model::{Actor, NewSource, Provider, ScanMethod, SourceUpdate};
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
            sqlx::query("INSERT INTO usage (source_id, project_id, kind) VALUES (?, ?, ?)")
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
    Ok(())
}
