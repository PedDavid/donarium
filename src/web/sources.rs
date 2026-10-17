//! Sources: the repositories that are scanned for dependencies. Everyone who
//! is logged in can see them; admins can add, change and remove them.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use tracing::info;

use super::auth::{Admin, Session, User};
use super::pages::Layout;
use super::views::{SourceRow, fmt_weight};
use super::{AppError, AppState};
use crate::model::{ForgeHosts, NewSource, Provider, ScanMethod, SourceUpdate};
use crate::scan::manifests::Skipped;
use crate::scan::resolve::parse_project_repo;
use crate::scan::usage::How;
use crate::store::{ImageReport, StoreError};
use crate::validation::{parse_repo, parse_weight};

/// Values of the add/edit forms, kept so a rejected submission is not lost.
#[derive(Default, Clone)]
pub struct FormValues {
    pub provider: String,
    pub repo: String,
    pub weight: String,
    pub scan_method: String,
    pub enabled: bool,
}

impl FormValues {
    fn blank() -> FormValues {
        FormValues {
            provider: Provider::Github.as_str().into(),
            weight: "1".into(),
            scan_method: ScanMethod::Auto.as_str().into(),
            enabled: true,
            ..Default::default()
        }
    }
}

pub struct SelectOption {
    pub value: &'static str,
    pub selected: bool,
}

/// The providers that can be used on this server (Gitea needs `--gitea-url`).
fn provider_options(hosts: &ForgeHosts, selected: &str) -> Vec<SelectOption> {
    hosts
        .available()
        .iter()
        .map(|p| SelectOption {
            value: p.as_str(),
            selected: p.as_str() == selected,
        })
        .collect()
}

fn method_options(selected: &str) -> Vec<SelectOption> {
    ScanMethod::ALL
        .iter()
        .map(|m| SelectOption {
            value: m.as_str(),
            selected: m.as_str() == selected,
        })
        .collect()
}

/// What a handler needs to render a page.
struct Ctx<'a> {
    state: &'a AppState,
    session: &'a Session,
    jar: &'a CookieJar,
}

/// What a page says about the request that led to it.
struct Feedback {
    error: String,
    notice: &'static str,
    status: StatusCode,
}

impl Feedback {
    fn ok(notice: &'static str) -> Feedback {
        Feedback {
            error: String::new(),
            notice,
            status: StatusCode::OK,
        }
    }

    fn rejected(error: impl Into<String>, status: StatusCode) -> Feedback {
        Feedback {
            error: error.into(),
            notice: "",
            status,
        }
    }
}

/// The only messages a `?notice=` can show: nothing from the URL is echoed.
fn notice(code: &str) -> &'static str {
    match code {
        "added" => "Source added.",
        "saved" => "Changes saved.",
        "deleted" => "Source deleted.",
        "scanning" => "Scan started. Reload in a moment to see the result.",
        "mapped" => "Image mapped. Usage was updated.",
        "unmapped" => "Mapping removed. Usage was updated.",
        _ => "",
    }
}

#[derive(Deserialize, Default)]
pub struct NoticeQuery {
    #[serde(default)]
    notice: String,
}

#[derive(Template)]
#[template(path = "sources.html")]
struct SourcesPage {
    layout: Layout,
    rows: Vec<SourceRow>,
    notice: &'static str,
    form: FormValues,
    providers: Vec<SelectOption>,
    methods: Vec<SelectOption>,
    error: String,
}

pub async fn index(
    State(state): State<AppState>,
    User(session): User,
    jar: CookieJar,
    Query(q): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    render_index(&ctx, FormValues::blank(), Feedback::ok(notice(&q.notice))).await
}

async fn render_index(
    ctx: &Ctx<'_>,
    form: FormValues,
    feedback: Feedback,
) -> Result<Response, AppError> {
    let now = ctx.state.now();
    let hosts = ctx.state.inner.cfg.forge_hosts();
    let rows = ctx
        .state
        .inner
        .store
        .list_sources()
        .await?
        .iter()
        .map(|s| SourceRow::new(s, now, &hosts))
        .collect();
    let page = SourcesPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, "Sources").section("sources"),
        rows,
        notice: feedback.notice,
        providers: provider_options(&ctx.state.inner.cfg.forge_hosts(), &form.provider),
        methods: method_options(&form.scan_method),
        form,
        error: feedback.error,
    };
    Ok((feedback.status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct AddForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    weight: String,
    #[serde(default)]
    scan_method: String,
}

/// Turns the submitted fields into a [`NewSource`], or says what is wrong.
fn parse_new(form: &AddForm, hosts: &ForgeHosts) -> Result<NewSource, String> {
    let provider = Provider::parse(&form.provider).ok_or("choose a provider")?;
    let host = hosts.host(provider).ok_or(match provider {
        Provider::Gitea => "no Gitea or Forgejo server is configured on this server (--gitea-url)",
        _ => "that provider is not available",
    })?;
    let scan_method = ScanMethod::parse(&form.scan_method).ok_or("choose a scan method")?;
    let repo = parse_repo(provider, &host, &form.repo).map_err(|e| e.to_string())?;
    let weight = parse_weight(&form.weight).map_err(|e| e.to_string())?;
    Ok(NewSource {
        provider,
        repo,
        weight,
        scan_method,
    })
}

pub async fn create(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let values = FormValues {
        provider: form.provider.clone(),
        repo: form.repo.clone(),
        weight: form.weight.clone(),
        scan_method: form.scan_method.clone(),
        enabled: true,
    };
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let new = match parse_new(&form, &state.inner.cfg.forge_hosts()) {
        Ok(new) => new,
        Err(error) => {
            let rejected = Feedback::rejected(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render_index(&ctx, values, rejected).await;
        }
    };
    match state
        .inner
        .store
        .create_source(&new, &session.actor(), jiff::Timestamp::now())
        .await
    {
        Ok(source) => {
            info!(actor = %session.sub, source = source.id, repo = %source.repo, provider = source.provider.as_str(), "source added");
            Ok(Redirect::to("/sources?notice=added").into_response())
        }
        Err(StoreError::Duplicate) => {
            let rejected = Feedback::rejected(
                format!("{} is already a source", new.repo),
                StatusCode::CONFLICT,
            );
            render_index(&ctx, values, rejected).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "source_detail.html")]
struct DetailPage {
    layout: Layout,
    source: SourceRow,
    notice: &'static str,
    form: FormValues,
    methods: Vec<SelectOption>,
    error: String,
    /// Why this source cannot be scanned, if it cannot.
    scan_problem: String,
    scanning: bool,
    images: Vec<ImageRow>,
    skipped: Vec<Skipped>,
}

/// An image found in a source, as the page shows it.
pub struct ImageRow {
    pub registry: String,
    pub repository: String,
    pub tags: String,
    pub file: String,
    pub uses: i64,
    pub project: String,
    /// "mapped by an admin", "from the image's source label", …
    pub how: &'static str,
    pub mapped: bool,
    pub note: String,
}

impl From<ImageReport> for ImageRow {
    fn from(r: ImageReport) -> ImageRow {
        ImageRow {
            registry: r.registry,
            repository: r.repository,
            tags: r.tags,
            file: r.file,
            uses: r.uses,
            project: r.resolved.project.unwrap_or_default(),
            how: r.resolved.how.map(How::as_str).unwrap_or(""),
            mapped: r.resolved.how == Some(How::Mapping),
            note: r.resolved.note.unwrap_or_default(),
        }
    }
}

pub async fn detail(
    State(state): State<AppState>,
    User(session): User,
    jar: CookieJar,
    Path(id): Path<i64>,
    Query(q): Query<NoticeQuery>,
) -> Result<Response, AppError> {
    let source = load(&state, id).await?;
    let form = FormValues {
        provider: source.provider.as_str().into(),
        repo: source.repo.clone(),
        weight: fmt_weight(source.weight),
        scan_method: source.scan_method.as_str().into(),
        enabled: source.enabled,
    };
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    render_detail(&ctx, &source, form, Feedback::ok(notice(&q.notice))).await
}

async fn load(state: &AppState, id: i64) -> Result<crate::model::Source, AppError> {
    state
        .inner
        .store
        .get_source(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no source with id {id}")))
}

async fn render_detail(
    ctx: &Ctx<'_>,
    source: &crate::model::Source,
    form: FormValues,
    feedback: Feedback,
) -> Result<Response, AppError> {
    let store = &ctx.state.inner.store;
    let images = store
        .image_report(source.id)
        .await?
        .into_iter()
        .map(ImageRow::from)
        .collect();
    let page = DetailPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, source.repo.clone())
            .section("sources"),
        source: SourceRow::new(source, ctx.state.now(), &ctx.state.inner.cfg.forge_hosts()),
        notice: feedback.notice,
        methods: method_options(&form.scan_method),
        form,
        error: feedback.error,
        scan_problem: ctx
            .state
            .inner
            .scanner
            .can_scan(source)
            .err()
            .unwrap_or_default(),
        scanning: ctx.state.inner.scanner.is_running(source.id),
        images,
        skipped: store.skipped_images(source.id).await?,
    };
    Ok((feedback.status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct EditForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    weight: String,
    #[serde(default)]
    scan_method: String,
    /// A checkbox: present (`on`) when ticked, absent when not.
    enabled: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<EditForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let source = load(&state, id).await?;
    let values = FormValues {
        provider: source.provider.as_str().into(),
        repo: source.repo.clone(),
        weight: form.weight.clone(),
        scan_method: form.scan_method.clone(),
        enabled: form.enabled.is_some(),
    };
    let parsed = parse_weight(&form.weight)
        .map_err(|e| e.to_string())
        .and_then(|weight| {
            ScanMethod::parse(&form.scan_method)
                .ok_or_else(|| "choose a scan method".to_string())
                .map(|scan_method| SourceUpdate {
                    weight,
                    scan_method,
                    enabled: form.enabled.is_some(),
                })
        });
    let change = match parsed {
        Ok(change) => change,
        Err(error) => {
            let ctx = Ctx {
                state: &state,
                session: &session,
                jar: &jar,
            };
            let rejected = Feedback::rejected(error, StatusCode::UNPROCESSABLE_ENTITY);
            return render_detail(&ctx, &source, values, rejected).await;
        }
    };
    state
        .inner
        .store
        .update_source(id, &change, &session.actor(), jiff::Timestamp::now())
        .await?;
    info!(actor = %session.sub, source = id, repo = %source.repo, weight = change.weight, method = change.scan_method.as_str(), enabled = change.enabled, "source updated");
    Ok(Redirect::to(&format!("/sources/{id}?notice=saved")).into_response())
}

#[derive(Deserialize, Default)]
pub struct CsrfOnly {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let source = load(&state, id).await?;
    state.inner.store.delete_source(id).await?;
    // Projects only this source used are no longer used by anything.
    state
        .inner
        .store
        .rebuild_image_usage(jiff::Timestamp::now())
        .await?;
    info!(actor = %session.sub, source = id, repo = %source.repo, "source deleted");
    Ok(Redirect::to("/sources?notice=deleted").into_response())
}

/// Starts a scan in the background and returns at once: a scan can take
/// minutes, and its result is shown on the source's page when it is done.
pub async fn scan(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfOnly>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let source = load(&state, id).await?;
    if !source.enabled {
        return Err(AppError::BadRequest("the source is disabled".into()));
    }
    state
        .inner
        .scanner
        .can_scan(&source)
        .map_err(AppError::BadRequest)?;
    if state.inner.scanner.is_running(id) {
        return Err(AppError::BadRequest(
            "this source is already being scanned".into(),
        ));
    }
    info!(actor = %session.sub, source = id, repo = %source.repo, "scan requested");
    let scanner = state.inner.scanner.clone();
    tokio::spawn(async move {
        // The outcome is saved on the source and logged by the scanner.
        let _ = scanner.scan_source(id).await;
    });
    Ok(Redirect::to(&format!("/sources/{id}?notice=scanning")).into_response())
}

#[derive(Deserialize)]
pub struct MapForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    source_id: i64,
    registry: String,
    repository: String,
    #[serde(default)]
    project_repo: String,
}

/// An image name as it appears in the page: short, printable, and not empty.
fn plausible(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}

pub async fn map_image(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<MapForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    if !plausible(&form.registry) || !plausible(&form.repository) {
        return Err(AppError::BadRequest("invalid image".into()));
    }
    let source = load(&state, form.source_id).await?;
    let Some(project) = parse_project_repo(&form.project_repo) else {
        let ctx = Ctx {
            state: &state,
            session: &session,
            jar: &jar,
        };
        let values = FormValues {
            provider: source.provider.as_str().into(),
            repo: source.repo.clone(),
            weight: fmt_weight(source.weight),
            scan_method: source.scan_method.as_str().into(),
            enabled: source.enabled,
        };
        let rejected = Feedback::rejected(
            "enter the project as host/owner/name, e.g. github.com/qdm12/gluetun",
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        return render_detail(&ctx, &source, values, rejected).await;
    };
    let store = &state.inner.store;
    store
        .set_mapping(
            &form.registry,
            &form.repository,
            &project,
            &session.actor(),
            jiff::Timestamp::now(),
        )
        .await?;
    store.rebuild_image_usage(jiff::Timestamp::now()).await?;
    info!(actor = %session.sub, registry = %form.registry, image = %form.repository, %project, "image mapped");
    Ok(Redirect::to(&format!("/sources/{}?notice=mapped", form.source_id)).into_response())
}

#[derive(Deserialize)]
pub struct UnmapForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    source_id: i64,
    registry: String,
    repository: String,
}

pub async fn unmap_image(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Form(form): Form<UnmapForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let store = &state.inner.store;
    store
        .delete_mapping(&form.registry, &form.repository)
        .await?;
    store.rebuild_image_usage(jiff::Timestamp::now()).await?;
    info!(actor = %session.sub, registry = %form.registry, image = %form.repository, "image mapping removed");
    Ok(Redirect::to(&format!("/sources/{}?notice=unmapped", form.source_id)).into_response())
}
