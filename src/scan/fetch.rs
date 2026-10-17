//! Downloads a repository as one archive and reads its YAML files in memory.
//! Nothing is written to disk, and archive paths are only ever used as
//! labels, so a hostile archive cannot escape anywhere.

use std::io::Read;
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::{Client, StatusCode};
use url::Url;

use super::manifests::is_candidate;

/// Limits that keep a hostile or huge repository from using the server up.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Compressed archive size.
    pub download: u64,
    /// Total bytes read after decompression (a "zip bomb" guard).
    pub expanded: u64,
    /// Largest single YAML file that is read.
    pub file: u64,
    /// Most YAML files that are read.
    pub files: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            download: 64 * 1024 * 1024,
            expanded: 512 * 1024 * 1024,
            file: 1024 * 1024,
            files: 5000,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("repository not found, or it is private and needs a token")]
    NotFound,
    #[error("the host refused the request ({0}); check the token")]
    Denied(StatusCode),
    #[error("the host is rate limiting requests; try again later, or configure a token")]
    RateLimited,
    #[error("request failed: {0}")]
    Http(String),
    #[error("the archive is larger than {0} bytes")]
    TooLarge(u64),
    #[error("could not read the archive: {0}")]
    Archive(String),
    #[error("the host sent the download somewhere it should not: {0}")]
    Redirect(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// (path, content) of each YAML file, paths relative to the repository root.
    pub files: Vec<(String, String)>,
    /// More YAML files than the limit: the rest were not read.
    pub truncated: bool,
}

/// A forge whose repositories can be downloaded as an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forge {
    /// `base` is the instance, e.g. `https://gitlab.com`.
    Gitlab { base: Url },
    /// Gitea or Forgejo; `base` is the server, e.g. `https://git.example.com`.
    Gitea { base: Url },
    /// `api` is the API root, `https://api.github.com`.
    Github { api: Url },
}

/// Downloads repositories from one forge. The token is sent only to that
/// forge's own host: redirects are not followed, except GitHub's one hop to
/// its download host, which is made without the token.
#[derive(Clone)]
pub struct ArchiveFetcher {
    http: Client,
    forge: Forge,
    token: Option<String>,
    limits: Limits,
    /// Hosts (`host` or `host:port`) a download may be redirected to.
    redirect_hosts: Vec<String>,
    /// Allow `http` redirect targets (tests, where the mock server has no TLS).
    insecure_redirects: bool,
}

// The token must never show up in logs.
impl std::fmt::Debug for ArchiveFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveFetcher")
            .field("forge", &self.forge)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl ArchiveFetcher {
    pub fn gitlab(base: Url, token: Option<String>) -> Result<ArchiveFetcher, reqwest::Error> {
        ArchiveFetcher::new(Forge::Gitlab { base }, token, vec![])
    }

    pub fn gitea(base: Url, token: Option<String>) -> Result<ArchiveFetcher, reqwest::Error> {
        ArchiveFetcher::new(Forge::Gitea { base }, token, vec![])
    }

    pub fn github(api: Url, token: Option<String>) -> Result<ArchiveFetcher, reqwest::Error> {
        // GitHub answers the tarball request with a redirect to its download host.
        ArchiveFetcher::new(
            Forge::Github { api },
            token,
            vec!["codeload.github.com".into()],
        )
    }

    fn new(
        forge: Forge,
        token: Option<String>,
        redirect_hosts: Vec<String>,
    ) -> Result<ArchiveFetcher, reqwest::Error> {
        Ok(ArchiveFetcher {
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            forge,
            token: token.filter(|t| !t.trim().is_empty()),
            limits: Limits::default(),
            redirect_hosts,
            insecure_redirects: false,
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> ArchiveFetcher {
        self.limits = limits;
        self
    }

    /// Also accepts a redirect to `host` over plain http. For tests.
    pub fn allowing_insecure_redirect_to(mut self, host: &str) -> ArchiveFetcher {
        self.redirect_hosts.push(host.to_string());
        self.insecure_redirects = true;
        self
    }

    pub async fn fetch(&self, repo: &str) -> Result<Fetched, FetchError> {
        let archive = match &self.forge {
            Forge::Gitlab { base } => {
                let url = join(
                    base,
                    &format!(
                        "api/v4/projects/{}/repository/archive.tar.gz",
                        encode_segment(repo)
                    ),
                );
                self.download(self.get(url, true).await?).await?
            }
            Forge::Gitea { base } => {
                let info = join(base, &format!("api/v1/repos/{}", encode_path(repo)));
                let info = self.get(info, true).await?;
                let branch = default_branch(&self.read_small(info).await?)?;
                let url = join(
                    base,
                    &format!(
                        "api/v1/repos/{}/archive/{}.tar.gz",
                        encode_path(repo),
                        encode_path(&branch)
                    ),
                );
                self.download(self.get(url, true).await?).await?
            }
            Forge::Github { api } => {
                let url = join(api, &format!("repos/{}/tarball", encode_path(repo)));
                let response = self.get(url, true).await?;
                if response.status().is_redirection() {
                    let target = self.redirect_target(&response)?;
                    // The download host gets no token: the address carries its own.
                    self.download(self.get(target, false).await?).await?
                } else {
                    self.download(response).await?
                }
            }
        };
        let limits = self.limits;
        tokio::task::spawn_blocking(move || read_archive(&archive, limits))
            .await
            .map_err(|e| FetchError::Archive(e.to_string()))?
    }

    /// Sends a GET and turns error statuses into [`FetchError`]s. Redirects
    /// are returned as they are (see [`Self::redirect_target`]).
    async fn get(&self, url: Url, authenticated: bool) -> Result<reqwest::Response, FetchError> {
        let mut request = self.http.get(url);
        if authenticated {
            request = match (&self.forge, &self.token) {
                (Forge::Gitlab { .. }, Some(t)) => request.header("PRIVATE-TOKEN", t),
                (Forge::Gitea { .. }, Some(t)) => {
                    request.header("Authorization", format!("token {t}"))
                }
                (Forge::Github { .. }, Some(t)) => request.bearer_auth(t),
                (_, None) => request,
            };
        }
        if matches!(self.forge, Forge::Github { .. }) {
            request = request
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
        }
        let response = request.send().await.map_err(|e| {
            FetchError::Http(if e.is_timeout() {
                "timed out".into()
            } else if e.is_connect() {
                "could not connect".into()
            } else {
                "request error".into()
            })
        })?;
        let status = response.status();
        if status.is_success() || status.is_redirection() {
            return Ok(response);
        }
        let out_of_quota = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        match status {
            StatusCode::NOT_FOUND => Err(FetchError::NotFound),
            StatusCode::TOO_MANY_REQUESTS => Err(FetchError::RateLimited),
            // GitHub reports an exhausted quota as 403.
            StatusCode::FORBIDDEN if out_of_quota => Err(FetchError::RateLimited),
            // A private repository without access is a 404 on most forges,
            // but a bad token is a 401.
            s @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => Err(FetchError::Denied(s)),
            s => Err(FetchError::Http(format!("HTTP {s}"))),
        }
    }

    /// Where a redirect points, if that is an allowed download host.
    fn redirect_target(&self, response: &reqwest::Response) -> Result<Url, FetchError> {
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| FetchError::Redirect("a redirect without a location".into()))?;
        let target = response
            .url()
            .join(location)
            .map_err(|_| FetchError::Redirect("an invalid location".into()))?;
        let host = match (target.host_str(), target.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => return Err(FetchError::Redirect("a location without a host".into())),
        };
        let secure =
            target.scheme() == "https" || (self.insecure_redirects && target.scheme() == "http");
        if !secure
            || !self
                .redirect_hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&host))
        {
            return Err(FetchError::Redirect(host));
        }
        Ok(target)
    }

    /// A small JSON response, for API calls that are not the download.
    async fn read_small(&self, response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
        const MAX: usize = 1024 * 1024;
        read_limited(response, MAX as u64).await
    }

    async fn download(&self, response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
        if response.status().is_redirection() {
            return Err(FetchError::Redirect(
                "an unexpected redirect while downloading".into(),
            ));
        }
        read_limited(response, self.limits.download).await
    }
}

async fn read_limited(mut response: reqwest::Response, max: u64) -> Result<Vec<u8>, FetchError> {
    if response.content_length().is_some_and(|n| n > max) {
        return Err(FetchError::TooLarge(max));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| FetchError::Http("the download was interrupted".into()))?
    {
        if body.len() as u64 + chunk.len() as u64 > max {
            return Err(FetchError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn default_branch(body: &[u8]) -> Result<String, FetchError> {
    let info: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| FetchError::Http("the repository details are not JSON".into()))?;
    info.get("default_branch")
        .and_then(serde_json::Value::as_str)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            FetchError::Http("the repository has no default branch (is it empty?)".into())
        })
}

/// `base` with `path` below its own path.
fn join(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}/{path}", base.path().trim_end_matches('/')));
    url
}

/// One path segment with everything escaped, including '/'.
fn encode_segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// A path whose '/' stay separators and whose parts are escaped.
fn encode_path(s: &str) -> String {
    s.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads the YAML files out of a `.tar.gz`, dropping the single top-level
/// folder GitLab and GitHub put every file under.
pub fn read_archive(archive: &[u8], limits: Limits) -> Result<Fetched, FetchError> {
    let invalid = |e: std::io::Error| FetchError::Archive(e.to_string());
    // `take` is the decompression-bomb guard: reading stops at the limit and
    // the archive then looks truncated, which is reported as an error.
    let decoder = GzDecoder::new(archive).take(limits.expanded);
    let mut tar = tar::Archive::new(decoder);
    let mut out = Fetched::default();
    for entry in tar.entries().map_err(invalid)? {
        let mut entry = entry.map_err(invalid)?;
        // Regular files only: no links, devices or directories.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(invalid)?
            .to_string_lossy()
            .into_owned();
        let Some(relative) = path.split_once('/').map(|(_, rest)| rest) else {
            continue;
        };
        if relative
            .split('/')
            .any(|part| part == ".." || part.is_empty())
            || !is_candidate(relative)
        {
            continue;
        }
        if entry.size() > limits.file {
            continue;
        }
        if out.files.len() >= limits.files {
            out.truncated = true;
            break;
        }
        let mut content = String::new();
        // Not UTF-8: not a YAML file worth reading.
        if entry.read_to_string(&mut content).is_ok() {
            out.files.push((relative.to_string(), content));
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod testing {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// Builds a `.tar.gz` like GitLab's: everything under one top folder.
    pub fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    format!("media-server-main-abc123/{name}"),
                    *data,
                )
                .unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::archive;

    #[test]
    fn keeps_only_small_yaml_files() {
        let big = vec![b'a'; 2 * 1024 * 1024];
        let bytes = archive(&[
            ("compose.yaml", b"services:\n  a:\n    image: nginx\n"),
            ("stacks/arr/docker-compose.yml", b"services: {}\n"),
            ("README.md", b"# hi"),
            ("node_modules/x/compose.yaml", b"services: {}"),
            ("huge.yaml", &big),
            ("binary.yaml", &[0xff, 0xfe, 0x00, 0x80]),
        ]);
        let got = read_archive(&bytes, Limits::default()).unwrap();
        let mut paths: Vec<&str> = got.files.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["compose.yaml", "stacks/arr/docker-compose.yml"]);
        assert!(!got.truncated);
        assert!(got.files.iter().any(|(_, c)| c.contains("image: nginx")));
    }

    #[test]
    fn symlinks_and_odd_paths_are_skipped() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_cksum();
        builder
            .append_link(&mut link, "top/evil.yaml", "/etc/passwd")
            .unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(2);
        file.set_cksum();
        // A top-level file has no folder to strip and is not a repository file.
        builder
            .append_data(&mut file, "loose.yaml", &b"a:"[..])
            .unwrap();
        let tar = builder.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let got = read_archive(&gz.finish().unwrap(), Limits::default()).unwrap();
        assert!(got.files.is_empty(), "{:?}", got.files);
    }

    #[test]
    fn limits_are_enforced() {
        let many: Vec<(String, Vec<u8>)> = (0..10)
            .map(|i| (format!("f{i}.yaml"), b"a: 1".to_vec()))
            .collect();
        let refs: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        let bytes = archive(&refs);
        let limited = Limits {
            files: 3,
            ..Limits::default()
        };
        let got = read_archive(&bytes, limited).unwrap();
        assert_eq!(got.files.len(), 3);
        assert!(got.truncated);

        // A bomb: tiny when compressed, enormous when expanded.
        let bomb = archive(&[("a.yaml", &vec![b'x'; 8 * 1024 * 1024][..])]);
        assert!(
            bomb.len() < 100 * 1024,
            "the test archive must compress well"
        );
        let small = Limits {
            expanded: 1024 * 1024,
            file: 10 * 1024 * 1024,
            ..Limits::default()
        };
        assert!(matches!(
            read_archive(&bomb, small),
            Err(FetchError::Archive(_))
        ));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(matches!(
            read_archive(b"not an archive", Limits::default()),
            Err(FetchError::Archive(_))
        ));
    }

    #[tokio::test]
    async fn downloads_a_public_project() {
        let server = MockServer::start().await;
        let body = archive(&[("compose.yaml", b"services:\n  a:\n    image: nginx\n")]);
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/me%2Fmedia-server/repository/archive.tar.gz",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(1)
            .mount(&server)
            .await;
        let fetcher = ArchiveFetcher::gitlab(server.uri().parse().unwrap(), None).unwrap();
        let got = fetcher.fetch("me/media-server").await.unwrap();
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].0, "compose.yaml");
    }

    #[tokio::test]
    async fn nested_groups_are_encoded_and_the_token_is_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/group%2Fsub%2Fproj/repository/archive.tar.gz",
            ))
            .and(header("private-token", "s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[])))
            .expect(1)
            .mount(&server)
            .await;
        let fetcher =
            ArchiveFetcher::gitlab(server.uri().parse().unwrap(), Some("s3cret".into())).unwrap();
        assert!(
            fetcher
                .fetch("group/sub/proj")
                .await
                .unwrap()
                .files
                .is_empty()
        );
        // The token is not printed with the fetcher.
        assert!(!format!("{fetcher:?}").contains("s3cret"));
    }

    #[tokio::test]
    async fn error_statuses() {
        let server = MockServer::start().await;
        for (name, status) in [("gone", 404), ("bad-token", 401), ("down", 503)] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/v4/projects/me%2F{name}/repository/archive.tar.gz"
                )))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
        }
        let fetcher = ArchiveFetcher::gitlab(server.uri().parse().unwrap(), None).unwrap();
        assert_eq!(fetcher.fetch("me/gone").await, Err(FetchError::NotFound));
        assert_eq!(
            fetcher.fetch("me/bad-token").await,
            Err(FetchError::Denied(StatusCode::UNAUTHORIZED))
        );
        assert!(matches!(
            fetcher.fetch("me/down").await,
            Err(FetchError::Http(_))
        ));
    }

    #[tokio::test]
    async fn oversized_downloads_are_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/me%2Fbig/repository/archive.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4096]))
            .mount(&server)
            .await;
        let fetcher = ArchiveFetcher::gitlab(server.uri().parse().unwrap(), None)
            .unwrap()
            .with_limits(Limits {
                download: 1024,
                ..Limits::default()
            });
        assert_eq!(
            fetcher.fetch("me/big").await,
            Err(FetchError::TooLarge(1024))
        );
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/me%2Fmoved/repository/archive.tar.gz",
            ))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "http://127.0.0.1:1/elsewhere"),
            )
            .mount(&server)
            .await;
        let fetcher =
            ArchiveFetcher::gitlab(server.uri().parse().unwrap(), Some("t".into())).unwrap();
        assert!(matches!(
            fetcher.fetch("me/moved").await,
            Err(FetchError::Redirect(_))
        ));
    }

    // --- Gitea and Forgejo ---

    const TOKEN: &str = "T0K";

    async fn gitea_repo(server: &MockServer, branch: &str, archive_path: &str) {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/flux"))
            .and(header("authorization", "token T0K"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"default_branch": branch})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(archive_path.to_string()))
            .and(header("authorization", "token T0K"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[(
                "apps/web.yaml",
                b"kind: Deployment\nspec:\n  containers:\n    - name: a\n      image: nginx\n",
            )])))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn gitea_downloads_the_default_branch_with_the_token() {
        let server = MockServer::start().await;
        gitea_repo(&server, "main", "/api/v1/repos/me/flux/archive/main.tar.gz").await;
        let fetcher =
            ArchiveFetcher::gitea(server.uri().parse().unwrap(), Some(TOKEN.into())).unwrap();
        let got = fetcher.fetch("me/flux").await.unwrap();
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].0, "apps/web.yaml");
        assert!(!format!("{fetcher:?}").contains(TOKEN));
    }

    #[tokio::test]
    async fn gitea_branches_with_slashes_and_servers_under_a_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gitea/api/v1/repos/me/flux"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"default_branch": "release/2 x"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/gitea/api/v1/repos/me/flux/archive/release/2%20x.tar.gz",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[])))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/gitea/", server.uri());
        let fetcher = ArchiveFetcher::gitea(base.parse().unwrap(), None).unwrap();
        assert!(fetcher.fetch("me/flux").await.unwrap().files.is_empty());
    }

    #[tokio::test]
    async fn gitea_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/empty"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"default_branch": ""})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/junk"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/private"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let fetcher = ArchiveFetcher::gitea(server.uri().parse().unwrap(), None).unwrap();
        assert_eq!(fetcher.fetch("me/gone").await, Err(FetchError::NotFound));
        assert!(
            matches!(fetcher.fetch("me/empty").await, Err(FetchError::Http(m)) if m.contains("default branch"))
        );
        assert!(matches!(
            fetcher.fetch("me/junk").await,
            Err(FetchError::Http(_))
        ));
        assert_eq!(
            fetcher.fetch("me/private").await,
            Err(FetchError::Denied(StatusCode::UNAUTHORIZED))
        );
    }

    // --- GitHub ---

    /// Matches requests that carry no credentials.
    struct NoAuthorization;

    impl wiremock::Match for NoAuthorization {
        fn matches(&self, request: &wiremock::Request) -> bool {
            !request.headers.contains_key("authorization")
        }
    }

    fn host_of(server: &MockServer) -> String {
        server.address().to_string()
    }

    #[tokio::test]
    async fn github_follows_its_download_redirect_without_the_token() {
        let (api, codeload) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("GET"))
            .and(path("/repos/me/infra/tarball"))
            .and(header("authorization", "Bearer T0K"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                format!(
                    "{}/me/infra/legacy.tar.gz/refs/heads/main?token=abc",
                    codeload.uri()
                ),
            ))
            .expect(1)
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/me/infra/legacy.tar.gz/refs/heads/main"))
            .and(NoAuthorization)
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive(&[("compose.yaml", b"services: {}\n")])),
            )
            .expect(1)
            .mount(&codeload)
            .await;
        let fetcher = ArchiveFetcher::github(api.uri().parse().unwrap(), Some(TOKEN.into()))
            .unwrap()
            .allowing_insecure_redirect_to(&host_of(&codeload));
        let got = fetcher.fetch("me/infra").await.unwrap();
        assert_eq!(got.files.len(), 1);
    }

    #[tokio::test]
    async fn github_redirects_to_other_hosts_are_refused() {
        let (api, elsewhere) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("GET"))
            .and(path("/repos/me/infra/tarball"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/steal", elsewhere.uri())),
            )
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&elsewhere)
            .await;
        let fetcher =
            ArchiveFetcher::github(api.uri().parse().unwrap(), Some(TOKEN.into())).unwrap();
        assert!(matches!(
            fetcher.fetch("me/infra").await,
            Err(FetchError::Redirect(_))
        ));
        // Plain http is refused even for the right host, unless a test allows it.
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/me/infra/tarball"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "http://codeload.github.com/x"),
            )
            .mount(&api)
            .await;
        let fetcher = ArchiveFetcher::github(api.uri().parse().unwrap(), None).unwrap();
        assert!(matches!(
            fetcher.fetch("me/infra").await,
            Err(FetchError::Redirect(_))
        ));
    }

    #[tokio::test]
    async fn github_rate_limits_and_missing_repositories() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/me/busy/tarball"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/me/forbidden/tarball"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/me/limited/tarball"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let fetcher = ArchiveFetcher::github(server.uri().parse().unwrap(), None).unwrap();
        assert_eq!(fetcher.fetch("me/busy").await, Err(FetchError::RateLimited));
        assert_eq!(
            fetcher.fetch("me/limited").await,
            Err(FetchError::RateLimited)
        );
        assert_eq!(
            fetcher.fetch("me/forbidden").await,
            Err(FetchError::Denied(StatusCode::FORBIDDEN))
        );
        assert_eq!(fetcher.fetch("me/gone").await, Err(FetchError::NotFound));
    }

    #[tokio::test]
    async fn an_api_that_serves_the_archive_directly_is_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/me/infra/tarball"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(archive(&[("a.yaml", b"a: 1\n")])),
            )
            .mount(&server)
            .await;
        let fetcher = ArchiveFetcher::github(server.uri().parse().unwrap(), None).unwrap();
        assert_eq!(fetcher.fetch("me/infra").await.unwrap().files.len(), 1);
    }

    #[test]
    fn path_encoding() {
        assert_eq!(encode_segment("group/sub proj"), "group%2Fsub%20proj");
        assert_eq!(encode_path("release/2 x"), "release/2%20x");
        let base: Url = "https://git.example.com/sub/".parse().unwrap();
        assert_eq!(
            join(&base, "api/v1").as_str(),
            "https://git.example.com/sub/api/v1"
        );
    }
}
