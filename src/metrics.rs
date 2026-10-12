//! Prometheus metrics. Per-project gauges are computed from the repository
//! on every scrape.

use std::sync::Arc;

use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, EncodeMetric};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::gauge::ConstGauge;
use prometheus_client::registry::Registry;

use crate::repo::Repository;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ProjectLabels {
    project: String,
}

pub struct Metrics {
    registry: Registry,
}

impl Metrics {
    pub fn new(repo: Arc<dyn Repository>) -> Arc<Self> {
        let mut registry = Registry::default();
        registry.register_collector(Box::new(ProjectCollector { repo }));
        Arc::new(Metrics { registry })
    }

    /// The OpenMetrics text for `/metrics`.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("encoding into a String cannot fail");
        out
    }
}

/// Computes the per-project gauges from the repository at scrape time.
#[derive(Debug)]
struct ProjectCollector {
    repo: Arc<dyn Repository>,
}

impl prometheus_client::collector::Collector for ProjectCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let projects = self.repo.list();
        let mut usage = encoder.encode_descriptor(
            "donarium_project_usage_score",
            "Usage score of the project across the scanned sources",
            None,
            MetricType::Gauge,
        )?;
        for p in &projects {
            let labels = ProjectLabels {
                project: p.name.clone(),
            };
            ConstGauge::new(p.usage_score).encode(usage.encode_family(&labels)?)?;
        }
        let mut last = encoder.encode_descriptor(
            "donarium_last_donation_timestamp_seconds",
            "Unix time of the last donation to the project",
            None,
            MetricType::Gauge,
        )?;
        for p in projects.iter().filter(|p| p.last_donation.is_some()) {
            let labels = ProjectLabels {
                project: p.name.clone(),
            };
            let at = p.last_donation.map(|t| t.as_second()).unwrap_or(0);
            ConstGauge::new(at).encode(last.encode_family(&labels)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use crate::repo::MemoryRepository;

    #[test]
    fn exports_per_project_gauges() {
        let now = jiff::Timestamp::from_second(1_800_000_000).unwrap();
        let repo = Arc::new(MemoryRepository::new(demo::sample_projects(now)));
        let text = Metrics::new(repo).encode();
        assert!(text.contains("donarium_project_usage_score{project=\"tokio-rs-tokio\"} 14.5"));
        assert!(text.contains("donarium_last_donation_timestamp_seconds{project=\"htmx\"}"));
        // Never-donated projects have no timestamp series.
        assert!(
            !text.contains("donarium_last_donation_timestamp_seconds{project=\"tokio-rs-axum\"}")
        );
    }
}
