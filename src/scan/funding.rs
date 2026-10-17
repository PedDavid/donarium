//! Finds how a project asks to be funded: the `FUNDING.yml` that GitHub
//! repositories keep in `.github/` (or, for a whole account, in its `.github`
//! repository). It only ever asks the configured GitHub API.

use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde_yaml_ng::Value;

use crate::funding;

/// How many ways to donate are kept per project.
const MAX_FOUND: usize = 10;
/// The largest FUNDING.yml read.
const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FinderError {
    #[error("GitHub could not be reached: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GitHub is rate limiting this server")]
    RateLimited,
    #[error("GitHub answered {0}")]
    Status(StatusCode),
}

/// One way to donate, as found. Same shape as a manually added one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub platform: String,
    pub handle: String,
    pub url: Option<String>,
}

#[derive(Clone)]
pub struct FundingFinder {
    http: Client,
    api: Url,
    token: Option<String>,
}

impl FundingFinder {
    pub fn new(api: Url, token: Option<String>) -> Result<FundingFinder, reqwest::Error> {
        Ok(FundingFinder {
            http: Client::builder()
                .timeout(Duration::from_secs(15))
                // The token must not follow a redirect to another host.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            api,
            token,
        })
    }

    async fn file(&self, repo: &str, path: &str) -> Result<Option<String>, FinderError> {
        let mut url = self.api.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(["repos"])
            .extend(repo.split('/'))
            .extend(["contents"])
            .extend(path.split('/'));
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github.raw+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let res = req.send().await?;
        match res.status() {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => Err(FinderError::RateLimited),
            s if s.is_success() => {
                let bytes = res.bytes().await?;
                if bytes.len() > MAX_BYTES {
                    return Ok(None);
                }
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            s => Err(FinderError::Status(s)),
        }
    }

    /// The ways to donate that `github.com/owner/name` declares. Nothing if it
    /// is not a GitHub project or declares none.
    pub async fn find(&self, repo: &str) -> Result<Vec<Found>, FinderError> {
        let Some(path) = repo.strip_prefix("github.com/") else {
            return Ok(Vec::new());
        };
        let Some((owner, _)) = path.split_once('/') else {
            return Ok(Vec::new());
        };
        let own = self.file(path, ".github/FUNDING.yml").await?;
        let text = match own {
            Some(text) => Some(text),
            // An account can set a default for all its repositories.
            None => {
                self.file(&format!("{owner}/.github"), "FUNDING.yml")
                    .await?
            }
        };
        Ok(text.map(|t| parse(&t)).unwrap_or_default())
    }
}

fn values(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.trim().to_string()],
        Value::Sequence(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.trim().to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads a FUNDING.yml. What does not look right is skipped: the file is
/// written by someone else.
pub fn parse(text: &str) -> Vec<Found> {
    let Ok(Value::Mapping(map)) = serde_yaml_ng::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut found: Vec<Found> = Vec::new();
    let mut add = |platform: &str, input: &str| {
        let Ok((handle, url)) = funding::parse(platform, input) else {
            return;
        };
        let item = Found {
            platform: platform.into(),
            handle,
            url,
        };
        if found.len() < MAX_FOUND && !found.contains(&item) {
            found.push(item);
        }
    };
    for (key, value) in &map {
        let Some(key) = key.as_str() else { continue };
        for v in values(value) {
            match key {
                "github" => add("github-sponsors", &v),
                "open_collective" => add("opencollective", &v),
                "ko_fi" => add("kofi", &v),
                "liberapay" => add("liberapay", &v),
                "patreon" => add("patreon", &v),
                // `gh/username`: only GitHub accounts have a link we know.
                "thanks_dev" => {
                    if let Some(user) = v.strip_prefix("gh/") {
                        add("thanks-dev", user);
                    }
                }
                "custom" => add("other", &v),
                // Platforms with a fixed link shape but no platform of their own.
                "tidelift" => add("other", &format!("https://tidelift.com/funding/github/{v}")),
                "buy_me_a_coffee" => add("other", &format!("https://www.buymeacoffee.com/{v}")),
                "polar" => add("other", &format!("https://polar.sh/{v}")),
                "issuehunt" => add("other", &format!("https://issuehunt.io/r/{v}")),
                _ => {}
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn found(platform: &str, handle: &str) -> Found {
        Found {
            platform: platform.into(),
            handle: handle.into(),
            url: None,
        }
    }

    #[test]
    fn reads_the_common_keys() {
        let yml = "github: [carllerche, seanmonstar]\nopen_collective: tokio\nko_fi: someone\ncustom: ['https://example.org/donate', 'http://insecure.example', 'javascript:alert(1)']\nthanks_dev: gh/tokio-rs\nunknown_platform: x\n";
        let got = parse(yml);
        assert_eq!(
            got,
            vec![
                found("github-sponsors", "carllerche"),
                found("github-sponsors", "seanmonstar"),
                found("opencollective", "tokio"),
                found("kofi", "someone"),
                Found {
                    platform: "other".into(),
                    handle: "https://example.org/donate".into(),
                    url: Some("https://example.org/donate".into()),
                },
                found("thanks-dev", "tokio-rs"),
            ]
        );
    }

    #[test]
    fn single_values_and_odd_values_are_handled() {
        assert_eq!(
            parse("github: user\n"),
            vec![found("github-sponsors", "user")]
        );
        // Names with characters a handle cannot have, links in handle fields,
        // nulls and nested maps are skipped.
        assert!(
            parse("github: [\"a b\", \"https://x.example/u\"]\npatreon:\nliberapay: {a: b}\n")
                .is_empty()
        );
        assert!(parse("- not\n- a map\n").is_empty());
        assert!(parse("{{{ not yaml").is_empty());
        assert_eq!(
            parse("tidelift: npm/lodash\n")[0].url.as_deref(),
            Some("https://tidelift.com/funding/github/npm/lodash")
        );
    }

    #[test]
    fn keeps_at_most_ten_without_duplicates() {
        let many: Vec<String> = (0..30).map(|i| format!("user{}", i % 15)).collect();
        let yml = format!("github: [{}]\n", many.join(", "));
        assert_eq!(parse(&yml).len(), MAX_FOUND);
    }

    fn finder(server: &MockServer, token: Option<&str>) -> FundingFinder {
        FundingFinder::new(server.uri().parse().unwrap(), token.map(Into::into)).unwrap()
    }

    #[tokio::test]
    async fn reads_the_repositorys_file_with_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/tokio-rs/tokio/contents/.github/FUNDING.yml"))
            .and(header("Authorization", "Bearer t0k"))
            .respond_with(ResponseTemplate::new(200).set_body_string("open_collective: tokio\n"))
            .mount(&server)
            .await;
        let got = finder(&server, Some("t0k"))
            .find("github.com/tokio-rs/tokio")
            .await
            .unwrap();
        assert_eq!(got, vec![found("opencollective", "tokio")]);
    }

    #[tokio::test]
    async fn falls_back_to_the_accounts_default_and_to_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/.github/contents/FUNDING.yml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("github: acme\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let f = finder(&server, None);
        assert_eq!(
            f.find("github.com/acme/tool").await.unwrap(),
            vec![found("github-sponsors", "acme")]
        );
        assert!(f.find("github.com/other/tool").await.unwrap().is_empty());
        // Other forges are not looked at, and make no request.
        let before = server.received_requests().await.unwrap().len();
        assert!(f.find("gitlab.com/g/p").await.unwrap().is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), before);
    }

    #[tokio::test]
    async fn rate_limits_and_errors_are_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/a/limited/contents/.github/FUNDING.yml"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = finder(&server, None);
        assert!(matches!(
            f.find("github.com/a/limited").await,
            Err(FinderError::RateLimited)
        ));
        assert!(matches!(
            f.find("github.com/a/b").await,
            Err(FinderError::Status(_))
        ));
    }
}
