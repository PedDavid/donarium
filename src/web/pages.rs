//! Page handlers.

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use super::auth::{AuthMode, Session, User};
use super::views::ProjectRow;
use super::{AppError, AppState};

/// Data shared by every full page.
pub struct Layout {
    pub title: String,
    pub user: String,
    pub admin: bool,
    pub csrf: String,
    pub asset_version: String,
    pub dark: bool,
    pub palette: String,
    pub dev_auth: bool,
    /// Which navigation entry to highlight.
    pub section: &'static str,
}

pub const PALETTES: [&str; 5] = ["neutral", "blue", "green", "orange", "violet"];

impl Layout {
    pub(super) fn new(
        state: &AppState,
        session: &Session,
        jar: &CookieJar,
        title: impl Into<String>,
    ) -> Layout {
        let palette = jar
            .get("donarium_palette")
            .map(|c| c.value().to_string())
            .filter(|p| PALETTES.contains(&p.as_str()))
            .unwrap_or_else(|| "neutral".into());
        Layout {
            title: title.into(),
            user: session.name.clone(),
            admin: session.admin,
            csrf: session.csrf.clone(),
            asset_version: state.inner.asset_version.clone(),
            dark: jar
                .get("donarium_theme")
                .is_some_and(|c| c.value() == "dark"),
            palette,
            dev_auth: matches!(state.inner.auth, AuthMode::InsecureDev { .. }),
            section: "projects",
        }
    }

    pub fn section(mut self, section: &'static str) -> Layout {
        self.section = section;
        self
    }
}

/// What a handler needs to render a page.
pub(super) struct Ctx<'a> {
    pub state: &'a AppState,
    pub session: &'a Session,
    pub jar: &'a CookieJar,
}

/// What a page says about the request that led to it.
pub(super) struct Feedback {
    pub error: String,
    pub notice: &'static str,
    pub status: StatusCode,
}

impl Feedback {
    pub fn ok(notice: &'static str) -> Feedback {
        Feedback {
            error: String::new(),
            notice,
            status: StatusCode::OK,
        }
    }

    pub fn rejected(error: impl Into<String>, status: StatusCode) -> Feedback {
        Feedback {
            error: error.into(),
            notice: "",
            status,
        }
    }
}

#[derive(Deserialize, Default, Clone)]
pub struct Filters {
    #[serde(default)]
    pub q: String,
}

pub(super) fn is_htmx(headers: &HeaderMap, target: &str) -> bool {
    headers.contains_key("hx-request")
        && headers
            .get("hx-target")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t == target)
}

pub(super) async fn rows(state: &AppState, q: &str) -> Result<(Vec<ProjectRow>, usize), AppError> {
    let now = state.now();
    let all: Vec<ProjectRow> = state
        .inner
        .store
        .list_projects()
        .await?
        .iter()
        .map(|p| ProjectRow::new(p, now))
        .collect();
    let total = all.len();
    let rows = all
        .into_iter()
        .filter(|r| q.is_empty() || r.matches(q))
        .collect();
    Ok((rows, total))
}

#[derive(Template)]
#[template(path = "partials/search_results.html")]
struct SearchResults {
    rows: Vec<ProjectRow>,
}

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// Results for the ⌘K command palette.
pub async fn search(
    State(state): State<AppState>,
    User(_): User,
    Query(query): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let (mut rows, _) = rows(&state, &query.q).await?;
    rows.truncate(8);
    Ok(Html(SearchResults { rows }.render()?).into_response())
}
