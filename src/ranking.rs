//! Which project to support next.
//!
//! `priority = usage share × staleness`. The share is how much of everything
//! you use a project accounts for; staleness grows with the time since you last
//! donated to it. The product keeps spreading donations out: a project you use a
//! lot but paid last week ranks below one you use half as much and have never
//! paid. The formula lives here, in one place, so it can change.

use jiff::civil::Date;

use crate::funding;
use crate::model::Project;

/// A project not donated to for this long (or never) is as stale as it gets,
/// so a very old donation is not worth more than a three-year-old one.
pub const STALENESS_CAP_DAYS: i64 = 3 * 365;

#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub name: String,
    pub usage_score: f64,
    /// Of all projects in the ranking, 0.0 to 1.0.
    pub usage_share: f64,
    /// `None` if there has never been a donation.
    pub days_since_donation: Option<i64>,
    /// 1.0 for a donation today, up to `1 + 3` after three years or never.
    pub staleness: f64,
    pub priority: f64,
    /// Donated so far (counting donations in the base currency).
    pub donated_minor: i64,
    /// What an even split by usage of everything donated would give this
    /// project: `usage_share × total donated`.
    pub fair_share_minor: i64,
    /// Whether there is a link to donate through.
    pub can_donate: bool,
}

impl Ranked {
    /// How far below its fair share the project is (negative when above).
    pub fn shortfall_minor(&self) -> i64 {
        self.fair_share_minor - self.donated_minor
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ranking {
    /// Highest priority first. Ignored projects are not in it.
    pub projects: Vec<Ranked>,
    pub total_usage: f64,
    /// Donated to the projects in the ranking.
    pub total_donated_minor: i64,
}

impl Ranking {
    pub fn get(&self, name: &str) -> Option<&Ranked> {
        self.projects.iter().find(|r| r.name == name)
    }

    /// 1-based position among the projects you can donate to, or `None` if the
    /// project is ignored or has no way to donate.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.projects
            .iter()
            .filter(|r| r.can_donate)
            .position(|r| r.name == name)
            .map(|i| i + 1)
    }
}

pub fn staleness(days_since_donation: Option<i64>) -> f64 {
    let days = days_since_donation.map_or(STALENESS_CAP_DAYS, |d| d.clamp(0, STALENESS_CAP_DAYS));
    1.0 + days as f64 / 365.0
}

/// Ranks the projects that are not ignored, as of `today`.
pub fn rank(projects: &[Project], today: Date) -> Ranking {
    let included: Vec<&Project> = projects.iter().filter(|p| !p.ignored).collect();
    let total_usage: f64 = included.iter().map(|p| p.usage_score).sum();
    let total_donated_minor: i64 = included.iter().map(|p| p.donated_base_minor).sum();

    let mut ranked: Vec<Ranked> = included
        .iter()
        .map(|p| {
            let share = if total_usage > 0.0 {
                p.usage_score / total_usage
            } else {
                0.0
            };
            let days = p.last_donation.map(|t| {
                let donated_on = t.to_zoned(jiff::tz::TimeZone::UTC).date();
                today
                    .since(donated_on)
                    .map_or(0, |span| span.get_days() as i64)
            });
            let staleness = staleness(days);
            Ranked {
                name: p.name.clone(),
                usage_score: p.usage_score,
                usage_share: share,
                days_since_donation: days,
                staleness,
                priority: share * staleness,
                donated_minor: p.donated_base_minor,
                fair_share_minor: (share * total_donated_minor as f64).round() as i64,
                can_donate: p
                    .funding
                    .iter()
                    .any(|f| funding::link(&f.platform, &f.handle, f.url.as_deref()).is_some()),
            }
        })
        .collect();
    // Highest priority first; among equals the one waiting longest, then by name,
    // so the order never changes between two views of the same data.
    ranked.sort_by(|a, b| {
        b.priority
            .total_cmp(&a.priority)
            .then_with(|| b.staleness.total_cmp(&a.staleness))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ranking {
        projects: ranked,
        total_usage,
        total_donated_minor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Funding, Origin};
    use jiff::civil::date;

    const TODAY: Date = date(2026, 10, 1);

    fn project(name: &str, usage: f64, last: Option<Date>, donated: i64, funded: bool) -> Project {
        Project {
            name: name.into(),
            repo: format!("github.com/x/{name}"),
            origin: Origin::Discovered,
            usage_score: usage,
            manual_usage: 0.0,
            ignored: false,
            notes: String::new(),
            last_donation: last.map(|d| format!("{d}T00:00:00Z").parse().unwrap()),
            donation_count: i64::from(last.is_some()),
            donated_base_minor: donated,
            funding: if funded {
                vec![Funding {
                    platform: "github-sponsors".into(),
                    handle: name.into(),
                    url: None,
                    manual: false,
                }]
            } else {
                vec![]
            },
        }
    }

    fn names(r: &Ranking) -> Vec<&str> {
        r.projects.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn staleness_grows_with_time_and_is_capped() {
        assert_eq!(staleness(Some(0)), 1.0);
        assert_eq!(staleness(Some(365)), 2.0);
        assert_eq!(staleness(Some(730)), 3.0);
        assert_eq!(staleness(Some(STALENESS_CAP_DAYS)), 4.0);
        assert_eq!(staleness(Some(STALENESS_CAP_DAYS + 5000)), 4.0, "capped");
        assert_eq!(staleness(None), 4.0, "never donated is the most stale");
        assert_eq!(
            staleness(Some(-5)),
            1.0,
            "a date in the future is treated as today"
        );
    }

    #[test]
    fn a_recent_donation_lowers_a_heavily_used_project_below_a_neglected_one() {
        let projects = [
            project(
                "big-but-paid-last-week",
                10.0,
                Some(date(2026, 9, 24)),
                5000,
                true,
            ),
            project("half-as-used-never-paid", 5.0, None, 0, true),
        ];
        let r = rank(&projects, TODAY);
        assert_eq!(
            names(&r),
            ["half-as-used-never-paid", "big-but-paid-last-week"]
        );
        let never = r.get("half-as-used-never-paid").unwrap();
        // 5/15 share, never donated: staleness 4.
        assert!((never.usage_share - 1.0 / 3.0).abs() < 1e-9);
        assert!((never.priority - 4.0 / 3.0).abs() < 1e-9);
        let paid = r.get("big-but-paid-last-week").unwrap();
        assert_eq!(paid.days_since_donation, Some(7));
        assert!((paid.staleness - (1.0 + 7.0 / 365.0)).abs() < 1e-9);
    }

    #[test]
    fn with_equal_staleness_the_more_used_project_wins() {
        let projects = [
            project("small", 1.0, None, 0, true),
            project("large", 9.0, None, 0, true),
        ];
        assert_eq!(names(&rank(&projects, TODAY)), ["large", "small"]);
    }

    #[test]
    fn shares_add_up_and_ignored_projects_are_left_out() {
        let mut ignored = project("ignored", 100.0, None, 999, true);
        ignored.ignored = true;
        let projects = [
            project("a", 1.0, None, 100, true),
            project("b", 3.0, None, 300, true),
            ignored,
        ];
        let r = rank(&projects, TODAY);
        assert_eq!(r.projects.len(), 2);
        assert!(r.get("ignored").is_none());
        let sum: f64 = r.projects.iter().map(|p| p.usage_share).sum();
        assert!((sum - 1.0).abs() < 1e-9);
        assert_eq!(r.total_usage, 4.0);
        assert_eq!(
            r.total_donated_minor, 400,
            "what went to ignored projects is not counted"
        );
    }

    #[test]
    fn fair_share_is_usage_share_of_what_was_donated() {
        // 400 donated in all. `b` is 75% of the usage, so its fair share is 300
        // and it has had 300; `a` is 25%, fair share 100, and has had 100.
        let projects = [
            project("a", 1.0, None, 100, true),
            project("b", 3.0, None, 300, true),
        ];
        let r = rank(&projects, TODAY);
        assert_eq!(r.get("b").unwrap().fair_share_minor, 300);
        assert_eq!(r.get("b").unwrap().shortfall_minor(), 0);
        // Someone neglected: all 400 went to `a`.
        let skewed = [
            project("a", 1.0, None, 400, true),
            project("b", 3.0, None, 0, true),
        ];
        let r = rank(&skewed, TODAY);
        assert_eq!(r.get("b").unwrap().fair_share_minor, 300);
        assert_eq!(r.get("b").unwrap().shortfall_minor(), 300);
        assert_eq!(
            r.get("a").unwrap().shortfall_minor(),
            -300,
            "above its share"
        );
    }

    #[test]
    fn unused_projects_rank_last_with_no_share() {
        let projects = [
            project("unused-but-never-paid", 0.0, None, 0, true),
            project("used", 1.0, Some(date(2026, 1, 1)), 100, true),
        ];
        let r = rank(&projects, TODAY);
        assert_eq!(names(&r), ["used", "unused-but-never-paid"]);
        let unused = r.get("unused-but-never-paid").unwrap();
        assert_eq!(
            (unused.usage_share, unused.priority, unused.fair_share_minor),
            (0.0, 0.0, 0)
        );
        // Nothing is used at all: no division by zero, nothing is NaN.
        let none = rank(&[project("a", 0.0, None, 0, true)], TODAY);
        assert_eq!(none.projects[0].priority, 0.0);
        assert!(none.projects[0].usage_share.is_finite());
    }

    #[test]
    fn position_counts_only_projects_you_can_donate_to() {
        let projects = [
            project("top-but-no-link", 9.0, None, 0, false),
            project("second", 5.0, None, 0, true),
            project("third", 1.0, None, 0, true),
        ];
        let r = rank(&projects, TODAY);
        assert_eq!(r.position("top-but-no-link"), None);
        assert_eq!(r.position("second"), Some(1));
        assert_eq!(r.position("third"), Some(2));
        assert_eq!(r.position("nope"), None);
        assert!(!r.get("top-but-no-link").unwrap().can_donate);
        // It still has its share, so the numbers stay honest.
        assert!(r.get("top-but-no-link").unwrap().usage_share > 0.5);
    }

    #[test]
    fn a_link_that_is_not_safe_does_not_count_as_a_way_to_donate() {
        let mut p = project("odd", 1.0, None, 0, false);
        p.funding.push(Funding {
            platform: "other".into(),
            handle: "x".into(),
            url: Some("http://insecure.example".into()),
            manual: true,
        });
        assert!(!rank(&[p], TODAY).projects[0].can_donate);
    }

    #[test]
    fn ties_are_broken_the_same_way_every_time() {
        let projects = [
            project("b", 1.0, None, 0, true),
            project("a", 1.0, None, 0, true),
            project("c", 1.0, None, 0, true),
        ];
        let first = names(&rank(&projects, TODAY))
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(first, ["a", "b", "c"]);
        let mut reversed = projects.to_vec();
        reversed.reverse();
        assert_eq!(names(&rank(&reversed, TODAY)), ["a", "b", "c"]);
    }

    #[test]
    fn an_empty_list_ranks_to_nothing() {
        let r = rank(&[], TODAY);
        assert_eq!(r, Ranking::default());
        assert_eq!(r.position("x"), None);
    }
}
