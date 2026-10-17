//! Recurring donations: setting one up and ending it. The donations
//! themselves are recorded by [`crate::recurring`] as they fall due.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use jiff::civil::Date;
use serde::Deserialize;
use tracing::info;

use super::auth::Admin;
use super::donations::{blank_form, render_with_plan};
use super::pages::{Ctx, Feedback};
use super::{AppError, AppState};
use crate::funding;
use crate::model::{Every, NewRecurring};
use crate::money;
use crate::store::StoreError;

/// What an admin typed into the form (or its defaults).
#[derive(Default, Clone)]
pub struct PlanValues {
    pub project: String,
    pub platform: String,
    pub amount: String,
    pub currency: String,
    pub every: Every,
    pub starts_on: String,
    pub note: String,
}

impl PlanValues {
    pub fn blank(state: &AppState) -> PlanValues {
        PlanValues {
            platform: "github-sponsors".into(),
            currency: state.inner.cfg.base_currency.clone(),
            starts_on: state
                .now()
                .to_zoned(jiff::tz::TimeZone::UTC)
                .date()
                .to_string(),
            ..Default::default()
        }
    }
}

#[derive(Deserialize)]
pub struct PlanForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    project: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    every: String,
    #[serde(default)]
    starts_on: String,
    #[serde(default)]
    note: String,
}

impl PlanForm {
    fn values(&self) -> PlanValues {
        PlanValues {
            project: self.project.clone(),
            platform: self.platform.clone(),
            amount: self.amount.clone(),
            currency: self.currency.clone(),
            every: Every::parse(&self.every).unwrap_or_default(),
            starts_on: self.starts_on.clone(),
            note: self.note.clone(),
        }
    }
}

async fn parse(state: &AppState, form: &PlanForm) -> Result<NewRecurring, String> {
    if state
        .inner
        .store
        .get_project(&form.project)
        .await
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("choose a project".into());
    }
    if !funding::is_donation_platform(&form.platform) {
        return Err("choose where you donate".into());
    }
    let every = Every::parse(&form.every).ok_or("choose how often")?;
    let currency = money::parse_currency(&form.currency).map_err(|e| e.0)?;
    let amount_minor = money::parse_amount(&form.amount, &currency).map_err(|e| e.0)?;
    let starts_on: Date = form
        .starts_on
        .trim()
        .parse()
        .map_err(|_| "enter the first date as YYYY-MM-DD".to_string())?;
    let today = state.now().to_zoned(jiff::tz::TimeZone::UTC).date();
    if starts_on < Date::constant(2000, 1, 1) {
        return Err("the first date is too long ago".into());
    }
    if starts_on > today.saturating_add(jiff::ToSpan::days(366)) {
        return Err("the first date is more than a year away".into());
    }
    let note = form.note.trim().to_string();
    if note.chars().count() > 500 || note.chars().any(char::is_control) {
        return Err("the note is too long or has characters that are not allowed".into());
    }
    Ok(NewRecurring {
        project: form.project.clone(),
        platform: form.platform.clone(),
        amount_minor,
        currency,
        every,
        starts_on,
        note,
    })
}

pub async fn create(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<PlanForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    // Like recording a donation, this is what step-up protects.
    if !session.fresh_enough(state.inner.cfg.auth.step_up_max_age, jiff::Timestamp::now()) {
        return Ok(Redirect::to("/auth/login?reauth=true&next=%2Fdonations").into_response());
    }
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let new = match parse(&state, &form).await {
        Ok(new) => new,
        Err(error) => {
            let rejected = Feedback::rejected(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render_with_plan(&ctx, "", blank_form(&state, ""), form.values(), rejected)
                .await;
        }
    };
    let id = state
        .inner
        .store
        .create_recurring(&new, &session.actor(), jiff::Timestamp::now())
        .await?;
    info!(actor = %session.sub, plan = id, project = %new.project, "recurring donation set up");
    // What has already fallen due is recorded now, not at the next hourly run.
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    crate::recurring::record_due(&state.inner.store, &state.inner.fx, today).await?;
    Ok(Redirect::to("/donations?notice=recurring-created").into_response())
}

#[derive(Deserialize)]
pub struct EndForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
}

pub async fn end(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<EndForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    // Ending it today keeps today's donation if one fell due today.
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    match state.inner.store.end_recurring(id, today).await {
        Ok(()) => {
            info!(actor = %session.sub, plan = id, "recurring donation ended");
            Ok(Redirect::to("/donations?notice=recurring-ended").into_response())
        }
        Err(StoreError::InUse(why)) => {
            let rejected = Feedback::rejected(why, StatusCode::CONFLICT);
            render_with_plan(
                &ctx,
                "",
                blank_form(&state, ""),
                PlanValues::blank(&state),
                rejected,
            )
            .await
        }
        Err(e) => Err(e.into()),
    }
}
