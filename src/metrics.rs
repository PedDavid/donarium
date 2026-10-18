//! Prometheus metrics. The per-project and per-source gauges are rebuilt from
//! the database on every scrape.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;
use tokio::sync::Mutex;

use crate::store::{Store, StoreError};
use crate::web::views::minor_to_major;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ProjectLabels {
    project: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    state: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabels {
    pub result: String,
}

/// Every `result` an exchange rate lookup can end in. Each series is created at
/// 0 on startup: `increase()` ignores a counter's first sample, so a series that
/// first appears at 1 would hide the first lookup after every restart.
pub const FX_RESULTS: [&str; 4] = ["ok", "unsupported", "rate_limited", "error"];

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CurrencyLabels {
    currency: String,
}

type FloatGauge = Gauge<f64, AtomicU64>;

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
    donated_base: Family<CurrencyLabels, FloatGauge>,
    donated_paid: Family<CurrencyLabels, FloatGauge>,
    donations: Gauge,
    unconverted: Gauge,
    no_receipt: Gauge,
    last_donation: Family<ProjectLabels, Gauge>,
    priority: Family<ProjectLabels, FloatGauge>,
    /// Exchange rate lookups by result (shared with [`crate::fx::Fx`]).
    pub fx_requests: Family<ResultLabels, Counter>,
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
        let donated_base = Family::default();
        registry.register(
            "donarium_donated_base_amount",
            "Donations that count, in the base currency (those awaiting a conversion are left out)",
            donated_base.clone(),
        );
        let donated_paid = Family::default();
        registry.register(
            "donarium_donated_paid_amount",
            "Donations that count, in the currencies they were paid in",
            donated_paid.clone(),
        );
        let donations = Gauge::default();
        registry.register(
            "donarium_donations",
            "Number of donations that count (not voided)",
            donations.clone(),
        );
        let unconverted = Gauge::default();
        registry.register(
            "donarium_donations_without_conversion",
            "Donations still waiting for their amount in the base currency",
            unconverted.clone(),
        );
        let no_receipt = Gauge::default();
        registry.register(
            "donarium_donations_without_receipt",
            "Donations that count and have no receipt linked",
            no_receipt.clone(),
        );
        let last_donation = Family::default();
        registry.register(
            "donarium_last_donation_timestamp_seconds",
            "Unix time of the last donation to a project (the day, at 00:00 UTC)",
            last_donation.clone(),
        );
        let priority = Family::default();
        registry.register(
            "donarium_project_priority",
            "Ranking priority (usage share times staleness) of the 20 projects to support next",
            priority.clone(),
        );
        let fx_requests = Family::<ResultLabels, Counter>::default();
        for result in FX_RESULTS {
            let _ = fx_requests.get_or_create(&ResultLabels {
                result: result.into(),
            });
        }
        registry.register(
            "donarium_fx_requests",
            "Exchange rate lookups by result (ok, unsupported, rate_limited, error)",
            fx_requests.clone(),
        );
        Arc::new(Metrics {
            fx_requests,
            registry,
            usage,
            sources,
            scan_time,
            scan_ok,
            donated_base,
            donated_paid,
            donations,
            unconverted,
            no_receipt,
            last_donation,
            priority,
            scrape: Mutex::new(()),
        })
    }

    /// The OpenMetrics text for `/metrics`, with gauges read from `store`.
    pub async fn render(&self, store: &Store, base_currency: &str) -> Result<String, StoreError> {
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
        let totals = store.donation_totals(None).await?;
        self.donated_base.clear();
        self.donated_paid.clear();
        self.last_donation.clear();
        self.donated_base
            .get_or_create(&CurrencyLabels {
                currency: base_currency.into(),
            })
            .set(minor_to_major(totals.base_minor, base_currency));
        for (currency, minor) in &totals.by_currency {
            self.donated_paid
                .get_or_create(&CurrencyLabels {
                    currency: currency.clone(),
                })
                .set(minor_to_major(*minor, currency));
        }
        self.donations.set(totals.count as i64);
        self.unconverted.set(totals.unconverted as i64);
        self.no_receipt
            .set(store.donations_without_receipt().await?);
        for p in &projects {
            if let Some(at) = p.last_donation {
                self.last_donation
                    .get_or_create(&ProjectLabels {
                        project: p.name.clone(),
                    })
                    .set(at.as_second());
            }
        }
        self.priority.clear();
        let ranking = crate::ranking::rank(&projects, jiff::Zoned::now().date());
        for r in ranking.projects.iter().filter(|r| r.can_donate).take(20) {
            self.priority
                .get_or_create(&ProjectLabels {
                    project: r.name.clone(),
                })
                .set(r.priority);
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
        let text = metrics.render(&store, "EUR").await.unwrap();
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
        assert!(
            metrics
                .render(&store, "EUR")
                .await
                .unwrap()
                .contains("prometheus-prometheus")
        );
        sqlx::query("DELETE FROM project WHERE name = 'prometheus-prometheus'")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(
            !metrics
                .render(&store, "EUR")
                .await
                .unwrap()
                .contains("prometheus-prometheus")
        );
    }
}
