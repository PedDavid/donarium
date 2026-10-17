//! Linking donations to receipts that already live in Paperless-ngx. Admins
//! search it (suggestions come from the donation's date), pick a document, and
//! only its id is stored.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use tracing::info;

use super::auth::Admin;
use super::pages::{Ctx, Layout};
use super::views::{DonationRow, ReceiptLink};
use super::{AppError, AppState};
use crate::receipts::{Document, PaperlessError};
use crate::store::StoreError;

/// Fills in the receipts of `rows`, and whether more can be linked.
pub async fn attach(state: &AppState, rows: &mut [DonationRow]) -> Result<(), AppError> {
    let Some(paperless) = &state.inner.paperless else {
        return Ok(());
    };
    let receipts = state.inner.store.list_receipts().await?;
    for row in rows {
        row.can_link = true;
        row.receipts = receipts
            .iter()
            .filter(|r| r.donation_id == row.id)
            .map(|r| ReceiptLink {
                id: r.id,
                title: if r.title.is_empty() {
                    format!("document {}", r.reference)
                } else {
                    r.title.clone()
                },
                url: r
                    .reference
                    .parse()
                    .map(|id| paperless.document_url(id))
                    .unwrap_or_default(),
            })
            .collect();
    }
    Ok(())
}

struct Hit {
    id: u64,
    title: String,
    date: String,
}

impl From<Document> for Hit {
    fn from(d: Document) -> Hit {
        Hit {
            date: d.day().to_string(),
            id: d.id,
            title: if d.title.is_empty() {
                format!("document {}", d.id)
            } else {
                d.title
            },
        }
    }
}

#[derive(Template)]
#[template(path = "receipt.html")]
struct ReceiptPage {
    layout: Layout,
    id: i64,
    summary: String,
    query: String,
    /// Whether the hits are suggestions from the date, not a search.
    suggested: bool,
    hits: Vec<Hit>,
    error: String,
}

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

pub async fn search(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    Path(id): Path<i64>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let ctx = Ctx {
        state: &state,
        session: &session,
        jar: &jar,
    };
    let Some(paperless) = &state.inner.paperless else {
        return Err(AppError::NotFound("not found".into()));
    };
    let donation = state
        .inner
        .store
        .get_donation(id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such donation".into()))?;
    let base = &state.inner.cfg.base_currency;
    let row = DonationRow::new(&donation, base);
    let query = q.q.trim().to_string();
    // With no search text, suggest what is dated around the donation. With
    // some, search everything: the receipt may be dated differently.
    let suggested = query.is_empty();
    let found = paperless
        .search(&query, suggested.then_some(donation.donated_on))
        .await;
    let (hits, error, status) = match found {
        Ok(docs) => (
            docs.into_iter().map(Hit::from).collect(),
            String::new(),
            StatusCode::OK,
        ),
        Err(e) => (Vec::new(), e.to_string(), StatusCode::BAD_GATEWAY),
    };
    let page = ReceiptPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, "Find receipt").section("donations"),
        id,
        summary: format!("{} to {}, {}", row.amount, row.project, row.date),
        query,
        suggested,
        hits,
        error,
    };
    Ok((status, Html(page.render()?)).into_response())
}

#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    document: String,
}

pub async fn link(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<LinkForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    let Some(paperless) = &state.inner.paperless else {
        return Err(AppError::NotFound("not found".into()));
    };
    let Ok(document) = form.document.trim().parse::<u64>() else {
        return Ok(too_bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the document must be its number in Paperless",
        ));
    };
    // The document must exist; its title is kept so the link reads well even
    // if Paperless is down.
    let doc = match paperless.document(document).await {
        Ok(doc) => doc,
        Err(e @ PaperlessError::NoSuchDocument(_)) => {
            return Ok(too_bad(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()));
        }
        Err(e) => return Ok(too_bad(StatusCode::BAD_GATEWAY, &e.to_string())),
    };
    match state
        .inner
        .store
        .link_receipt(
            id,
            &doc.id.to_string(),
            &doc.title,
            &session.actor(),
            jiff::Timestamp::now(),
        )
        .await
    {
        Ok(()) => {
            info!(actor = %session.sub, donation = id, document = doc.id, "receipt linked");
            Ok(Redirect::to("/donations?notice=receipt-linked").into_response())
        }
        Err(StoreError::NotFound) => Err(AppError::NotFound("not found".into())),
        Err(StoreError::InUse(why)) => Ok(too_bad(StatusCode::CONFLICT, &why)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct UnlinkForm {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
    #[serde(default)]
    project: String,
}

pub async fn unlink(
    State(state): State<AppState>,
    Admin(session): Admin,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<UnlinkForm>,
) -> Result<Response, AppError> {
    session.check_csrf(&headers, form.csrf.as_deref())?;
    match state.inner.store.unlink_receipt(id).await {
        Ok(()) => {
            info!(actor = %session.sub, receipt = id, "receipt unlinked");
            let project =
                url::form_urlencoded::byte_serialize(form.project.as_bytes()).collect::<String>();
            Ok(Redirect::to(&format!(
                "/donations?notice=receipt-unlinked&project={project}"
            ))
            .into_response())
        }
        Err(StoreError::NotFound) => Err(AppError::NotFound("not found".into())),
        Err(e) => Err(e.into()),
    }
}

fn too_bad(status: StatusCode, why: &str) -> Response {
    (status, why.to_string()).into_response()
}
