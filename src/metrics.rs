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

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ScanLabels {
    provider: String,
    repo: String,
}

pub struct Metrics {
    registry: Registry,
    usage: Family<ProjectLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    sources: Family<SourceLabels, Gauge>,
    scan_time: Family<ScanLabels, Gauge>,
    scan_ok: Family<ScanLabels, Gauge>,
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
        let scan_time = Family::default();
        registry.register(
            "donarium_source_last_scan_timestamp_seconds",
            "Unix time of the last scan of an enabled source",
            scan_time.clone(),
        );
        let scan_ok = Family::default();
        registry.register(
            "donarium_source_last_scan_ok",
            "1 if the last scan of an enabled source worked, 0 if it failed",
            scan_ok.clone(),
        );
        Arc::new(Metrics {
            registry,
            usage,
            sources,
            scan_time,
            scan_ok,
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
        self.scan_time.clear();
        self.scan_ok.clear();
        for s in sources.iter().filter(|s| s.enabled) {
            let labels = ScanLabels {
                provider: s.provider.as_str().into(),
                repo: s.repo.clone(),
            };
            if let Some(at) = s.last_scan_at {
                self.scan_time.get_or_create(&labels).set(at.as_second());
            }
            if let Some(ok) = s.last_scan_ok {
                self.scan_ok.get_or_create(&labels).set(i64::from(ok));
            }
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
