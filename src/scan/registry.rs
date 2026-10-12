//! Reads the source repository of a container image from its registry: the
//! `org.opencontainers.image.source` label (or annotation) that most images
//! carry. Anonymous access only, so this finds public images.
//!
//! Only registries on the allow-list are contacted (see
//! [`super::resolve::registry_allowed`]), because the registry host comes from
//! the contents of a repository.

use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use super::image::ImageRef;
use super::resolve::registry_allowed;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// Largest manifest, config or token response that is read.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Labels and annotations that name the source repository, best first.
const SOURCE_KEYS: [&str; 2] = [
    "org.opencontainers.image.source",
    "org.label-schema.vcs-url",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LookupError {
    #[error("registry {0} is not on the allow-list")]
    NotAllowed(String),
    #[error("image not found (it may be private)")]
    NotFound,
    #[error("the registry is rate limiting requests; this is retried at the next scan")]
    RateLimited,
    #[error("registry authentication failed: {0}")]
    Auth(String),
    #[error("registry request failed: {0}")]
    Http(String),
    #[error("unexpected registry response: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug)]
pub struct RegistryClient {
    http: Client,
    extra_allowed: Vec<String>,
    /// Sends every registry's requests here instead (tests only).
    base_override: Option<String>,
    /// Requires token endpoints to be public https hosts.
    strict: bool,
}

impl RegistryClient {
    pub fn new(extra_allowed: Vec<String>) -> Result<RegistryClient, reqwest::Error> {
        Ok(RegistryClient {
            http: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::limited(5))
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            extra_allowed,
            base_override: None,
            strict: true,
        })
    }

    /// A client that talks to `base` for every registry, with every registry
    /// allowed and plain-http token endpoints accepted.
    pub fn for_tests(base: &str) -> RegistryClient {
        let mut client = RegistryClient::new(Vec::new()).expect("building an http client");
        client.base_override = Some(base.trim_end_matches('/').to_string());
        client.strict = false;
        client
    }

    pub fn allows(&self, registry: &str) -> bool {
        self.base_override.is_some() || registry_allowed(registry, &self.extra_allowed)
    }

    /// The label naming the image's source repository, if it has one.
    pub async fn source_label(&self, image: &ImageRef) -> Result<Option<String>, LookupError> {
        if !self.allows(&image.registry) {
            return Err(LookupError::NotAllowed(image.registry.clone()));
        }
        // lscr.io is linuxserver.io's front for ghcr.io.
        let registry = match image.registry.as_str() {
            "lscr.io" => "ghcr.io",
            other => other,
        };
        let base = match (&self.base_override, registry) {
            (Some(base), _) => base.clone(),
            (None, "docker.io") => "https://registry-1.docker.io".to_string(),
            (None, other) => format!("https://{other}"),
        };
        let repo = &image.repository;
        let reference = image
            .digest
            .as_deref()
            .or(image.tag.as_deref())
            .unwrap_or("latest");

        let mut token: Option<String> = None;
        let mut manifest = self
            .get_json(
                &format!("{base}/v2/{repo}/manifests/{reference}"),
                repo,
                &mut token,
                true,
            )
            .await?;
        if let Some(label) = annotation(&manifest) {
            return Ok(Some(label));
        }
        if let Some(entries) = manifest.get("manifests").and_then(Value::as_array) {
            // A multi-architecture image: follow one of its images.
            let Some(digest) = pick_platform(entries) else {
                return Ok(None);
            };
            manifest = self
                .get_json(
                    &format!("{base}/v2/{repo}/manifests/{digest}"),
                    repo,
                    &mut token,
                    true,
                )
                .await?;
            if let Some(label) = annotation(&manifest) {
                return Ok(Some(label));
            }
        }
        let Some(config) = manifest.pointer("/config/digest").and_then(Value::as_str) else {
            return Ok(None);
        };
        let blob = self
            .get_json(
                &format!("{base}/v2/{repo}/blobs/{config}"),
                repo,
                &mut token,
                false,
            )
            .await?;
        Ok(labels(&blob))
    }

    async fn get_json(
        &self,
        url: &str,
        repo: &str,
        token: &mut Option<String>,
        manifest: bool,
    ) -> Result<Value, LookupError> {
        for attempt in 0..2 {
            let mut request = self.http.get(url);
            if manifest {
                request = request.header(ACCEPT, MANIFEST_ACCEPT);
            }
            if let Some(t) = token.as_deref() {
                request = request.header(AUTHORIZATION, format!("Bearer {t}"));
            }
            let response = request
                .send()
                .await
                .map_err(|e| LookupError::Http(describe(&e)))?;
            match response.status() {
                s if s.is_success() => {
                    let body = read_capped(response).await?;
                    return serde_json::from_slice(&body)
                        .map_err(|e| LookupError::Invalid(format!("not JSON: {e}")));
                }
                StatusCode::UNAUTHORIZED if attempt == 0 && token.is_none() => {
                    *token = Some(self.fetch_token(response.headers(), repo).await?);
                }
                StatusCode::NOT_FOUND => return Err(LookupError::NotFound),
                StatusCode::TOO_MANY_REQUESTS => return Err(LookupError::RateLimited),
                // Private images answer 401 or 403 even with a token.
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    return Err(LookupError::NotFound);
                }
                other => return Err(LookupError::Http(format!("HTTP {other}"))),
            }
        }
        Err(LookupError::Auth(
            "the registry kept asking to log in".into(),
        ))
    }

    /// Anonymous token for the challenge a registry answered with.
    async fn fetch_token(&self, headers: &HeaderMap, repo: &str) -> Result<String, LookupError> {
        let challenge = headers
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| LookupError::Auth("401 without a challenge".into()))?;
        let params = parse_bearer_challenge(challenge)
            .ok_or_else(|| LookupError::Auth(format!("unsupported challenge: {challenge}")))?;
        let get = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let realm = get("realm").ok_or_else(|| LookupError::Auth("no realm".into()))?;
        let mut url = Url::parse(realm).map_err(|_| LookupError::Auth("bad realm".into()))?;
        if self.strict && !public_https(&url) {
            return Err(LookupError::Auth(format!(
                "refusing token endpoint {realm}"
            )));
        }
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = get("service") {
                query.append_pair("service", service);
            }
            let scope = get("scope")
                .map(str::to_string)
                .unwrap_or_else(|| format!("repository:{repo}:pull"));
            query.append_pair("scope", &scope);
        }
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| LookupError::Auth(describe(&e)))?;
        if !response.status().is_success() {
            return Err(LookupError::Auth(format!(
                "token endpoint answered {}",
                response.status()
            )));
        }
        let body: Value = serde_json::from_slice(&read_capped(response).await?)
            .map_err(|e| LookupError::Auth(format!("token response: {e}")))?;
        body.get("token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| LookupError::Auth("no token in the response".into()))
    }
}

fn describe(e: &reqwest::Error) -> String {
    // Without the URL: it can carry an auth query string.
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_redirect() {
        "too many redirects"
    } else {
        "request error"
    };
    kind.to_string()
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, LookupError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(LookupError::Invalid("response too large".into()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| LookupError::Http(describe(&e)))?
    {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(LookupError::Invalid("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A token endpoint must be https on a hostname, never an IP address or a
/// local name, so a registry cannot send the server to an internal address.
fn public_https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|h| {
            h != "localhost" && h.contains('.') && h.parse::<std::net::IpAddr>().is_err()
        })
}

/// `Bearer realm="…",service="…",scope="…"` as key/value pairs.
fn parse_bearer_challenge(header: &str) -> Option<Vec<(String, String)>> {
    let rest = header
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| header.trim().strip_prefix("bearer "))?;
    let mut pairs = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.next_if(|c| *c == ',' || c.is_whitespace()).is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=') {
            key.push(c);
        }
        if chars.next() != Some('=') {
            break;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => value.extend(chars.next()),
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                value.push(c);
            }
        }
        pairs.push((key.trim().to_ascii_lowercase(), value));
    }
    (!pairs.is_empty()).then_some(pairs)
}

fn annotation(manifest: &Value) -> Option<String> {
    let map = manifest.get("annotations")?.as_object()?;
    first_source(map)
}

fn labels(config: &Value) -> Option<String> {
    let map = config.pointer("/config/Labels")?.as_object()?;
    first_source(map)
}

fn first_source(map: &serde_json::Map<String, Value>) -> Option<String> {
    SOURCE_KEYS
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The image of an index to inspect: linux/amd64 if there is one, otherwise the
/// first real image (attestation manifests are not images).
fn pick_platform(entries: &[Value]) -> Option<String> {
    let real = |e: &&Value| {
        e.pointer("/platform/os").and_then(Value::as_str) != Some("unknown")
            && e.pointer("/annotations/vnd.docker.reference.type")
                .is_none()
    };
    let chosen = entries
        .iter()
        .filter(real)
        .find(|e| {
            e.pointer("/platform/os").and_then(Value::as_str) == Some("linux")
                && e.pointer("/platform/architecture").and_then(Value::as_str) == Some("amd64")
        })
        .or_else(|| entries.iter().find(real))?;
    chosen
        .get("digest")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SOURCE: &str = "org.opencontainers.image.source";

    fn image(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap()
    }

    async fn serve(server: &MockServer, p: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(p.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn reads_the_label_from_the_image_config() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/v2/jellyfin/jellyfin/manifests/10.9",
            json!({"config": {"digest": "sha256:cfg"}}),
        )
        .await;
        serve(
            &server,
            "/v2/jellyfin/jellyfin/blobs/sha256:cfg",
            json!({"config": {"Labels": {SOURCE: "https://github.com/jellyfin/jellyfin"}}}),
        )
        .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(
            client
                .source_label(&image("jellyfin/jellyfin:10.9"))
                .await
                .unwrap()
                .as_deref(),
            Some("https://github.com/jellyfin/jellyfin")
        );
    }

    #[tokio::test]
    async fn follows_the_token_challenge() {
        let server = MockServer::start().await;
        let realm = format!("{}/token", server.uri());
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("service", "registry.test"))
            .and(query_param("scope", "repository:o/app:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "T0K"})))
            .expect(1)
            .mount(&server)
            .await;
        // Without the token: a challenge. With it: the manifest.
        Mock::given(method("GET"))
            .and(path("/v2/o/app/manifests/latest"))
            .and(header("authorization", "Bearer T0K"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"config": {"digest": "sha256:c"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/o/app/manifests/latest"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(r#"Bearer realm="{realm}",service="registry.test",scope="repository:o/app:pull""#),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/o/app/blobs/sha256:c"))
            .and(header("authorization", "Bearer T0K"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"config": {"Labels": {SOURCE: "https://github.com/o/app"}}}),
                ),
            )
            .mount(&server)
            .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(
            client
                .source_label(&image("o/app"))
                .await
                .unwrap()
                .as_deref(),
            Some("https://github.com/o/app")
        );
    }

    #[tokio::test]
    async fn multi_arch_images_follow_linux_amd64() {
        let server = MockServer::start().await;
        serve(&server, "/v2/o/app/manifests/v1", json!({"manifests": [
            {"digest": "sha256:attest", "platform": {"os": "unknown", "architecture": "unknown"},
             "annotations": {"vnd.docker.reference.type": "attestation-manifest"}},
            {"digest": "sha256:arm", "platform": {"os": "linux", "architecture": "arm64"}},
            {"digest": "sha256:amd", "platform": {"os": "linux", "architecture": "amd64"}},
        ]})).await;
        serve(
            &server,
            "/v2/o/app/manifests/sha256:amd",
            json!({"config": {"digest": "sha256:cfg"}}),
        )
        .await;
        serve(&server, "/v2/o/app/blobs/sha256:cfg", json!({"config": {"Labels": {"org.label-schema.vcs-url": "https://gitlab.com/g/app.git"}}})).await;
        let client = RegistryClient::for_tests(&server.uri());
        // Falls back to the older label-schema key.
        assert_eq!(
            client
                .source_label(&image("o/app:v1"))
                .await
                .unwrap()
                .as_deref(),
            Some("https://gitlab.com/g/app.git")
        );
    }

    #[tokio::test]
    async fn annotations_on_the_index_are_enough() {
        let server = MockServer::start().await;
        serve(&server, "/v2/o/app/manifests/latest", json!({
            "annotations": {SOURCE: "https://github.com/o/app"},
            "manifests": [{"digest": "sha256:x", "platform": {"os": "linux", "architecture": "amd64"}}]
        })).await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(
            client
                .source_label(&image("o/app"))
                .await
                .unwrap()
                .as_deref(),
            Some("https://github.com/o/app")
        );
    }

    #[tokio::test]
    async fn images_without_a_label() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/v2/o/app/manifests/latest",
            json!({"config": {"digest": "sha256:cfg"}}),
        )
        .await;
        serve(
            &server,
            "/v2/o/app/blobs/sha256:cfg",
            json!({"config": {"Labels": {"maintainer": "x"}}}),
        )
        .await;
        serve(
            &server,
            "/v2/o/bare/manifests/latest",
            json!({"config": {"digest": "sha256:c2"}}),
        )
        .await;
        serve(&server, "/v2/o/bare/blobs/sha256:c2", json!({"config": {}})).await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(client.source_label(&image("o/app")).await.unwrap(), None);
        assert_eq!(client.source_label(&image("o/bare")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn missing_and_private_images() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/o/private/manifests/latest"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(
            client.source_label(&image("o/gone")).await,
            Err(LookupError::NotFound)
        );
        assert_eq!(
            client.source_label(&image("o/private")).await,
            Err(LookupError::NotFound)
        );
    }

    #[tokio::test]
    async fn rate_limits_are_reported_as_such() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/o/busy/manifests/latest"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
            .mount(&server)
            .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert_eq!(
            client.source_label(&image("o/busy")).await,
            Err(LookupError::RateLimited)
        );
    }

    #[tokio::test]
    async fn server_errors_and_garbage_are_errors_not_panics() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/o/down/manifests/latest"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/o/junk/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert!(matches!(
            client.source_label(&image("o/down")).await,
            Err(LookupError::Http(_))
        ));
        assert!(matches!(
            client.source_label(&image("o/junk")).await,
            Err(LookupError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_registry_that_always_challenges_does_not_loop() {
        let server = MockServer::start().await;
        let realm = format!("{}/token", server.uri());
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "x"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/o/app/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", format!(r#"Bearer realm="{realm}""#)),
            )
            .expect(2)
            .mount(&server)
            .await;
        let client = RegistryClient::for_tests(&server.uri());
        assert!(client.source_label(&image("o/app")).await.is_err());
    }

    #[tokio::test]
    async fn registries_off_the_allow_list_are_not_contacted() {
        let client = RegistryClient::new(vec![]).unwrap();
        for registry in [
            "registry.internal:5000/a",
            "localhost/a",
            "10.1.2.3:5000/a/b",
        ] {
            let image = image(registry);
            assert_eq!(
                client.source_label(&image).await,
                Err(LookupError::NotAllowed(image.registry.clone()))
            );
        }
        assert!(client.allows("ghcr.io"));
        let client = RegistryClient::new(vec!["registry.example.com".into()]).unwrap();
        assert!(client.allows("registry.example.com"));
    }

    #[tokio::test]
    async fn token_endpoints_must_be_public_https() {
        // A real (strict) client refuses a realm that points at a local address.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/o/app/manifests/latest"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Bearer realm="http://127.0.0.1:1/token",service="x""#,
            ))
            .mount(&server)
            .await;
        let mut client = RegistryClient::for_tests(&server.uri());
        client.strict = true;
        match client.source_label(&image("o/app")).await {
            Err(LookupError::Auth(msg)) => assert!(msg.contains("refusing"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn challenge_parsing() {
        let p = parse_bearer_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#).unwrap();
        assert_eq!(
            p[0],
            ("realm".into(), "https://auth.docker.io/token".into())
        );
        assert_eq!(p[1], ("service".into(), "registry.docker.io".into()));
        assert_eq!(p[2].1, "repository:library/nginx:pull");
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
        assert!(parse_bearer_challenge("Bearer ").is_none());
        let p = parse_bearer_challenge(r#"Bearer realm="https://ghcr.io/token", service=ghcr.io"#)
            .unwrap();
        assert_eq!(p[1], ("service".into(), "ghcr.io".into()));
    }

    #[test]
    fn public_https_hosts() {
        let ok = |s: &str| public_https(&Url::parse(s).unwrap());
        assert!(ok("https://auth.docker.io/token"));
        assert!(!ok("http://auth.docker.io/token"));
        assert!(!ok("https://127.0.0.1/token"));
        assert!(!ok("https://localhost/token"));
        assert!(!ok("https://[::1]/token"));
        assert!(!ok("https://registry/token"));
    }
}
