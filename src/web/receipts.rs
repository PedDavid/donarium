//! Linking donations to receipts that already live in Paperless-ngx. Admins
//! search it (suggestions come from the donation's date), pick a document, and
//! only its id is stored.

use askama::Template;
use axum::Form;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use tracing::info;

use super::auth::{Admin, User};
use super::pages::{Ctx, Layout};
use super::views::{DonationRow, ReceiptLink};
use super::{AppError, AppState};
use crate::model::{NewUpload, ReceiptStore};
use crate::receipts::{Document, PaperlessError};
use crate::s3::sha256_hex;
use crate::store::StoreError;

/// The biggest receipt file accepted, in bytes.
pub const MAX_UPLOAD: usize = 10 * 1024 * 1024;

/// Fills in the receipts of `rows`, and whether more can be linked.
pub async fn attach(state: &AppState, rows: &mut [DonationRow]) -> Result<(), AppError> {
    let paperless = state.inner.paperless.as_ref();
    if paperless.is_none() && state.inner.s3.is_none() {
        return Ok(());
    }
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
                url: match (r.store, paperless) {
                    (ReceiptStore::Paperless, Some(p)) => r
                        .reference
                        .parse()
                        .map(|id| p.document_url(id))
                        .unwrap_or_default(),
                    (ReceiptStore::S3, _) => format!("/receipts/{}/file", r.id),
                    _ => String::new(),
                },
            })
            .collect();
    }
    Ok(())
}

/// What a file is, from its first bytes, if it is a kind of receipt we keep.
fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// A name that is safe in an object key and a header: its last path segment,
/// with anything but letters, digits and `._-` replaced.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let clean = clean.trim_start_matches('.').to_string();
    if clean.is_empty() {
        "receipt".into()
    } else {
        clean
    }
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
    /// Paperless is configured, so there is something to search.
    paperless: bool,
    /// Files can be uploaded.
    upload: bool,
    max_mb: usize,
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
    show(&state, &session, &jar, id, q.q.trim(), None).await
}

/// The page to find or upload a receipt, optionally with something that went
/// wrong when the admin just tried to.
async fn show(
    state: &AppState,
    session: &super::auth::Session,
    jar: &CookieJar,
    id: i64,
    query: &str,
    problem: Option<(StatusCode, String)>,
) -> Result<Response, AppError> {
    let ctx = Ctx {
        state,
        session,
        jar,
    };
    let (paperless, s3) = (&state.inner.paperless, &state.inner.s3);
    if paperless.is_none() && s3.is_none() {
        return Err(AppError::NotFound("receipts are not set up".into()));
    }
    let donation = state
        .inner
        .store
        .get_donation(id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such donation".into()))?;
    let base = &state.inner.cfg.base_currency;
    let row = DonationRow::new(&donation, base);
    // With no search text, suggest what is dated around the donation. With
    // some, search everything: the receipt may be dated differently.
    let suggested = query.is_empty();
    let (mut hits, mut error, mut status) = (Vec::new(), String::new(), StatusCode::OK);
    if let Some(paperless) = paperless {
        match paperless
            .search(query, suggested.then_some(donation.donated_on))
            .await
        {
            Ok(docs) => hits = docs.into_iter().map(Hit::from).collect(),
            Err(e) => (error, status) = (e.to_string(), StatusCode::BAD_GATEWAY),
        }
    }
    if let Some((s, why)) = problem {
        (error, status) = (why, s);
    }
    let page = ReceiptPage {
        layout: Layout::new(ctx.state, ctx.session, ctx.jar, "Find receipt").section("donations"),
        id,
        summary: format!("{} to {}, {}", row.amount, row.project, row.date),
        query: query.to_string(),
        suggested,
        hits,
        paperless: paperless.is_some(),
        upload: s3.is_some(),
        max_mb: MAX_UPLOAD / (1024 * 1024),
        error,
    };
    Ok((status, Html(page.render()?)).into_response())
}

pub async fn upload(
    State(state): State<AppState>,
    Admin(session): Admin,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    mut form: Multipart,
) -> Result<Response, AppError> {
    let Some(s3) = &state.inner.s3 else {
        return Err(AppError::NotFound("uploads are not set up".into()));
    };
    let (mut csrf, mut file) = (None, None);
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("the upload could not be read: {e}")))?
    {
        match field.name() {
            Some("_csrf") => csrf = field.text().await.ok(),
            Some("file") => {
                let name = field.file_name().unwrap_or_default().to_string();
                let data = field.bytes().await.map_err(|_| {
                    AppError::BadRequest(format!(
                        "the file is too big: the limit is {} MB",
                        MAX_UPLOAD / (1024 * 1024)
                    ))
                })?;
                file = Some((name, data));
            }
            _ => {}
        }
    }
    session.check_csrf(&headers, csrf.as_deref())?;
    let reject = |why: &str| Some((StatusCode::UNPROCESSABLE_ENTITY, why.to_string()));
    let Some((name, data)) = file else {
        return show(&state, &session, &jar, id, "", reject("choose a file")).await;
    };
    if data.is_empty() || data.len() > MAX_UPLOAD {
        let why = format!(
            "the file must be between 1 byte and {} MB",
            MAX_UPLOAD / (1024 * 1024)
        );
        return show(&state, &session, &jar, id, "", reject(&why)).await;
    }
    let Some(content_type) = sniff(&data) else {
        return show(
            &state,
            &session,
            &jar,
            id,
            "",
            reject("only PDF, PNG and JPEG files are accepted"),
        )
        .await;
    };
    let donation = state
        .inner
        .store
        .get_donation(id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such donation".into()))?;
    let filename = sanitize_filename(&name);
    let sha256 = sha256_hex(&data);
    let key = format!(
        "receipts/{}/{id}/{}-{filename}",
        donation.donated_on.year(),
        &sha256[..12]
    );
    let now = jiff::Timestamp::now();
    if let Err(e) = s3.put(&key, content_type, data.to_vec(), now).await {
        tracing::warn!(error = %e, donation = id, "receipt upload failed");
        return show(
            &state,
            &session,
            &jar,
            id,
            "",
            Some((StatusCode::BAD_GATEWAY, e.to_string())),
        )
        .await;
    }
    let upload = NewUpload {
        key,
        filename,
        content_type: content_type.to_string(),
        size: data.len() as i64,
        sha256,
    };
    match state
        .inner
        .store
        .add_upload(id, &upload, &session.actor(), now)
        .await
    {
        Ok(()) => {
            info!(actor = %session.sub, donation = id, key = %upload.key, "receipt uploaded");
            Ok(Redirect::to("/donations?notice=receipt-linked").into_response())
        }
        Err(StoreError::InUse(why)) => Ok(too_bad(StatusCode::CONFLICT, &why)),
        Err(StoreError::NotFound) => Err(AppError::NotFound("no such donation".into())),
        Err(e) => Err(e.into()),
    }
}

/// Sends a logged-in user to a short-lived link to an uploaded receipt.
pub async fn download(
    State(state): State<AppState>,
    User(_session): User,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(s3) = &state.inner.s3 else {
        return Err(AppError::NotFound("uploads are not set up".into()));
    };
    let receipt = state
        .inner
        .store
        .get_receipt(id)
        .await?
        .filter(|r| r.store == ReceiptStore::S3)
        .ok_or_else(|| AppError::NotFound("no such receipt".into()))?;
    let url = s3.presigned_download(
        &receipt.reference,
        &receipt.title,
        receipt
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream"),
        jiff::Timestamp::now(),
    );
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Redirect::to(url.as_str()),
    )
        .into_response())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pdf_png_and_jpeg_are_recognised_by_content() {
        assert_eq!(sniff(b"%PDF-1.4\n"), Some("application/pdf"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        for other in [
            &b""[..],
            b"<html>",
            b"MZ",
            b"GIF89a",
            b"PK\x03\x04",
            b" %PDF-",
        ] {
            assert_eq!(sniff(other), None, "{other:?}");
        }
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(sanitize_filename("receipt.pdf"), "receipt.pdf");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Users\\me\\a b.pdf"), "a_b.pdf");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        assert_eq!(sanitize_filename("quo\"te\r\n.pdf"), "quo_te__.pdf");
        assert_eq!(sanitize_filename(""), "receipt");
        assert_eq!(sanitize_filename("..."), "receipt");
        assert_eq!(sanitize_filename(&"a".repeat(300)).len(), 100);
    }
}
