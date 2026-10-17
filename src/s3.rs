//! Storing receipts that exist nowhere else (a PDF saved from a donation
//! page) in an S3-compatible bucket. Requests are signed with AWS Signature
//! Version 4; downloads go through short-lived presigned URLs, handed out only
//! to logged-in users.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use reqwest::{Client, StatusCode, Url};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("the storage could not be reached: {0}")]
    Http(#[from] reqwest::Error),
    #[error("the storage answered {0}")]
    Status(StatusCode),
}

#[derive(Clone)]
pub struct S3Config {
    /// Where the S3 API is, e.g. `https://s3.eu-west-1.amazonaws.com` or a
    /// MinIO server. Buckets are addressed in the path.
    pub endpoint: Url,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Ask for server-side encryption (`AES256`) on every upload.
    pub sse: bool,
}

#[derive(Clone)]
pub struct S3 {
    http: Client,
    cfg: S3Config,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encodes everything but the unreserved characters, as SigV4 wants.
/// `/` is kept when encoding a path.
fn encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

struct Signing<'a> {
    secret_key: &'a str,
    region: &'a str,
    now: Timestamp,
}

impl Signing<'_> {
    fn date(&self) -> String {
        self.now.strftime("%Y%m%d").to_string()
    }
    fn stamp(&self) -> String {
        self.now.strftime("%Y%m%dT%H%M%SZ").to_string()
    }
    fn scope(&self) -> String {
        format!("{}/{}/s3/aws4_request", self.date(), self.region)
    }
    fn signature(&self, canonical_request: &str) -> String {
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            self.stamp(),
            self.scope(),
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac(format!("AWS4{}", self.secret_key).as_bytes(), &self.date());
        let k_region = hmac(&k_date, self.region);
        let k_service = hmac(&k_region, "s3");
        let k_signing = hmac(&k_service, "aws4_request");
        hex(hmac(&k_signing, &to_sign))
    }
}

/// `host` or `host:port`, as the `Host` header carries it.
fn host_of(url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    }
}

/// A presigned GET for `path` on `host`, valid for `expires` seconds. `query`
/// holds extra (unencoded) parameters to sign, such as response overrides.
#[allow(clippy::too_many_arguments)]
fn presign_get(
    scheme: &str,
    host: &str,
    path: &str,
    query: &[(&str, String)],
    access_key: &str,
    secret_key: &str,
    region: &str,
    expires: u64,
    now: Timestamp,
) -> Url {
    let signing = Signing {
        secret_key,
        region,
        now,
    };
    let mut params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        (
            "X-Amz-Credential".into(),
            format!("{access_key}/{}", signing.scope()),
        ),
        ("X-Amz-Date".into(), signing.stamp()),
        ("X-Amz-Expires".into(), expires.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    params.extend(query.iter().map(|(k, v)| (k.to_string(), v.clone())));
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (encode(k, false), encode(v, false)))
        .collect();
    encoded.sort();
    let query_string = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let encoded_path = encode(path, true);
    let canonical =
        format!("GET\n{encoded_path}\n{query_string}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let signature = signing.signature(&canonical);
    format!("{scheme}://{host}{encoded_path}?{query_string}&X-Amz-Signature={signature}")
        .parse()
        .expect("a signed URL is a URL")
}

impl S3 {
    pub fn new(cfg: S3Config) -> Result<S3, reqwest::Error> {
        Ok(S3 {
            http: Client::builder()
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            cfg,
        })
    }

    fn path(&self, key: &str) -> String {
        let prefix = self.cfg.endpoint.path().trim_end_matches('/');
        format!("{prefix}/{}/{key}", self.cfg.bucket)
    }

    /// A link to download `key` that works for five minutes, shown as a file
    /// called `filename` of type `content_type`.
    pub fn presigned_download(
        &self,
        key: &str,
        filename: &str,
        content_type: &str,
        now: Timestamp,
    ) -> Url {
        presign_get(
            self.cfg.endpoint.scheme(),
            &host_of(&self.cfg.endpoint),
            &self.path(key),
            &[
                (
                    "response-content-disposition",
                    format!("attachment; filename=\"{filename}\""),
                ),
                ("response-content-type", content_type.to_string()),
            ],
            &self.cfg.access_key,
            &self.cfg.secret_key,
            &self.cfg.region,
            300,
            now,
        )
    }

    /// Uploads `body` as `key`.
    pub async fn put(
        &self,
        key: &str,
        content_type: &str,
        body: Vec<u8>,
        now: Timestamp,
    ) -> Result<(), S3Error> {
        let signing = Signing {
            secret_key: &self.cfg.secret_key,
            region: &self.cfg.region,
            now,
        };
        let host = host_of(&self.cfg.endpoint);
        let payload = sha256_hex(&body);
        let stamp = signing.stamp();
        // Names in sorted order, as the canonical request needs them.
        let mut headers: Vec<(&str, String)> = vec![
            ("content-type", content_type.to_string()),
            ("host", host.clone()),
            ("x-amz-content-sha256", payload.clone()),
            ("x-amz-date", stamp.clone()),
        ];
        if self.cfg.sse {
            headers.push(("x-amz-server-side-encryption", "AES256".into()));
        }
        let signed = headers
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let path = encode(&self.path(key), true);
        let canonical = format!("PUT\n{path}\n\n{canonical_headers}\n{signed}\n{payload}");
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={signed}, Signature={}",
            self.cfg.access_key,
            signing.scope(),
            signing.signature(&canonical)
        );
        let url = format!("{}://{host}{path}", self.cfg.endpoint.scheme());
        let mut req = self
            .http
            .put(url)
            .header("Authorization", authorization)
            .header("x-amz-date", stamp)
            .header("x-amz-content-sha256", payload)
            .header("Content-Type", content_type);
        if self.cfg.sse {
            req = req.header("x-amz-server-side-encryption", "AES256");
        }
        let res = req.body(body).send().await?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(S3Error::Status(res.status()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The example in AWS's documentation of query-string authentication.
    #[test]
    fn matches_the_aws_presigned_url_example() {
        let url = presign_get(
            "https",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            86400,
            "2013-05-24T00:00:00Z".parse().unwrap(),
        );
        assert_eq!(
            url.as_str(),
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    fn s3(server: &MockServer, sse: bool) -> S3 {
        S3::new(S3Config {
            endpoint: server.uri().parse().unwrap(),
            region: "eu-west-1".into(),
            bucket: "receipts".into(),
            access_key: "AK".into(),
            secret_key: "SK".into(),
            sse,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn uploads_with_a_signature_and_the_encryption_header() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/receipts/receipts/2026/1/ab-my_file.pdf"))
            .and(header("x-amz-server-side-encryption", "AES256"))
            .and(header("content-type", "application/pdf"))
            .and(header_exists("x-amz-content-sha256"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        s3(&server, true)
            .put(
                "receipts/2026/1/ab-my_file.pdf",
                "application/pdf",
                b"%PDF-1.7".to_vec(),
                "2026-10-01T08:00:00Z".parse().unwrap(),
            )
            .await
            .unwrap();
        let req = &server.received_requests().await.unwrap()[0];
        let auth = req.headers["authorization"].to_str().unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AK/20261001/eu-west-1/s3/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-server-side-encryption, Signature="
        ), "{auth}");
        assert_eq!(
            req.headers["x-amz-content-sha256"].to_str().unwrap(),
            sha256_hex(b"%PDF-1.7")
        );
    }

    #[tokio::test]
    async fn without_sse_the_header_is_not_sent_and_failures_are_reported() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let res = s3(&server, false)
            .put(
                "k",
                "image/png",
                vec![1],
                "2026-10-01T08:00:00Z".parse().unwrap(),
            )
            .await;
        assert!(matches!(res, Err(S3Error::Status(StatusCode::FORBIDDEN))));
        let req = &server.received_requests().await.unwrap()[0];
        assert!(!req.headers.contains_key("x-amz-server-side-encryption"));
    }

    #[test]
    fn downloads_are_presigned_for_five_minutes_with_the_file_name() {
        let cfg = S3Config {
            endpoint: "http://minio.local:9000".parse().unwrap(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            access_key: "AK".into(),
            secret_key: "SK".into(),
            sse: false,
        };
        let url = S3::new(cfg).unwrap().presigned_download(
            "receipts/2026/1/ab-a b.pdf",
            "a b.pdf",
            "application/pdf",
            "2026-10-01T08:00:00Z".parse().unwrap(),
        );
        assert_eq!(url.host_str(), Some("minio.local"));
        assert_eq!(url.port(), Some(9000));
        assert_eq!(url.path(), "/b/receipts/2026/1/ab-a%20b.pdf");
        let q = url.query().unwrap();
        assert!(q.contains("X-Amz-Expires=300"));
        assert!(
            q.contains("response-content-disposition=attachment%3B%20filename%3D%22a%20b.pdf%22")
        );
        assert!(q.contains("X-Amz-Signature="));
    }
}
