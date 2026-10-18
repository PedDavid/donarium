//! Recurring donations: when a plan's donations fall due, and recording them.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use jiff::ToSpan;
use jiff::civil::Date;
use tracing::{info, warn};

use crate::fx::{Fx, FxError};
use crate::model::{DonationKind, Every, NewDonation, Recurring};
use crate::store::{Store, StoreError};

/// The most donations one plan records in a single run, so a plan that starts
/// years ago does not make hundreds of rate lookups at once.
const MAX_PER_RUN: usize = 36;

/// Every date `plan` falls on up to `today`, oldest first. Each is counted from
/// the start date, so a plan starting on the 31st falls on the last day of
/// shorter months and returns to the 31st after.
pub fn due_dates(plan: &Recurring, today: Date) -> Vec<Date> {
    let last = plan.ends_on.map_or(today, |end| end.min(today));
    let mut dates = Vec::new();
    for n in 0.. {
        let span = match plan.every {
            Every::Monthly => n.months(),
            Every::Yearly => n.years(),
        };
        let Ok(due) = plan.starts_on.checked_add(span) else {
            break;
        };
        if due > last {
            break;
        }
        dates.push(due);
    }
    dates
}

/// Records the donations that have fallen due and are not recorded yet.
/// Returns how many were recorded.
pub async fn record_due(store: &Store, fx: &Fx, today: Date) -> Result<usize, StoreError> {
    let now = Timestamp::now();
    let mut recorded = 0;
    for plan in store.list_recurring().await? {
        let done = store.recorded_due_dates(plan.id).await?;
        let by = plan.created_by.clone();
        let missing = due_dates(&plan, today)
            .into_iter()
            .filter(|d| !done.contains(d))
            .take(MAX_PER_RUN);
        for due in missing {
            // Like a donation entered by hand: if no rate is known the
            // donation is recorded anyway and converted later.
            let conversion = match fx
                .convert(store, plan.amount_minor, &plan.currency, due)
                .await
            {
                Ok(c) => Some(c),
                Err(e @ (FxError::RateLimited | FxError::Http(_))) => {
                    warn!(error = %e, plan = plan.id, "no exchange rate for a recurring donation yet");
                    None
                }
                Err(_) => None,
            };
            let new = NewDonation {
                project: plan.project.clone(),
                platform: plan.platform.clone(),
                amount_minor: plan.amount_minor,
                currency: plan.currency.clone(),
                donated_on: due,
                kind: DonationKind::Recurring,
                note: plan.note.clone(),
                conversion,
            };
            if store
                .record_planned_donation(plan.id, due, &new, &by, now)
                .await?
                .is_some()
            {
                recorded += 1;
            }
        }
    }
    Ok(recorded)
}

/// Records what is due every `every`, starting shortly after the server starts.
pub fn spawn(store: Arc<Store>, fx: Arc<Fx>, every: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            let today = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
            match record_due(&store, &fx, today).await {
                Ok(0) => {}
                Ok(n) => info!(recorded = n, "recorded recurring donations"),
                Err(e) => warn!(error = %e, "recording recurring donations failed"),
            }
            tokio::time::sleep(every).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Actor;
    use jiff::civil::date;

    fn plan(every: Every, starts_on: Date, ends_on: Option<Date>) -> Recurring {
        Recurring {
            id: 1,
            project: "p".into(),
            project_repo: "github.com/o/p".into(),
            platform: "github-sponsors".into(),
            amount_minor: 500,
            currency: "EUR".into(),
            every,
            starts_on,
            ends_on,
            note: String::new(),
            created_by: Actor::new("u1", "alice"),
            recorded: 0,
        }
    }

    #[test]
    fn monthly_dates_run_up_to_today() {
        let p = plan(Every::Monthly, date(2026, 1, 15), None);
        assert_eq!(
            due_dates(&p, date(2026, 4, 15)),
            vec![
                date(2026, 1, 15),
                date(2026, 2, 15),
                date(2026, 3, 15),
                date(2026, 4, 15)
            ]
        );
        assert_eq!(due_dates(&p, date(2026, 4, 14)).len(), 3);
        assert!(due_dates(&p, date(2026, 1, 14)).is_empty());
    }

    #[test]
    fn the_end_of_the_month_is_kept() {
        let p = plan(Every::Monthly, date(2026, 1, 31), None);
        assert_eq!(
            due_dates(&p, date(2026, 4, 30)),
            vec![
                date(2026, 1, 31),
                date(2026, 2, 28),
                date(2026, 3, 31),
                date(2026, 4, 30)
            ]
        );
    }

    #[test]
    fn yearly_and_ended_plans() {
        let yearly = plan(Every::Yearly, date(2024, 2, 29), None);
        assert_eq!(
            due_dates(&yearly, date(2026, 12, 31)),
            vec![date(2024, 2, 29), date(2025, 2, 28), date(2026, 2, 28)]
        );
        let ended = plan(Every::Monthly, date(2026, 1, 1), Some(date(2026, 2, 10)));
        assert_eq!(
            due_dates(&ended, date(2026, 12, 31)),
            vec![date(2026, 1, 1), date(2026, 2, 1)]
        );
    }

    async fn setup() -> (Arc<Store>, Arc<Fx>) {
        let store = Arc::new(Store::memory().await.unwrap());
        crate::demo::seed(&store, "2026-10-01T00:00:00Z".parse().unwrap())
            .await
            .unwrap();
        let fx = Fx::new(None, "EUR".into(), Default::default());
        (store, fx)
    }

    fn new_plan(starts_on: Date, currency: &str) -> crate::model::NewRecurring {
        crate::model::NewRecurring {
            project: "tokio-rs-tokio".into(),
            platform: "github-sponsors".into(),
            amount_minor: 500,
            currency: currency.into(),
            every: Every::Monthly,
            starts_on,
            note: "sponsorship".into(),
        }
    }

    #[tokio::test]
    async fn due_donations_are_recorded_once_and_stop_when_the_plan_ends() {
        let (store, fx) = setup().await;
        let before = store.list_donations(None).await.unwrap().len();
        let alice = Actor::new("u1", "alice");
        let id = store
            .create_recurring(
                &new_plan(date(2026, 7, 10), "EUR"),
                &alice,
                Timestamp::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            record_due(&store, &fx, date(2026, 10, 17)).await.unwrap(),
            4
        );
        // Running again records nothing new.
        assert_eq!(
            record_due(&store, &fx, date(2026, 10, 17)).await.unwrap(),
            0
        );
        let all = store.list_donations(Some("tokio-rs-tokio")).await.unwrap();
        let mine: Vec<_> = all.iter().filter(|d| d.note == "sponsorship").collect();
        assert_eq!(mine.len(), 4);
        assert_eq!(store.list_donations(None).await.unwrap().len(), before + 4);
        assert!(mine.iter().all(|d| d.kind == DonationKind::Recurring));
        assert!(
            mine.iter().all(|d| d.conversion.is_some()),
            "EUR needs no rate"
        );
        assert_eq!(mine[0].donated_on, date(2026, 10, 10));
        assert_eq!(mine[0].recorded_by, alice);

        // A month later the next one is recorded; after ending, none are.
        assert_eq!(
            record_due(&store, &fx, date(2026, 11, 10)).await.unwrap(),
            1
        );
        store.end_recurring(id, date(2026, 11, 20)).await.unwrap();
        assert_eq!(record_due(&store, &fx, date(2027, 6, 1)).await.unwrap(), 0);
        let plans = store.list_recurring().await.unwrap();
        assert_eq!(plans[0].recorded, 5);
        assert_eq!(plans[0].ends_on, Some(date(2026, 11, 20)));
        // It can only be ended once.
        assert!(matches!(
            store.end_recurring(id, date(2026, 12, 1)).await,
            Err(StoreError::InUse(_))
        ));
        assert!(matches!(
            store.end_recurring(999, date(2026, 12, 1)).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_donation_in_another_currency_waits_for_its_rate_and_a_run_is_capped() {
        let (store, fx) = setup().await;
        let alice = Actor::new("u1", "alice");
        store
            .create_recurring(&new_plan(date(2020, 1, 1), "USD"), &alice, Timestamp::now())
            .await
            .unwrap();
        // Rates are off in this test, so each is recorded pending; and a plan
        // that began years ago catches up in steps.
        assert_eq!(
            record_due(&store, &fx, date(2026, 10, 17)).await.unwrap(),
            MAX_PER_RUN
        );
        let pending = store.list_donations(None).await.unwrap();
        assert_eq!(
            pending
                .iter()
                .filter(|d| d.note == "sponsorship" && d.conversion.is_none())
                .count(),
            MAX_PER_RUN
        );
        assert!(record_due(&store, &fx, date(2026, 10, 17)).await.unwrap() > 0);
    }

    #[tokio::test]
    async fn a_recorded_donation_cannot_be_moved_to_another_plan() {
        let (store, fx) = setup().await;
        let alice = Actor::new("u1", "alice");
        store
            .create_recurring(
                &new_plan(date(2026, 10, 1), "EUR"),
                &alice,
                Timestamp::now(),
            )
            .await
            .unwrap();
        record_due(&store, &fx, date(2026, 10, 17)).await.unwrap();
        let attempt =
            sqlx::query("UPDATE donation SET recurring_id = NULL WHERE recurring_id IS NOT NULL")
                .execute(store.pool())
                .await;
        assert!(attempt.is_err(), "donations are append-only");
    }
}
