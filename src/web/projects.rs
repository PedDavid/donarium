//! Projects: the open-source projects you depend on or want to support. Scans
//! discover them; admins can also add one by hand, set how much it counts,
//! ignore it, and say how to donate to it.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use tracing::info;

use super::auth::{Admin, User};
use super::pages::{Ctx, Feedback, Filters, Layout, is_htmx, rows};
use super::views::{DonationRow, FundingLink, ProjectRow, fmt_money, fmt_weight};
use super::{AppError, AppState};
use crate::funding;
use crate::model::ProjectUpdate;
use crate::scan::resolve::parse_project_repo;
use crate::store::StoreError;
use crate::validation::parse_usage;

/// The only messages a `?notice=` can show: nothing from the URL is echoed.
fn notice(code: &str) -> &'static str {
    match code {
        "created" => "Project added.",
        "saved" => "Changes saved.",
        "deleted" => "Project deleted.",
        "funding-added" => "Way to donate added.",
        "funding-removed" => "Way to donate removed.",
        "funding-found" => "Looked at the project's FUNDING.yml: its ways to donate are listed.",
        "funding-none" => "The project declares no ways to donate in a FUNDING.yml.",
        _ => "",
    }
}

#[derive(Deserialize, Default)]
pub struct IndexQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    notice: String,
}

/// What an admin typed into the "add a project" form.
#[derive(Default, Clone)]
pub struct AddValues {
    pub repo: String,
    pub usage: String,
    pub notes: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    layout: Layout,
    rows: Vec<ProjectRow>,
    ignored: Vec<ProjectRow>,
    total: usize,
    filters: Filters,
    notice: &'static str,
    error: String,
    form: AddValues,
}

#[derive(Template)]
#[template(path = "partials/projects_table.html")]
struct ProjectsTable {
    rows: Vec<ProjectRow>,
    total: usize,
}

pub async fn index(
    State(state): State<AppState>,
    User(session): User,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let filters = Filters { q: q.q };
    if is_htmx(&headers, "projects-table") {
        let (rows, total) = active(&state, &filters.q).await?;
        return Ok(Html(ProjectsTable { rows, total }.render()?).into_response());
    }
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    render_index(
        &ctx,
        filters,
        AddValues {
            usage: "1".into(),
            ..Default::default()
        },
        Feedback::ok(notice(&q.notice)),
    )
    .await
}

/// Projects that take part in the ranking, and how many projects there are.
async fn active(state: &AppState, q: &str) -> Result<(Vec<ProjectRow>, usize), AppError> {
    let (all, total) = rows(state, q).await?;
    Ok((all.into_iter().filter(|r| !r.ignored).collect(), total))
}

async fn render_index(
    ctx: &Ctx<'_>,
    filters: Filters,
    form: AddValues,
    feedback: Feedback,
) -> Result<Response, AppError> {
    let (all, total) = rows(ctx.state, &filters.q).await?;
    let (ignored, rows): (Vec<_>, Vec<_>) = all.into_iter().partition(|r| r.ignored);
    let page = IndexPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, "Projects"),
        rows,
        ignored,
        total,
        filters,
        notice: feedback.notice,
        error: feedback.error,
        form,
    };
    Ok((feedback.status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct AddForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    usage: String,
    #[serde(default)]
    notes: String,
}

/// Free text an admin writes: bounded, and no control characters but newlines.
fn clean_notes(notes: &str) -> Result<String, String> {
    let notes = notes.trim();
    if notes.chars().count() > 1000 {
        return Err("the notes are too long (at most 1000 characters)".into());
    }
    if notes
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r')
    {
        return Err("the notes contain characters that are not allowed".into());
    }
    Ok(notes.replace("\r\n", "\n"))
}

pub async fn create(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let values = AddValues {
        repo: form.repo.clone(),
        usage: form.usage.clone(),
        notes: form.notes.clone(),
    };
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let reject =
        |error: String, status: StatusCode| (values.clone(), Feedback::rejected(error, status));
    let parsed = parse_project_repo(&form.repo)
        .ok_or_else(|| {
            "enter the project as host/owner/name, e.g. github.com/tokio-rs/tokio".to_string()
        })
        .and_then(|repo| {
            let usage = parse_usage(&form.usage).map_err(|e| e.to_string())?;
            let notes = clean_notes(&form.notes)?;
            Ok((repo, usage, notes))
        });
    let (repo, usage, notes) = match parsed {
        Ok(p) => p,
        Err(error) => {
            let (v, f) = reject(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render_index(&ctx, Filters::default(), v, f).await;
        }
    };
    match state
        .inner
        .store
        .create_project(
            &repo,
            usage,
            &notes,
            &session.actor(),
            jiff::Timestamp::now(),
        )
        .await
    {
        Ok(project) => {
            info!(actor = %session.sub, project = %project.name, %repo, "project added");
            Ok(Redirect::to(&format!("/projects/{}?notice=created", project.name)).into_response())
        }
        Err(StoreError::Duplicate) => {
            let (v, f) = reject(format!("{repo} is already a project"), StatusCode::CONFLICT);
            render_index(&ctx, Filters::default(), v, f).await
        }
        Err(e) => Err(e.into()),
    }
}

/// An admin's settings for a project, as shown in its form.
#[derive(Default, Clone)]
pub struct SettingsValues {
    pub usage: String,
    pub ignore: bool,
    pub notes: String,
}

/// What an admin typed into the "add a way to donate" form.
#[derive(Default, Clone)]
pub struct FundingValues {
    pub platform: String,
    pub handle: String,
}

#[derive(Template)]
#[template(path = "project_detail.html")]
struct DetailPage {
    layout: Layout,
    project: ProjectRow,
    links: Vec<FundingLink>,
    /// The project's FUNDING.yml can be looked up (a GitHub project, and GitHub is reachable).
    can_discover: bool,
    platforms: Vec<(&'static str, &'static str, bool)>,
    notice: &'static str,
    error: String,
    settings: SettingsValues,
    funding: FundingValues,
    donations: Vec<DonationRow>,
    /// Always false here: the project is the page.
    show_project: bool,
    /// Keeps the donations table's void buttons on this project afterwards.
    filter: String,
    /// What this project's donations add up to, in the base currency.
    donated: String,
}

pub async fn detail(
    State(state): State<AppState>,
    User(session): User,
    jar: CookieJar,
    Path(name): Path<String>,
    Query(q): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    render_detail(&ctx, &name, None, None, Feedback::ok(notice(&q.notice))).await
}

async fn render_detail(
    ctx: &Ctx<'_>,
    name: &str,
    settings: Option<SettingsValues>,
    funding_values: Option<FundingValues>,
    feedback: Feedback,
) -> Result<Response, AppError> {
    let project = ctx
        .state
        .inner
        .store
        .get_project(name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no project named {name:?}")))?;
    let settings = settings.unwrap_or_else(|| SettingsValues {
        usage: fmt_weight(project.manual_usage),
        ignore: project.ignored,
        notes: project.notes.clone(),
    });
    let funding_values = funding_values.unwrap_or_else(|| FundingValues {
        platform: funding::PLATFORMS[0].0.into(),
        handle: String::new(),
    });
    let platforms = funding::PLATFORMS
        .iter()
        .map(|(id, label)| (*id, *label, *id == funding_values.platform))
        .collect();
    let base = &ctx.state.inner.cfg.base_currency;
    let donations = ctx
        .state
        .inner
        .store
        .list_donations(Some(&project.name))
        .await?
        .iter()
        .map(|d| DonationRow::new(d, base))
        .collect();
    let donated = fmt_money(project.donated_base_minor, base);
    let page = DetailPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, project.name.clone()),
        donations,
        show_project: false,
        filter: project.name.clone(),
        donated,
        links: project.funding.iter().map(FundingLink::new).collect(),
        can_discover: ctx.state.inner.scanner.can_find_funding()
            && project.repo.starts_with("github.com/"),
        project: ProjectRow::new(&project, ctx.state.now()),
        platforms,
        notice: feedback.notice,
        error: feedback.error,
        settings,
        funding: funding_values,
    };
    Ok((feedback.status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    usage: String,
    /// A checkbox: present (`on`) when ticked.
    ignore: Option<String>,
    #[serde(default)]
    notes: String,
}

pub async fn update(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let ignore = form.ignore.is_some();
    let parsed = parse_usage(&form.usage)
        .map_err(|e| e.to_string())
        .and_then(|usage| Ok((usage, clean_notes(&form.notes)?)));
    let (usage, notes) = match parsed {
        Ok(p) => p,
        Err(error) => {
            let values = SettingsValues {
                usage: form.usage.clone(),
                ignore,
                notes: form.notes.clone(),
            };
            let rejected = Feedback::rejected(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render_detail(&ctx, &name, Some(values), None, rejected).await;
        }
    };
    state
        .inner
        .store
        .update_project(
            &name,
            &ProjectUpdate {
                manual_usage: usage,
                ignore,
                notes,
            },
            &session.actor(),
            jiff::Timestamp::now(),
        )
        .await?;
    info!(actor = %session.sub, project = %name, usage, ignore, "project settings changed");
    Ok(Redirect::to(&format!("/projects/{name}?notice=saved")).into_response())
}

#[derive(Deserialize)]
pub struct FundingForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    handle: String,
}

pub async fn add_funding(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<FundingForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let values = FundingValues {
        platform: form.platform.clone(),
        handle: form.handle.clone(),
    };
    let (handle, url) = match funding::parse(&form.platform, &form.handle) {
        Ok(p) => p,
        Err(e) => {
            let rejected = Feedback::rejected(e.to_string(), StatusCode::UNPROCESSABLE_ENTITY);
            return render_detail(&ctx, &name, None, Some(values), rejected).await;
        }
    };
    match state
        .inner
        .store
        .add_funding(&name, &form.platform, &handle, url.as_deref())
        .await
    {
        Ok(()) => {
            info!(actor = %session.sub, project = %name, platform = %form.platform, "way to donate added");
            Ok(Redirect::to(&format!("/projects/{name}?notice=funding-added")).into_response())
        }
        Err(StoreError::Duplicate) => {
            let rejected = Feedback::rejected("that is already listed", StatusCode::CONFLICT);
            render_detail(&ctx, &name, None, Some(values), rejected).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn discover_funding(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let project = state
        .inner
        .store
        .get_project(&name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no project named {name:?}")))?;
    match state
        .inner
        .scanner
        .discover_funding(&name, &project.repo)
        .await
    {
        Ok(n) => {
            info!(actor = %session.sub, project = %name, found = n, "funding looked up");
            let code = if n > 0 {
                "funding-found"
            } else {
                "funding-none"
            };
            Ok(Redirect::to(&format!("/projects/{name}?notice={code}")).into_response())
        }
        Err(e) => {
            let rejected = Feedback::rejected(e.to_string(), StatusCode::BAD_GATEWAY);
            render_detail(&ctx, &name, None, None, rejected).await
        }
    }
}

pub async fn remove_funding(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<FundingForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    state
        .inner
        .store
        .remove_funding(&name, &form.platform, &form.handle)
        .await?;
    info!(actor = %session.sub, project = %name, platform = %form.platform, "way to donate removed");
    Ok(Redirect::to(&format!("/projects/{name}?notice=funding-removed")).into_response())
}

#[derive(Deserialize, Default)]
pub struct CsrfOnly {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    match state.inner.store.delete_project(&name).await {
        Ok(()) => {
            info!(actor = %session.sub, project = %name, "project deleted");
            Ok(Redirect::to("/?notice=deleted").into_response())
        }
        Err(StoreError::InUse(why)) => {
            let rejected = Feedback::rejected(why, StatusCode::CONFLICT);
            render_detail(&ctx, &name, None, None, rejected).await
        }
        Err(e) => Err(e.into()),
    }
}
