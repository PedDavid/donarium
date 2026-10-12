//! Prometheus metrics. The per-project and per-source gauges are rebuilt from
//! the database on every scrape.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::Mutex;

use crate::store::{Store, StoreError};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ProjectLabels {
    project: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    state: String,
}

pub struct Metrics {
    registry: Registry,
    usage: Family<ProjectLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    sources: Family<SourceLabels, Gauge>,
    /// Held from refresh to encode, so a scrape never sees a half-built state.
    scrape: Mutex<()>,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let mut registry = Registry::default();
        let usage = Family::default();
        registry.register(
            "donarium_project_usage_score",
            "Usage score of the project across the enabled sources",
            usage.clone(),
        );
        let sources = Family::default();
        registry.register(
            "donarium_sources",
            "Configured sources by state (enabled, disabled)",
            sources.clone(),
        );
        Arc::new(Metrics {
            registry,
            usage,
            sources,
            scrape: Mutex::new(()),
        })
    }

    /// The OpenMetrics text for `/metrics`, with gauges read from `store`.
    pub async fn render(&self, store: &Store) -> Result<String, StoreError> {
        let _scrape = self.scrape.lock().await;
        let projects = store.list_projects().await?;
        let sources = store.list_sources().await?;
        self.usage.clear();
        for p in &projects {
            self.usage
                .get_or_create(&ProjectLabels {
                    project: p.name.clone(),
                })
                .set(p.usage_score);
        }
        for state in ["enabled", "disabled"] {
            let count = sources
                .iter()
                .filter(|s| s.enabled == (state == "enabled"))
                .count();
            self.sources
                .get_or_create(&SourceLabels {
                    state: state.into(),
                })
                .set(count as i64);
        }
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("encoding into a String cannot fail");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;

    #[tokio::test]
    async fn exports_project_and_source_gauges() {
        let store = Store::memory().await.unwrap();
        demo::seed(&store, "2026-09-26T12:00:00Z".parse().unwrap())
            .await
            .unwrap();
        let metrics = Metrics::new();
        let text = metrics.render(&store).await.unwrap();
        assert!(
            text.contains("donarium_project_usage_score{project=\"tokio-rs-tokio\"}"),
            "{text}"
        );
        assert!(
            text.contains("donarium_sources{state=\"enabled\"} 3"),
            "{text}"
        );
        assert!(
            text.contains("donarium_sources{state=\"disabled\"} 1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn series_of_deleted_projects_disappear() {
        let store = Store::memory().await.unwrap();
        demo::seed(&store, "2026-09-26T12:00:00Z".parse().unwrap())
            .await
            .unwrap();
        let metrics = Metrics::new();
        assert!(metrics.render(&store).await.unwrap().contains("htmx"));
        sqlx::query("DELETE FROM project WHERE name = 'bigskysoftware-htmx'")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(!metrics.render(&store).await.unwrap().contains("htmx"));
    }
}
