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
    #[error("request failed: {0}")]
    Http(String),
    #[error("the archive is larger than {0} bytes")]
    TooLarge(u64),
    #[error("could not read the archive: {0}")]
    Archive(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// (path, content) of each YAML file, paths relative to the repository root.
    pub files: Vec<(String, String)>,
    /// More YAML files than the limit: the rest were not read.
    pub truncated: bool,
}

#[derive(Clone)]
pub struct GitlabFetcher {
    http: Client,
    base: Url,
    token: Option<String>,
    limits: Limits,
}

// The token must never show up in logs.
impl std::fmt::Debug for GitlabFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitlabFetcher")
            .field("base", &self.base.as_str())
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl GitlabFetcher {
    /// `base` is the GitLab instance, e.g. `https://gitlab.com`.
    pub fn new(base: Url, token: Option<String>) -> Result<GitlabFetcher, reqwest::Error> {
        Ok(GitlabFetcher {
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                // The token is only for this host: do not follow a redirect
                // anywhere else with it.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base,
            token: token.filter(|t| !t.trim().is_empty()),
            limits: Limits::default(),
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> GitlabFetcher {
        self.limits = limits;
        self
    }

    /// The archive URL of the default branch of `repo` (`group/sub/project`).
    fn archive_url(&self, repo: &str) -> Url {
        let mut url = self.base.clone();
        let encoded: String = url::form_urlencoded::byte_serialize(repo.as_bytes()).collect();
        // `byte_serialize` writes spaces as '+', and the path form needs %2F.
        let encoded = encoded.replace('+', "%20");
        url.set_path(&format!(
            "{}/api/v4/projects/{encoded}/repository/archive.tar.gz",
            self.base.path().trim_end_matches('/')
        ));
        url
    }

    pub async fn fetch(&self, repo: &str) -> Result<Fetched, FetchError> {
        let mut request = self.http.get(self.archive_url(repo));
        if let Some(token) = &self.token {
            request = request.header("PRIVATE-TOKEN", token);
        }
        let mut response = request.send().await.map_err(|e| {
            FetchError::Http(if e.is_timeout() {
                "timed out".into()
            } else if e.is_connect() {
                "could not connect".into()
            } else {
                "request error".into()
            })
        })?;
        match response.status() {
            s if s.is_success() => {}
            StatusCode::NOT_FOUND => return Err(FetchError::NotFound),
            s @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                // GitLab answers 404 for private projects it will not reveal,
                // but a bad token is a 401.
                return Err(FetchError::Denied(s));
            }
            s => return Err(FetchError::Http(format!("HTTP {s}"))),
        }
        if response
            .content_length()
            .is_some_and(|n| n > self.limits.download)
        {
            return Err(FetchError::TooLarge(self.limits.download));
        }
        let mut archive = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| FetchError::Http("the download was interrupted".into()))?
        {
            if archive.len() as u64 + chunk.len() as u64 > self.limits.download {
                return Err(FetchError::TooLarge(self.limits.download));
            }
            archive.extend_from_slice(&chunk);
        }
        let limits = self.limits;
        tokio::task::spawn_blocking(move || read_archive(&archive, limits))
            .await
            .map_err(|e| FetchError::Archive(e.to_string()))?
    }
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
        let fetcher = GitlabFetcher::new(server.uri().parse().unwrap(), None).unwrap();
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
            GitlabFetcher::new(server.uri().parse().unwrap(), Some("s3cret".into())).unwrap();
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
        let fetcher = GitlabFetcher::new(server.uri().parse().unwrap(), None).unwrap();
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
        let fetcher = GitlabFetcher::new(server.uri().parse().unwrap(), None)
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
        let fetcher = GitlabFetcher::new(server.uri().parse().unwrap(), Some("t".into())).unwrap();
        assert!(matches!(
            fetcher.fetch("me/moved").await,
            Err(FetchError::Http(_))
        ));
    }
}
