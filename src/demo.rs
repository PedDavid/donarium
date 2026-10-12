//! Sample data for `--demo`.

use jiff::{SignedDuration, Timestamp};

use crate::model::Project;

pub fn sample_projects(now: Timestamp) -> Vec<Project> {
    let days = |n: i64| now - SignedDuration::from_hours(24 * n);
    let p =
        |name: &str, repo: &str, usage: f64, last: Option<Timestamp>, funding: &[&str]| Project {
            name: name.into(),
            repo: repo.into(),
            usage_score: usage,
            last_donation: last,
            funding: funding.iter().map(|s| s.to_string()).collect(),
        };
    vec![
        p(
            "tokio-rs-tokio",
            "github.com/tokio-rs/tokio",
            14.5,
            Some(days(400)),
            &["github-sponsors", "opencollective"],
        ),
        p(
            "tokio-rs-axum",
            "github.com/tokio-rs/axum",
            9.0,
            None,
            &["github-sponsors"],
        ),
        p(
            "serde-rs-serde",
            "github.com/serde-rs/serde",
            12.0,
            Some(days(90)),
            &["github-sponsors"],
        ),
        p(
            "htmx",
            "github.com/bigskysoftware/htmx",
            3.5,
            Some(days(30)),
            &["github-sponsors"],
        ),
        p(
            "prometheus-prometheus",
            "github.com/prometheus/prometheus",
            6.0,
            None,
            &[],
        ),
    ]
}
