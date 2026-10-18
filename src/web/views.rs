//! View models: projects turned into pre-formatted rows for the templates.

use jiff::{SignedDuration, Timestamp};

use crate::funding;
use crate::model::{Donation, ForgeHosts, Funding, FxSource, Origin, Project, Source, Totals};
use crate::money::{format_amount, minor_exponent};
use crate::ranking::Ranked;
use crate::store::UsageEntry;

pub fn fmt_date(t: Timestamp) -> String {
    t.strftime("%Y-%m-%d").to_string()
}

/// "12d ago", "in 3d", "now". Coarse on purpose: the exact date is in the title.
pub fn relative(t: Timestamp, now: Timestamp) -> String {
    let d = t.duration_since(now);
    if d.abs() < SignedDuration::from_secs(60) {
        return "now".into();
    }
    let days = d.abs().as_secs() / 86_400;
    let span = match days {
        0 => format!("{}h", d.abs().as_secs() / 3_600),
        1..=59 => format!("{days}d"),
        60..=729 => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    };
    if d.is_positive() {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

#[derive(Clone, Debug)]
pub struct ProjectRow {
    pub name: String,
    pub repo: String,
    pub usage: String,
    /// The part of the usage an admin assigned.
    pub manual_usage: String,
    pub last_donation: String,
    pub last_donation_date: String,
    /// Display names of the ways to donate, once each.
    pub funding: Vec<String>,
    /// Added by hand rather than found by a scan.
    pub manual: bool,
    pub ignored: bool,
    pub notes: String,
    pub donation_count: i64,
    /// The first way to donate that has a safe link, if any.
    pub donate_url: String,
    pub donate_label: String,
    /// Set by [`ProjectRow::with_ranking`]; empty for a project not ranked.
    pub rank: String,
    pub share: String,
    pub staleness: String,
    pub priority: String,
}

/// One way to donate, ready to show.
#[derive(Clone, Debug)]
pub struct FundingLink {
    pub platform: String,
    pub platform_name: String,
    pub handle: String,
    /// Empty if there is no safe link.
    pub url: String,
    pub manual: bool,
}

impl FundingLink {
    pub fn new(f: &Funding) -> FundingLink {
        FundingLink {
            platform: f.platform.clone(),
            platform_name: funding::display_name(&f.platform).to_string(),
            handle: f.handle.clone(),
            url: funding::link(&f.platform, &f.handle, f.url.as_deref()).unwrap_or_default(),
            manual: f.manual,
        }
    }
}

impl ProjectRow {
    pub fn new(p: &Project, now: Timestamp) -> ProjectRow {
        let mut funding: Vec<String> = Vec::new();
        for f in &p.funding {
            let name = funding::display_name(&f.platform).to_string();
            if !funding.contains(&name) {
                funding.push(name);
            }
        }
        ProjectRow {
            name: p.name.clone(),
            repo: p.repo.clone(),
            usage: format!("{:.1}", p.usage_score),
            manual_usage: fmt_weight(p.manual_usage),
            last_donation: p
                .last_donation
                .map(|t| relative(t, now))
                .unwrap_or_else(|| "never".into()),
            last_donation_date: p.last_donation.map(fmt_date).unwrap_or_default(),
            funding,
            manual: p.origin == Origin::Manual,
            ignored: p.ignored,
            notes: p.notes.clone(),
            donation_count: p.donation_count,
            donate_url: p
                .funding
                .iter()
                .find_map(|f| funding::link(&f.platform, &f.handle, f.url.as_deref()))
                .unwrap_or_default(),
            donate_label: p
                .funding
                .iter()
                .find(|f| funding::link(&f.platform, &f.handle, f.url.as_deref()).is_some())
                .map(|f| funding::display_name(&f.platform).to_string())
                .unwrap_or_default(),
            rank: String::new(),
            share: String::new(),
            staleness: String::new(),
            priority: String::new(),
        }
    }

    /// Adds where the project stands in the ranking.
    pub fn with_ranking(mut self, r: &Ranked, position: Option<usize>) -> ProjectRow {
        self.rank = position.map(|p| p.to_string()).unwrap_or_default();
        self.share = fmt_percent(r.usage_share);
        self.staleness = format!("×{:.1}", r.staleness);
        self.priority = format!("{:.2}", r.priority);
        self
    }

    /// Case-insensitive match on name and repository, for the filter box.
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.name.to_lowercase().contains(&q) || self.repo.to_lowercase().contains(&q)
    }
}

/// 0.123 as "12%", and "<1%" for anything that rounds to nothing.
pub fn fmt_percent(share: f64) -> String {
    let pct = share * 100.0;
    if pct <= 0.0 {
        "0%".into()
    } else if pct < 1.0 {
        "<1%".into()
    } else {
        format!("{pct:.0}%")
    }
}

/// How a project stands against its fair share, for its page.
#[derive(Clone, Debug)]
pub struct StandingView {
    pub position: String,
    pub of: usize,
    pub share: String,
    pub staleness: String,
    pub days: String,
    pub priority: String,
    pub donated: String,
    pub fair_share: String,
    /// Donated as a percentage of the fair share, 0 to 100, for a bar.
    pub progress: i64,
    pub shortfall: String,
    pub over: bool,
    pub has_fair_share: bool,
}

impl StandingView {
    pub fn new(r: &Ranked, position: Option<usize>, of: usize, base: &str) -> StandingView {
        let shortfall = r.shortfall_minor();
        StandingView {
            position: position.map(|p| p.to_string()).unwrap_or_default(),
            of,
            share: fmt_percent(r.usage_share),
            staleness: format!("×{:.1}", r.staleness),
            days: match r.days_since_donation {
                None => "never donated".into(),
                Some(0) => "donated today".into(),
                Some(d) => format!("{d} day(s) since the last donation"),
            },
            priority: format!("{:.2}", r.priority),
            donated: fmt_money(r.donated_minor, base),
            fair_share: fmt_money(r.fair_share_minor, base),
            progress: if r.fair_share_minor > 0 {
                (r.donated_minor * 100 / r.fair_share_minor).clamp(0, 100)
            } else {
                0
            },
            shortfall: fmt_money(shortfall.abs(), base),
            over: shortfall < 0,
            has_fair_share: r.fair_share_minor > 0,
        }
    }
}

/// A source's use of a project, ready to show.
#[derive(Clone, Debug)]
pub struct UsageRow {
    pub source_id: i64,
    pub repo: String,
    pub provider: &'static str,
    pub how: &'static str,
    pub kind: &'static str,
    pub weight: String,
    pub contribution: String,
    pub enabled: bool,
}

impl UsageRow {
    pub fn new(u: &UsageEntry) -> UsageRow {
        UsageRow {
            source_id: u.source_id,
            repo: u.repo.clone(),
            provider: u.provider.as_str(),
            how: if u.via == "images" {
                "container image"
            } else {
                "dependency"
            },
            kind: if u.direct { "direct" } else { "transitive" },
            weight: fmt_weight(u.weight),
            contribution: format!("{:.2}", u.contribution),
            enabled: u.enabled,
        }
    }
}

/// `12.50 EUR`.
pub fn fmt_money(minor: i64, currency: &str) -> String {
    format!("{} {currency}", format_amount(minor, currency))
}

/// A donation, ready to show.
#[derive(Clone, Debug)]
pub struct DonationRow {
    pub id: i64,
    pub date: String,
    pub project: String,
    pub project_repo: String,
    pub platform: String,
    pub amount: String,
    /// In the base currency, when that differs from what was paid and is known.
    pub base: String,
    /// How the base amount was worked out, for a tooltip.
    pub base_how: String,
    /// The conversion to the base currency has not happened yet.
    pub pending: bool,
    pub kind: &'static str,
    pub note: String,
    pub recorded_by: String,
    /// "bob, 2026-10-01", if it was voided.
    pub voided: String,
    /// Linked receipts, filled in by [`super::receipts::attach`].
    pub receipts: Vec<ReceiptLink>,
    /// Receipts can be linked (Paperless is configured).
    pub can_link: bool,
}

#[derive(Clone, Debug)]
pub struct ReceiptLink {
    pub id: i64,
    pub title: String,
    pub url: String,
}

impl DonationRow {
    pub fn new(d: &Donation, base_currency: &str) -> DonationRow {
        let (base, base_how, pending) = match &d.conversion {
            None => (String::new(), String::new(), d.currency != base_currency),
            Some(c) if c.source == FxSource::Same => (String::new(), String::new(), false),
            Some(c) => (
                format!("≈ {}", fmt_money(c.base_minor, base_currency)),
                format!(
                    "{}: 1 {} = {} {base_currency}",
                    c.source.label(),
                    d.currency,
                    fmt_rate(c.rate)
                ),
                false,
            ),
        };
        DonationRow {
            id: d.id,
            date: d.donated_on.to_string(),
            project: d.project.clone(),
            project_repo: d.project_repo.clone(),
            platform: funding::display_name(&d.platform).to_string(),
            amount: fmt_money(d.amount_minor, &d.currency),
            base,
            base_how,
            pending,
            kind: d.kind.label(),
            note: d.note.clone(),
            recorded_by: d.recorded_by.to_string(),
            voided: d
                .voided
                .as_ref()
                .map(|(by, at)| format!("{by}, {}", fmt_date(*at)))
                .unwrap_or_default(),
            receipts: Vec::new(),
            can_link: false,
        }
    }
}

/// "0.923", "160", "0.0061": up to six decimals, no trailing zeros.
pub fn fmt_rate(rate: f64) -> String {
    let text = format!("{rate:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Totals, ready to show.
#[derive(Clone, Debug, Default)]
pub struct TotalsView {
    pub count: usize,
    pub base: String,
    pub unconverted: usize,
    pub by_currency: Vec<(String, String)>,
    /// Worth showing: some donation was paid in a currency other than the base.
    pub show_as_paid: bool,
    pub by_year: Vec<(i32, String)>,
}

impl TotalsView {
    pub fn new(t: &Totals, base_currency: &str) -> TotalsView {
        TotalsView {
            count: t.count,
            base: fmt_money(t.base_minor, base_currency),
            unconverted: t.unconverted,
            by_currency: t
                .by_currency
                .iter()
                .map(|(c, minor)| (c.clone(), fmt_money(*minor, c)))
                .collect(),
            show_as_paid: t.by_currency.iter().any(|(c, _)| c != base_currency),
            by_year: t
                .by_year
                .iter()
                .map(|(y, minor)| (*y, fmt_money(*minor, base_currency)))
                .collect(),
        }
    }
}

/// The base currency's smallest unit as a decimal, for metrics: `0.01`.
pub fn minor_to_major(minor: i64, currency: &str) -> f64 {
    minor as f64 / 10f64.powi(minor_exponent(currency) as i32)
}

/// A source turned into pre-formatted fields for the templates.
#[derive(Clone, Debug)]
pub struct SourceRow {
    pub id: i64,
    pub provider: &'static str,
    pub repo: String,
    /// Built from the configured host and a validated path; empty when the
    /// provider is not configured.
    pub url: String,
    pub weight: String,
    pub scan_method: &'static str,
    pub enabled: bool,
    pub last_scan: String,
    pub last_scan_status: String,
    /// Whether the last scan worked; `None` before the first one.
    pub last_scan_ok: Option<bool>,
    pub updated_by: String,
    pub updated: String,
    pub updated_date: String,
    pub created_by: String,
    pub created_date: String,
}

impl SourceRow {
    pub fn new(s: &Source, now: Timestamp, hosts: &ForgeHosts) -> SourceRow {
        SourceRow {
            id: s.id,
            provider: s.provider.as_str(),
            repo: s.repo.clone(),
            url: hosts.repo_url(s.provider, &s.repo).unwrap_or_default(),
            weight: fmt_weight(s.weight),
            scan_method: s.scan_method.as_str(),
            enabled: s.enabled,
            last_scan: s
                .last_scan_at
                .map(|t| relative(t, now))
                .unwrap_or_else(|| "never".into()),
            last_scan_status: s.last_scan_status.clone().unwrap_or_default(),
            last_scan_ok: s.last_scan_ok,
            updated_by: s.updated_by.to_string(),
            updated: relative(s.updated_at, now),
            updated_date: fmt_date(s.updated_at),
            created_by: s.created_by.to_string(),
            created_date: fmt_date(s.created_at),
        }
    }
}

/// "1", "0.5", "0.25": no trailing zeros.
pub fn fmt_weight(w: f64) -> String {
    format!("{w}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages() {
        assert_eq!(fmt_percent(0.0), "0%");
        assert_eq!(fmt_percent(0.004), "<1%");
        assert_eq!(fmt_percent(0.0149), "1%");
        assert_eq!(fmt_percent(0.123), "12%");
        assert_eq!(fmt_percent(1.0), "100%");
    }

    #[test]
    fn money_and_rates_are_formatted() {
        assert_eq!(fmt_money(1250, "EUR"), "12.50 EUR");
        assert_eq!(fmt_money(1500, "JPY"), "1500 JPY");
        assert_eq!(fmt_rate(0.923), "0.923");
        assert_eq!(fmt_rate(160.0), "160");
        assert_eq!(fmt_rate(0.0061), "0.0061");
        assert_eq!(fmt_rate(0.9230004), "0.923");
        assert_eq!(minor_to_major(1250, "EUR"), 12.5);
        assert_eq!(minor_to_major(1500, "JPY"), 1500.0);
    }

    #[test]
    fn relative_dates_are_coarse() {
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let ago = |days: i64| now - SignedDuration::from_hours(24 * days);
        assert_eq!(relative(now, now), "now");
        assert_eq!(relative(ago(3), now), "3d ago");
        assert_eq!(relative(ago(90), now), "3mo ago");
        assert_eq!(relative(ago(800), now), "2y ago");
        assert_eq!(relative(now + SignedDuration::from_hours(48), now), "in 2d");
    }
}
