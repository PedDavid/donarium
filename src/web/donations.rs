//! Donations: recording what you gave, correcting mistakes by voiding, and
//! the totals. Everyone who is logged in can read; admins can record and void.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use jiff::civil::Date;
use serde::Deserialize;
use tracing::info;

use super::auth::{Admin, User};
use super::pages::{Ctx, Feedback, Layout};
use super::views::{DonationRow, TotalsView};
use super::{AppError, AppState};
use crate::funding;
use crate::fx::FxError;
use crate::model::{Conversion, DonationKind, FxSource, NewDonation};
use crate::money;
use crate::store::StoreError;

/// The only messages a `?notice=` can show: nothing from the URL is echoed.
fn notice(code: &str) -> &'static str {
    match code {
        "recorded" => "Donation recorded.",
        "pending" => "Donation recorded. Its amount in the base currency is not known yet.",
        "pending-unsupported" => {
            "Donation recorded. The European Central Bank publishes no rate for this currency: enter one on the donation to include it in the totals."
        }
        "pending-later" => {
            "Donation recorded. The exchange rate service could not be reached, so its amount in the base currency will be filled in automatically later."
        }
        "pending-off" => {
            "Donation recorded. Exchange rates are turned off: enter a rate on the donation to include it in the totals."
        }
        "rated" => "Rate saved. The donation now counts in the totals.",
        "voided" => "Donation voided. It stays in the history, marked as voided.",
        _ => "",
    }
}

#[derive(Deserialize, Default)]
pub struct IndexQuery {
    /// Show one project's donations, and preselect it in the form.
    #[serde(default)]
    project: String,
    #[serde(default)]
    notice: String,
}

/// What an admin typed into the form.
#[derive(Default, Clone)]
pub struct FormValues {
    pub project: String,
    pub platform: String,
    pub amount: String,
    pub currency: String,
    pub date: String,
    pub kind: String,
    pub note: String,
    pub rate: String,
}

pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "donations.html")]
struct DonationsPage {
    layout: Layout,
    donations: Vec<DonationRow>,
    totals: TotalsView,
    /// The project the list is limited to, if any.
    filter: String,
    show_project: bool,
    base_currency: String,
    notice: &'static str,
    error: String,
    form: FormValues,
    projects: Vec<SelectOption>,
    platforms: Vec<SelectOption>,
    kinds: Vec<SelectOption>,
}

fn options<T: Into<String>>(
    items: impl IntoIterator<Item = (T, String)>,
    selected: &str,
) -> Vec<SelectOption> {
    items
        .into_iter()
        .map(|(value, label)| {
            let value = value.into();
            SelectOption {
                selected: value == selected,
                value,
                label,
            }
        })
        .collect()
}

#[derive(Deserialize, Default)]
pub struct ExportQuery {
    /// A year to limit the file to; empty for everything.
    #[serde(default)]
    year: String,
}

/// The donations that count as a CSV file, everything or one year.
pub async fn export(
    State(state): State<AppState>,
    User(_session): User,
    Query(q): Query<ExportQuery>,
) -> Result<Response, AppError> {
    let year = match q.year.trim() {
        "" => None,
        y => Some(
            y.parse::<i16>()
                .ok()
                .filter(|y| (1900..=9999).contains(y))
                .ok_or_else(|| AppError::BadRequest("the year must be like 2026".into()))?,
        ),
    };
    let donations = state.inner.store.list_donations(None).await?;
    let csv = crate::export::donations_csv(&donations, &state.inner.cfg.base_currency, year);
    let name = match year {
        Some(y) => format!("donarium-donations-{y}.csv"),
        None => "donarium-donations.csv".into(),
    };
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/csv; charset=utf-8".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        csv,
    )
        .into_response())
}

pub async fn index(
    State(state): State<AppState>,
    User(session): User,
    jar: CookieJar,
    Query(q): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let form = blank_form(&state, &q.project);
    render(&ctx, &q.project, form, Feedback::ok(notice(&q.notice))).await
}

fn blank_form(state: &AppState, project: &str) -> FormValues {
    FormValues {
        project: project.to_string(),
        platform: "github-sponsors".into(),
        currency: state.inner.cfg.base_currency.clone(),
        date: state
            .now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date()
            .to_string(),
        kind: DonationKind::OneOff.as_str().into(),
        ..Default::default()
    }
}

async fn render(
    ctx: &Ctx<'_>,
    project: &str,
    form: FormValues,
    feedback: Feedback,
) -> Result<Response, AppError> {
    let store = &ctx.state.inner.store;
    let base = ctx.state.inner.cfg.base_currency.clone();
    let filter = (!project.is_empty()).then_some(project);
    let donations = store
        .list_donations(filter)
        .await?
        .iter()
        .map(|d| DonationRow::new(d, &base))
        .collect();
    let projects = store
        .list_projects()
        .await?
        .into_iter()
        .map(|p| (p.name.clone(), format!("{} ({})", p.name, p.repo)))
        .collect::<Vec<_>>();
    let page = DonationsPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, "Donations").section("donations"),
        donations,
        totals: TotalsView::new(&store.donation_totals(filter).await?, &base),
        filter: project.to_string(),
        show_project: project.is_empty(),
        base_currency: base,
        notice: feedback.notice,
        error: feedback.error,
        projects: options(projects, &form.project),
        platforms: options(
            funding::donation_platforms()
                .into_iter()
                .map(|(id, name)| (id, name.to_string())),
            &form.platform,
        ),
        kinds: options(
            DonationKind::ALL
                .into_iter()
                .map(|k| (k.as_str(), k.label().to_string())),
            &form.kind,
        ),
        form,
    };
    Ok((feedback.status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct RecordForm {
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
    date: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    rate: String,
}

impl RecordForm {
    fn values(&self) -> FormValues {
        FormValues {
            project: self.project.clone(),
            platform: self.platform.clone(),
            amount: self.amount.clone(),
            currency: self.currency.clone(),
            date: self.date.clone(),
            kind: self.kind.clone(),
            note: self.note.clone(),
            rate: self.rate.clone(),
        }
    }
}

/// Turns the form into a donation, or says what is wrong with it.
/// Why a donation's amount in the base currency is not known, if it is not.
async fn parse(
    state: &AppState,
    form: &RecordForm,
) -> Result<(NewDonation, Option<&'static str>), String> {
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
        return Err("choose where you donated".into());
    }
    let kind = DonationKind::parse(&form.kind).ok_or("choose one-off or recurring")?;
    let currency = money::parse_currency(&form.currency).map_err(|e| e.0)?;
    let amount_minor = money::parse_amount(&form.amount, &currency).map_err(|e| e.0)?;
    let donated_on: Date = form
        .date
        .trim()
        .parse()
        .map_err(|_| "enter the date as YYYY-MM-DD".to_string())?;
    // A day of slack for time zones ahead of UTC.
    let today = state.now().to_zoned(jiff::tz::TimeZone::UTC).date();
    if donated_on > today.tomorrow().map_err(|e| e.to_string())? {
        return Err("the date is in the future".into());
    }
    if donated_on < Date::constant(2000, 1, 1) {
        return Err("the date is too long ago".into());
    }
    let note = form.note.trim().to_string();
    if note.chars().count() > 500 {
        return Err("the note is too long (at most 500 characters)".into());
    }
    if note.chars().any(char::is_control) {
        return Err("the note contains characters that are not allowed".into());
    }
    let base = &state.inner.cfg.base_currency;
    let mut pending = None;
    let conversion = if &currency == base {
        if !form.rate.trim().is_empty() {
            return Err(format!("no rate is needed: it is already in {base}"));
        }
        Some(Conversion {
            base_minor: amount_minor,
            rate: 1.0,
            source: FxSource::Same,
        })
    } else if form.rate.trim().is_empty() {
        // Ask the exchange rate service. If it cannot say, the donation is
        // recorded anyway and stays pending.
        match state
            .inner
            .fx
            .convert(&state.inner.store, amount_minor, &currency, donated_on)
            .await
        {
            Ok(conversion) => Some(conversion),
            Err(e) => {
                tracing::info!(error = %e, %currency, "recording a donation without its conversion");
                pending = Some(match e {
                    FxError::Unsupported(_) | FxError::Dust => "pending-unsupported",
                    FxError::Disabled => "pending-off",
                    _ => "pending-later",
                });
                None
            }
        }
    } else {
        let rate = money::parse_rate(&form.rate).map_err(|e| e.0)?;
        let base_minor = money::convert(amount_minor, &currency, rate, base)
            .ok_or("that amount converts to nothing in the base currency")?;
        Some(Conversion {
            base_minor,
            rate,
            source: FxSource::Manual,
        })
    };
    Ok((
        NewDonation {
            project: form.project.clone(),
            platform: form.platform.clone(),
            amount_minor,
            currency,
            donated_on,
            kind,
            note,
            conversion,
        },
        pending,
    ))
}

pub async fn record(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<RecordForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    // Recording is what step-up protects: a login that is too old goes back
    // through the identity provider first.
    if !session.fresh_enough(state.inner.cfg.auth.step_up_max_age, jiff::Timestamp::now()) {
        return Ok(Redirect::to("/auth/login?reauth=true&next=%2Fdonations").into_response());
    }
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let (new, pending) = match parse(&state, &form).await {
        Ok(parsed) => parsed,
        Err(error) => {
            let rejected = Feedback::rejected(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render(&ctx, &form.project, form.values(), rejected).await;
        }
    };
    let donation = state
        .inner
        .store
        .record_donation(&new, &session.actor(), jiff::Timestamp::now())
        .await?;
    info!(
        actor = %session.sub, donation = donation.id, project = %donation.project,
        amount = donation.amount_minor, currency = %donation.currency,
        converted = donation.conversion.is_some(), "donation recorded"
    );
    let code = match (donation.conversion.is_some(), pending) {
        (true, _) => "recorded",
        (false, Some(why)) => why,
        (false, None) => "pending",
    };
    Ok(Redirect::to(&format!(
        "/donations?notice={code}&project={}",
        url::form_urlencoded::byte_serialize(donation.project.as_bytes()).collect::<String>()
    ))
    .into_response())
}

#[derive(Deserialize, Default)]
pub struct VoidForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    /// Keep the list limited to this project afterwards.
    #[serde(default)]
    project: String,
}

pub async fn void(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<VoidForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    if !session.fresh_enough(state.inner.cfg.auth.step_up_max_age, jiff::Timestamp::now()) {
        return Ok(Redirect::to("/auth/login?reauth=true&next=%2Fdonations").into_response());
    }
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    match state
        .inner
        .store
        .void_donation(id, &session.actor(), jiff::Timestamp::now())
        .await
    {
        Ok(()) => {
            info!(actor = %session.sub, donation = id, "donation voided");
            let project =
                url::form_urlencoded::byte_serialize(form.project.as_bytes()).collect::<String>();
            Ok(
                Redirect::to(&format!("/donations?notice=voided&project={project}"))
                    .into_response(),
            )
        }
        Err(StoreError::InUse(why)) => {
            let rejected = Feedback::rejected(why, StatusCode::CONFLICT);
            render(
                &ctx,
                &form.project,
                blank_form(&state, &form.project),
                rejected,
            )
            .await
        }
        Err(e) => Err(e.into()),
    }
}

/// The donations page again, with what went wrong.
async fn reject(
    ctx: &Ctx<'_>,
    project: &str,
    error: String,
    status: StatusCode,
) -> Result<Response, AppError> {
    let form = blank_form(ctx.state, project);
    render(ctx, project, form, Feedback::rejected(error, status)).await
}

#[derive(Deserialize, Default)]
pub struct RateForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    rate: String,
    /// Keep the list limited to this project afterwards.
    #[serde(default)]
    project: String,
}

/// Sets the rate of a donation that is still waiting for one: for a currency
/// the ECB does not publish, or to record what the card was really charged.
/// A donation's conversion is never changed once it exists.
pub async fn set_rate(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RateForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    if !session.fresh_enough(state.inner.cfg.auth.step_up_max_age, jiff::Timestamp::now()) {
        return Ok(Redirect::to("/auth/login?reauth=true&next=%2Fdonations").into_response());
    }
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let donation = state
        .inner
        .store
        .get_donation(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no donation with id {id}")))?;
    if donation.conversion.is_some() {
        let why = "this donation already has its amount in the base currency";
        return reject(&ctx, &form.project, why.into(), StatusCode::CONFLICT).await;
    }
    let base = &state.inner.cfg.base_currency;
    let converted = money::parse_rate(&form.rate)
        .map_err(|e| e.0)
        .and_then(|rate| {
            money::convert(donation.amount_minor, &donation.currency, rate, base)
                .map(|base_minor| Conversion {
                    base_minor,
                    rate,
                    source: FxSource::Manual,
                })
                .ok_or_else(|| "that amount converts to nothing in the base currency".to_string())
        });
    let conversion = match converted {
        Ok(c) => c,
        Err(error) => {
            return reject(&ctx, &form.project, error, StatusCode::UNPROCESSABLE_ENTITY).await;
        }
    };
    match state.inner.store.set_conversion(id, &conversion).await {
        Ok(()) => {
            info!(actor = %session.sub, donation = id, rate = conversion.rate, "rate set by hand");
            let project =
                url::form_urlencoded::byte_serialize(form.project.as_bytes()).collect::<String>();
            Ok(Redirect::to(&format!("/donations?notice=rated&project={project}")).into_response())
        }
        Err(StoreError::InUse(why)) => reject(&ctx, &form.project, why, StatusCode::CONFLICT).await,
        Err(e) => Err(e.into()),
    }
}
