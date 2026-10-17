//! View models: projects turned into pre-formatted rows for the templates.

use jiff::{SignedDuration, Timestamp};

use crate::funding;
use crate::model::{ForgeHosts, Funding, Origin, Project, Source};

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
        }
    }

    /// Case-insensitive match on name and repository, for the filter box.
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.name.to_lowercase().contains(&q) || self.repo.to_lowercase().contains(&q)
    }
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
