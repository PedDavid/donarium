//! The donations as a CSV file, for tax returns and spreadsheets.

use crate::model::Donation;
use crate::money::format_amount;

const HEADER: [&str; 14] = [
    "date",
    "project",
    "repository",
    "platform",
    "kind",
    "amount",
    "currency",
    "amount_in_base",
    "base_currency",
    "rate",
    "rate_source",
    "note",
    "recorded_by",
    "recorded_at",
];

/// A text cell. A spreadsheet treats a cell that starts with one of `= + - @`
/// (or a tab or carriage return) as a formula, so those get a leading quote.
fn text(value: &str) -> String {
    let guarded = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// The donations that count, in the order given (voided ones are skipped),
/// limited to `year` if one is named.
pub fn donations_csv(donations: &[Donation], base_currency: &str, year: Option<i16>) -> String {
    let mut out = HEADER.join(",");
    out.push_str("\r\n");
    for d in donations
        .iter()
        .filter(|d| d.voided.is_none() && year.is_none_or(|y| d.donated_on.year() == y))
    {
        let (base, rate, source) = match &d.conversion {
            Some(c) => (
                format_amount(c.base_minor, base_currency),
                c.rate.to_string(),
                c.source.as_str().to_string(),
            ),
            None => Default::default(),
        };
        let cells = [
            d.donated_on.to_string(),
            text(&d.project),
            text(&d.project_repo),
            text(&d.platform),
            d.kind.as_str().to_string(),
            format_amount(d.amount_minor, &d.currency),
            d.currency.clone(),
            base,
            base_currency.to_string(),
            rate,
            source,
            text(&d.note),
            text(&d.recorded_by.name),
            d.recorded_at.to_string(),
        ];
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, Conversion, DonationKind, FxSource};
    use jiff::civil::date;

    fn donation(id: i64, day: jiff::civil::Date, note: &str) -> Donation {
        Donation {
            id,
            project: "tokio-rs-tokio".into(),
            project_repo: "github.com/tokio-rs/tokio".into(),
            platform: "github-sponsors".into(),
            amount_minor: 1250,
            currency: "USD".into(),
            donated_on: day,
            conversion: Some(Conversion {
                base_minor: 1156,
                rate: 0.9255,
                source: FxSource::Ecb,
            }),
            kind: DonationKind::OneOff,
            note: note.into(),
            recorded_by: Actor::new("u1", "alice"),
            recorded_at: "2026-10-01T08:00:00Z".parse().unwrap(),
            voided: None,
        }
    }

    #[test]
    fn writes_a_header_and_one_row_per_donation() {
        let csv = donations_csv(&[donation(1, date(2026, 3, 1), "thanks")], "EUR", None);
        let lines: Vec<&str> = csv.split("\r\n").collect();
        assert!(lines[0].starts_with("date,project,repository,"));
        assert_eq!(
            lines[1],
            "2026-03-01,tokio-rs-tokio,github.com/tokio-rs/tokio,github-sponsors,one_off,12.50,USD,11.56,EUR,0.9255,ecb,thanks,alice,2026-10-01T08:00:00Z"
        );
        assert_eq!(lines[2], "", "ends with a line break");
    }

    #[test]
    fn leaves_the_base_amount_empty_while_pending() {
        let mut d = donation(1, date(2026, 3, 1), "");
        d.conversion = None;
        let csv = donations_csv(&[d], "EUR", None);
        assert!(csv.contains(",12.50,USD,,EUR,,,"), "{csv}");
    }

    #[test]
    fn skips_voided_donations_and_other_years() {
        let mut voided = donation(1, date(2026, 3, 1), "");
        voided.voided = Some((
            Actor::new("u2", "bob"),
            "2026-04-01T00:00:00Z".parse().unwrap(),
        ));
        let all = [
            voided,
            donation(2, date(2025, 12, 31), "last year"),
            donation(3, date(2026, 1, 1), "this year"),
        ];
        let csv = donations_csv(&all, "EUR", Some(2026));
        assert!(csv.contains("this year"));
        assert!(!csv.contains("last year"));
        assert_eq!(csv.matches("\r\n").count(), 2);
        assert!(donations_csv(&all, "EUR", None).contains("last year"));
    }

    #[test]
    fn quotes_what_needs_it_and_defuses_formulas() {
        let csv = donations_csv(
            &[
                donation(1, date(2026, 3, 1), "a, \"b\"\nc"),
                donation(2, date(2026, 3, 2), "=HYPERLINK(\"http://evil\")"),
                donation(3, date(2026, 3, 3), "-1+1"),
            ],
            "EUR",
            None,
        );
        assert!(csv.contains(",\"a, \"\"b\"\"\nc\","));
        assert!(csv.contains(",\"'=HYPERLINK(\"\"http://evil\"\")\","));
        assert!(csv.contains(",'-1+1,"));
    }
}
