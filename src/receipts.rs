//! Receipts live in Paperless-ngx, where the email receipts usually arrive on
//! their own. Donarium only searches it and remembers a document's id, so
//! nothing is copied and Paperless keeps protecting the files.

use std::time::Duration;

use jiff::ToSpan;
use jiff::civil::Date;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

/// How many documents a search shows.
const PAGE_SIZE: u32 = 15;

#[derive(Debug, thiserror::Error)]
pub enum PaperlessError {
    #[error("Paperless could not be reached: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Paperless refused the token")]
    Unauthorized,
    #[error("Paperless answered {0}")]
    Status(StatusCode),
    #[error("there is no document {0} in Paperless")]
    NoSuchDocument(u64),
}

/// A document, as much of it as is shown when picking one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Document {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    /// The date on the document, `YYYY-MM-DD` (Paperless may add a time).
    #[serde(default)]
    pub created: String,
}

impl Document {
    /// The date part of `created`.
    pub fn day(&self) -> &str {
        self.created.get(..10).unwrap_or(&self.created)
    }
}

#[derive(Deserialize)]
struct Page {
    results: Vec<Document>,
}

#[derive(Clone)]
pub struct Paperless {
    http: Client,
    base: Url,
    token: String,
}

impl Paperless {
    pub fn new(base: Url, token: String) -> Result<Paperless, reqwest::Error> {
        Ok(Paperless {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                // The token is only ever sent to the configured server.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base,
            token,
        })
    }

    fn api(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(path.split('/'));
        // Paperless's API routes end in a slash.
        url.path_segments_mut().unwrap().push("");
        url
    }

    /// Where the document is shown in Paperless's own UI.
    pub fn document_url(&self, id: u64) -> String {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(["documents", &id.to_string(), "details"]);
        url.to_string()
    }

    async fn get(&self, url: Url) -> Result<reqwest::Response, PaperlessError> {
        let res = self
            .http
            .get(url)
            .header("Authorization", format!("Token {}", self.token))
            .header("Accept", "application/json")
            .send()
            .await?;
        match res.status() {
            s if s.is_success() => Ok(res),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(PaperlessError::Unauthorized),
            s => Err(PaperlessError::Status(s)),
        }
    }

    /// Documents matching `text`, most recently created first. With `near`,
    /// only those dated within a week of it, which is where the receipt of a
    /// donation made that day will be.
    pub async fn search(
        &self,
        text: &str,
        near: Option<Date>,
    ) -> Result<Vec<Document>, PaperlessError> {
        let mut url = self.api("api/documents");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("page_size", &PAGE_SIZE.to_string());
            q.append_pair("ordering", "-created");
            if !text.trim().is_empty() {
                q.append_pair("query", text.trim());
            }
            if let Some(day) = near
                && let (Ok(from), Ok(to)) = (day.checked_sub(7.days()), day.checked_add(7.days()))
            {
                q.append_pair("created__date__gte", &from.to_string());
                q.append_pair("created__date__lte", &to.to_string());
            }
        }
        Ok(self.get(url).await?.json::<Page>().await?.results)
    }

    pub async fn document(&self, id: u64) -> Result<Document, PaperlessError> {
        let url = self.api(&format!("api/documents/{id}"));
        match self.get(url).await {
            Ok(res) => Ok(res.json().await?),
            Err(PaperlessError::Status(StatusCode::NOT_FOUND)) => {
                Err(PaperlessError::NoSuchDocument(id))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Paperless {
        Paperless::new(server.uri().parse().unwrap(), "secret".into()).unwrap()
    }

    #[tokio::test]
    async fn searches_near_a_date_with_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .and(header("Authorization", "Token secret"))
            .and(query_param("query", "sponsors"))
            .and(query_param("created__date__gte", "2026-09-18"))
            .and(query_param("created__date__lte", "2026-10-02"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 1,
                "results": [{"id": 42, "title": "GitHub Sponsors receipt", "created": "2026-09-26T00:00:00+02:00", "correspondent": 3}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let found = client(&server)
            .search("sponsors", Some(date(2026, 9, 25)))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 42);
        assert_eq!(found[0].day(), "2026-09-26");
    }

    #[tokio::test]
    async fn reads_one_document_and_knows_when_it_is_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/documents/7/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": 7, "title": "Receipt", "created": "2026-01-02"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/8/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let p = client(&server);
        assert_eq!(p.document(7).await.unwrap().title, "Receipt");
        assert!(matches!(
            p.document(8).await,
            Err(PaperlessError::NoSuchDocument(8))
        ));
    }

    #[tokio::test]
    async fn a_refused_token_is_reported_and_redirects_are_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/documents/1/"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/2/"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "https://evil.example/"),
            )
            .mount(&server)
            .await;
        let p = client(&server);
        assert!(matches!(
            p.document(1).await,
            Err(PaperlessError::Unauthorized)
        ));
        assert!(matches!(
            p.document(2).await,
            Err(PaperlessError::Status(StatusCode::FOUND))
        ));
    }

    #[test]
    fn links_to_the_documents_page_under_a_sub_path() {
        let p = Paperless::new(
            "https://docs.example.com/paperless/".parse().unwrap(),
            "t".into(),
        )
        .unwrap();
        assert_eq!(
            p.document_url(5),
            "https://docs.example.com/paperless/documents/5/details"
        );
    }
}
