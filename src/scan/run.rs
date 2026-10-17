//! Runs scans: fetch a source's repository, find its images, ask registries
//! where they come from, and rewrite the usage that results.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tracing::{info, warn};

use super::fetch::ArchiveFetcher;
use super::image::ImageRef;
use super::manifests::scan_files;
use super::registry::{LookupError, RegistryClient};
use super::resolve::canonical_repo;
use super::usage::Lookup;
use crate::model::{Provider, ScanMethod, Source};
use crate::store::{Store, StoreError};

/// How long a registry answer is trusted: a found label for a month, anything
/// else (no label, a failure) for a day.
const LABEL_FRESH_FOR: SignedDuration = SignedDuration::from_hours(30 * 24);
const RETRY_AFTER: SignedDuration = SignedDuration::from_hours(24);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub images: usize,
    pub projects: usize,
    pub unresolved: usize,
    pub skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("no such source")]
    NotFound,
    #[error("this source is already being scanned")]
    AlreadyRunning,
    /// The scan could not run; the reason is also saved as the source's status.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct Scanner {
    store: Arc<Store>,
    fetchers: HashMap<Provider, ArchiveFetcher>,
    registry: RegistryClient,
    running: Mutex<HashSet<i64>>,
}

/// Marks a source as being scanned until dropped, so two scans of it never overlap.
struct Running<'a> {
    scanner: &'a Scanner,
    id: i64,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.scanner.running.lock() {
            set.remove(&self.id);
        }
    }
}

impl Scanner {
    /// `fetchers` has one entry per provider this server can read.
    pub fn new(
        store: Arc<Store>,
        fetchers: Vec<(Provider, ArchiveFetcher)>,
        registry: RegistryClient,
    ) -> Scanner {
        Scanner {
            store,
            fetchers: fetchers.into_iter().collect(),
            registry,
            running: Mutex::new(HashSet::new()),
        }
    }

    pub fn is_running(&self, id: i64) -> bool {
        self.running.lock().is_ok_and(|set| set.contains(&id))
    }

    /// Whether a source can be scanned by this server, or why not.
    pub fn can_scan(&self, source: &Source) -> Result<(), String> {
        if !matches!(source.scan_method, ScanMethod::Auto | ScanMethod::Images) {
            return Err(format!(
                "the {} scan is not available yet; only image scans are",
                source.scan_method.as_str()
            ));
        }
        if self.fetchers.contains_key(&source.provider) {
            return Ok(());
        }
        Err(match source.provider {
            Provider::Github => "GitHub is not configured",
            Provider::Gitlab => "GitLab is not configured",
            Provider::Gitea => "no Gitea or Forgejo server is configured (--gitea-url)",
        }
        .into())
    }

    /// Scans one source now. The outcome, good or bad, is saved on the source.
    pub async fn scan_source(&self, id: i64) -> Result<ScanSummary, ScanError> {
        let source = self
            .store
            .get_source(id)
            .await?
            .ok_or(ScanError::NotFound)?;
        {
            let mut set = self
                .running
                .lock()
                .expect("the scan lock is never poisoned");
            if !set.insert(id) {
                return Err(ScanError::AlreadyRunning);
            }
        }
        let _running = Running { scanner: self, id };
        let now = Timestamp::now();
        match self.run(&source, now).await {
            Ok(summary) => {
                let mut status =
                    format!("{} images, {} projects", summary.images, summary.projects);
                if summary.unresolved > 0 {
                    status.push_str(&format!(", {} unresolved", summary.unresolved));
                }
                if summary.skipped > 0 {
                    status.push_str(&format!(", {} skipped", summary.skipped));
                }
                self.store.finish_scan(id, now, true, &status).await?;
                info!(source = id, repo = %source.repo, %status, "scan finished");
                Ok(summary)
            }
            Err(ScanError::Failed(reason)) => {
                self.store.finish_scan(id, now, false, &reason).await?;
                warn!(source = id, repo = %source.repo, %reason, "scan failed");
                Err(ScanError::Failed(reason))
            }
            Err(other) => Err(other),
        }
    }

    async fn run(&self, source: &Source, now: Timestamp) -> Result<ScanSummary, ScanError> {
        if !source.enabled {
            return Err(ScanError::Failed("the source is disabled".into()));
        }
        self.can_scan(source).map_err(ScanError::Failed)?;
        let fetcher = self
            .fetchers
            .get(&source.provider)
            .expect("can_scan checked the fetcher");
        let fetched = fetcher
            .fetch(&source.repo)
            .await
            .map_err(|e| ScanError::Failed(format!("fetching the repository failed: {e}")))?;
        let found = scan_files(fetched.files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
        self.store
            .store_scan(source.id, &found.images, &found.skipped)
            .await?;

        self.look_up_images(now).await?;
        let usage = self.store.rebuild_image_usage(now).await?;
        let images = self
            .store
            .image_report(source.id)
            .await?
            .iter()
            .filter(|i| i.resolved.project.is_none())
            .count();
        Ok(ScanSummary {
            images: found
                .images
                .iter()
                .map(|u| (&u.image.registry, &u.image.repository))
                .collect::<HashSet<_>>()
                .len(),
            projects: usage.projects,
            unresolved: images,
            skipped: found.skipped.len(),
        })
    }

    /// Asks the registries about images that have no recent answer.
    pub async fn look_up_images(&self, now: Timestamp) -> Result<usize, StoreError> {
        let targets = self
            .store
            .images_to_look_up(now, LABEL_FRESH_FOR, RETRY_AFTER)
            .await?;
        // Registries that are rate limiting us: asking again now would only
        // make it worse, so the rest of their images wait for the next scan.
        let mut limited: HashSet<String> = HashSet::new();
        for target in &targets {
            if limited.contains(&target.registry) {
                let lookup = Lookup {
                    upstream: None,
                    label: None,
                    error: Some(LookupError::RateLimited.to_string()),
                    retry: true,
                };
                self.store
                    .save_lookup(&target.registry, &target.repository, &lookup, now)
                    .await?;
                continue;
            }
            let image = ImageRef {
                registry: target.registry.clone(),
                repository: target.repository.clone(),
                tag: target.tag.clone(),
                digest: target.digest.clone(),
            };
            let lookup = match self.registry.source_label(&image).await {
                Ok(Some(label)) => match canonical_repo(&label) {
                    Some(upstream) => Lookup {
                        upstream: Some(upstream),
                        label: Some(label),
                        error: None,
                        retry: false,
                    },
                    None => Lookup {
                        upstream: None,
                        label: Some(label),
                        error: Some("the source label is not a repository URL".into()),
                        retry: false,
                    },
                },
                Ok(None) => Lookup {
                    upstream: None,
                    label: None,
                    error: None,
                    retry: false,
                },
                Err(e) => {
                    let retry = e == LookupError::RateLimited;
                    if retry {
                        warn!(registry = %target.registry, "the registry is rate limiting lookups; the rest wait for the next scan");
                        limited.insert(target.registry.clone());
                    }
                    Lookup {
                        upstream: None,
                        label: None,
                        error: Some(e.to_string()),
                        retry,
                    }
                }
            };
            self.store
                .save_lookup(&target.registry, &target.repository, &lookup, now)
                .await?;
        }
        Ok(targets.len())
    }

    /// Scans every enabled source that can be scanned, one after the other.
    pub async fn scan_all(&self) {
        let sources = match self.store.list_sources().await {
            Ok(sources) => sources,
            Err(e) => {
                warn!(error = %e, "listing sources for the scheduled scan failed");
                return;
            }
        };
        for source in sources
            .iter()
            .filter(|s| s.enabled && self.can_scan(s).is_ok())
        {
            // Failures are saved on the source and logged by `scan_source`.
            let _ = self.scan_source(source.id).await;
        }
    }
}

/// Scans all sources every `every`, starting shortly after the server starts.
pub fn spawn_scheduler(scanner: Arc<Scanner>, every: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            scanner.scan_all().await;
            tokio::time::sleep(every).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, NewSource};
    use crate::scan::fetch::testing::archive;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const COMPOSE: &str = r#"
services:
  jellyfin:
    image: jellyfin/jellyfin:10.9
  sonarr:
    image: lscr.io/linuxserver/sonarr:${SONARR_TAG:-latest}
  vpn:
    image: qmcgaw/gluetun
  paperless:
    image: ghcr.io/paperless-ngx/paperless-ngx:2.13
  proxy:
    image: ${PROXY_IMAGE}
"#;

    struct Setup {
        store: Arc<Store>,
        scanner: Scanner,
        gitlab: MockServer,
        registry: MockServer,
        id: i64,
    }

    async fn gitlab_repo(server: &MockServer, repo_path: &str, files: &[(&str, &[u8])]) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v4/projects/{repo_path}/repository/archive.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(files)))
            .mount(server)
            .await;
    }

    async fn image_label(server: &MockServer, repo: &str, tag: &str, label: Option<&str>) {
        let digest = format!("sha256:{}", repo.replace('/', "-"));
        Mock::given(method("GET"))
            .and(path(format!("/v2/{repo}/manifests/{tag}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"config": {"digest": digest}})),
            )
            .mount(server)
            .await;
        let labels = match label {
            Some(l) => json!({"org.opencontainers.image.source": l}),
            None => json!({}),
        };
        Mock::given(method("GET"))
            .and(path(format!("/v2/{repo}/blobs/{digest}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"config": {"Labels": labels}})),
            )
            .mount(server)
            .await;
    }

    async fn setup() -> Setup {
        let store = Arc::new(Store::memory().await.unwrap());
        let (gitlab, registry) = (MockServer::start().await, MockServer::start().await);
        let id = store
            .create_source(
                &NewSource {
                    provider: Provider::Gitlab,
                    repo: "me/media-server".into(),
                    weight: 1.0,
                    scan_method: ScanMethod::Images,
                },
                &Actor::new("u1", "alice"),
                "2026-09-26T12:00:00Z".parse().unwrap(),
            )
            .await
            .unwrap()
            .id;
        let scanner = Scanner::new(
            store.clone(),
            vec![(
                Provider::Gitlab,
                ArchiveFetcher::gitlab(gitlab.uri().parse().unwrap(), None).unwrap(),
            )],
            RegistryClient::for_tests(&registry.uri()),
        );
        Setup {
            store,
            scanner,
            gitlab,
            registry,
            id,
        }
    }

    async fn names(store: &Store) -> Vec<String> {
        store
            .list_projects()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    #[tokio::test]
    async fn scans_a_compose_project_end_to_end() {
        let s = setup().await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[("compose.yaml", COMPOSE.as_bytes())],
        )
        .await;
        image_label(
            &s.registry,
            "jellyfin/jellyfin",
            "10.9",
            Some("https://github.com/jellyfin/jellyfin"),
        )
        .await;
        image_label(
            &s.registry,
            "linuxserver/sonarr",
            "latest",
            Some("https://github.com/linuxserver/docker-sonarr"),
        )
        .await;
        image_label(&s.registry, "paperless-ngx/paperless-ngx", "2.13", None).await;
        // gluetun is not served: its lookup fails with "not found".

        let summary = s.scanner.scan_source(s.id).await.unwrap();
        assert_eq!(
            summary,
            ScanSummary {
                images: 4,
                projects: 3,
                unresolved: 1,
                skipped: 1
            }
        );
        assert_eq!(
            names(&s.store).await,
            [
                "jellyfin-jellyfin",
                "linuxserver-docker-sonarr",
                // no label, so guessed from the ghcr.io name
                "paperless-ngx-paperless-ngx"
            ]
        );
        let source = s.store.get_source(s.id).await.unwrap().unwrap();
        assert_eq!(source.last_scan_ok, Some(true));
        assert_eq!(
            source.last_scan_status.as_deref(),
            Some("4 images, 3 projects, 1 unresolved, 1 skipped")
        );

        let report = s.store.image_report(s.id).await.unwrap();
        let gluetun = report
            .iter()
            .find(|r| r.repository == "qmcgaw/gluetun")
            .unwrap();
        assert_eq!(gluetun.resolved.project, None);
        assert_eq!(
            gluetun.resolved.note.as_deref(),
            Some("image not found (it may be private)")
        );
        let paperless = report
            .iter()
            .find(|r| r.repository == "paperless-ngx/paperless-ngx")
            .unwrap();
        assert_eq!(
            paperless.resolved.project.as_deref(),
            Some("github.com/paperless-ngx/paperless-ngx")
        );
        assert_eq!(paperless.resolved.how, Some(crate::scan::usage::How::Guess));
        let skipped = s.store.skipped_images(s.id).await.unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].value, "${PROXY_IMAGE}");

        // Usage counts the source's weight, once per project.
        let projects = s.store.list_projects().await.unwrap();
        assert!(projects.iter().all(|p| (p.usage_score - 1.0).abs() < 1e-9));
    }

    #[tokio::test]
    async fn rescans_reuse_cached_lookups() {
        let s = setup().await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[(
                "compose.yaml",
                b"services:\n  a:\n    image: jellyfin/jellyfin:10.9\n",
            )],
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/v2/jellyfin/jellyfin/manifests/10.9"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"config": {"digest": "sha256:c"}})),
            )
            .expect(1)
            .mount(&s.registry)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/jellyfin/jellyfin/blobs/sha256:c"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"config": {"Labels": {"org.opencontainers.image.source": "https://github.com/jellyfin/jellyfin"}}})))
            .expect(1)
            .mount(&s.registry)
            .await;
        s.scanner.scan_source(s.id).await.unwrap();
        s.scanner.scan_source(s.id).await.unwrap();
        assert_eq!(names(&s.store).await, ["jellyfin-jellyfin"]);
        // `expect(1)` is verified when the mock server is dropped.
    }

    #[tokio::test]
    async fn a_rate_limited_registry_is_not_asked_again_until_the_next_scan() {
        let s = setup().await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[("compose.yaml", b"services:\n  a:\n    image: aaa/one\n  b:\n    image: bbb/two\n  c:\n    image: ccc/three\n")],
        )
        .await;
        // The first lookup is refused; the others must not even be tried.
        Mock::given(method("GET"))
            .and(path("/v2/aaa/one/manifests/latest"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&s.registry)
            .await;
        for repo in ["bbb/two", "ccc/three"] {
            Mock::given(method("GET"))
                .and(path(format!("/v2/{repo}/manifests/latest")))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&s.registry)
                .await;
        }
        s.scanner.scan_source(s.id).await.unwrap();
        let report = s.store.image_report(s.id).await.unwrap();
        assert_eq!(report.len(), 3);
        for r in &report {
            assert_eq!(r.resolved.project, None);
            assert!(
                r.resolved
                    .note
                    .as_deref()
                    .unwrap()
                    .contains("rate limiting"),
                "{r:?}"
            );
        }

        // The next scan asks again, however recent the answer is.
        s.registry.reset().await;
        for (repo, label) in [
            ("aaa/one", "https://github.com/aaa/one"),
            ("bbb/two", "https://github.com/bbb/two"),
            ("ccc/three", "https://github.com/ccc/three"),
        ] {
            image_label(&s.registry, repo, "latest", Some(label)).await;
        }
        s.scanner.scan_source(s.id).await.unwrap();
        assert_eq!(names(&s.store).await, ["aaa-one", "bbb-two", "ccc-three"]);
    }

    #[tokio::test]
    async fn mappings_change_usage_without_a_new_scan() {
        let s = setup().await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[(
                "compose.yaml",
                b"services:\n  vpn:\n    image: qmcgaw/gluetun\n",
            )],
        )
        .await;
        s.scanner.scan_source(s.id).await.unwrap();
        assert!(
            names(&s.store).await.is_empty(),
            "unresolved image, no project"
        );

        s.store
            .set_mapping(
                "docker.io",
                "qmcgaw/gluetun",
                "github.com/qdm12/gluetun",
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        let summary = s.store.rebuild_image_usage(Timestamp::now()).await.unwrap();
        assert_eq!((summary.projects, summary.unresolved), (1, 0));
        assert_eq!(names(&s.store).await, ["qdm12-gluetun"]);
        let report = s.store.image_report(s.id).await.unwrap();
        assert_eq!(
            report[0].resolved.how,
            Some(crate::scan::usage::How::Mapping)
        );

        s.store
            .delete_mapping("docker.io", "qmcgaw/gluetun")
            .await
            .unwrap();
        s.store.rebuild_image_usage(Timestamp::now()).await.unwrap();
        assert!(names(&s.store).await.is_empty());
    }

    #[tokio::test]
    async fn removed_images_stop_counting_and_projects_are_pruned() {
        let s = setup().await;
        image_label(
            &s.registry,
            "a/one",
            "latest",
            Some("https://github.com/a/one"),
        )
        .await;
        image_label(
            &s.registry,
            "b/two",
            "latest",
            Some("https://github.com/b/two"),
        )
        .await;
        let both = b"services:\n  x:\n    image: a/one\n  y:\n    image: b/two\n";
        let only_one = b"services:\n  x:\n    image: a/one\n";
        // The archive is replaced between scans.
        gitlab_repo(&s.gitlab, "me%2Fmedia-server", &[("compose.yaml", both)]).await;
        s.scanner.scan_source(s.id).await.unwrap();
        assert_eq!(names(&s.store).await, ["a-one", "b-two"]);

        s.gitlab.reset().await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[("compose.yaml", only_one)],
        )
        .await;
        s.scanner.scan_source(s.id).await.unwrap();
        assert_eq!(names(&s.store).await, ["a-one"]);
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_previous_findings() {
        let s = setup().await;
        image_label(
            &s.registry,
            "a/one",
            "latest",
            Some("https://github.com/a/one"),
        )
        .await;
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[("compose.yaml", b"services:\n  x:\n    image: a/one\n")],
        )
        .await;
        s.scanner.scan_source(s.id).await.unwrap();

        s.gitlab.reset().await; // now every request is a 404
        let err = s.scanner.scan_source(s.id).await.unwrap_err();
        assert!(
            matches!(err, ScanError::Failed(ref m) if m.contains("not found")),
            "{err}"
        );
        let source = s.store.get_source(s.id).await.unwrap().unwrap();
        assert_eq!(source.last_scan_ok, Some(false));
        assert!(
            source
                .last_scan_status
                .unwrap()
                .contains("fetching the repository failed")
        );
        assert_eq!(
            names(&s.store).await,
            ["a-one"],
            "what was found before still counts"
        );
    }

    /// A scanner that can read all three forges, each from its own mock.
    async fn all_forges() -> (
        Arc<Store>,
        Scanner,
        MockServer,
        MockServer,
        MockServer,
        MockServer,
    ) {
        let store = Arc::new(Store::memory().await.unwrap());
        let (gitlab, gitea, github, registry) = (
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
        );
        let scanner = Scanner::new(
            store.clone(),
            vec![
                (
                    Provider::Gitlab,
                    ArchiveFetcher::gitlab(gitlab.uri().parse().unwrap(), None).unwrap(),
                ),
                (
                    Provider::Gitea,
                    ArchiveFetcher::gitea(gitea.uri().parse().unwrap(), Some("t".into())).unwrap(),
                ),
                (
                    Provider::Github,
                    ArchiveFetcher::github(github.uri().parse().unwrap(), None).unwrap(),
                ),
            ],
            RegistryClient::for_tests(&registry.uri()),
        );
        (store, scanner, gitlab, gitea, github, registry)
    }

    async fn add_source(store: &Store, provider: Provider, repo: &str) -> i64 {
        store
            .create_source(
                &NewSource {
                    provider,
                    repo: repo.into(),
                    weight: 1.0,
                    scan_method: ScanMethod::Images,
                },
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn gitea_and_github_sources_are_scanned_too() {
        let (store, scanner, _gitlab, gitea, github, registry) = all_forges().await;
        image_label(
            &registry,
            "linuxserver/sonarr",
            "latest",
            Some("https://github.com/linuxserver/docker-sonarr"),
        )
        .await;
        image_label(
            &registry,
            "grafana/grafana",
            "11",
            Some("https://github.com/grafana/grafana"),
        )
        .await;

        // Gitea: the default branch first, then its archive.
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/flux"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"default_branch": "main"})),
            )
            .mount(&gitea)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/me/flux/archive/main.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[(
                "apps/sonarr/helmrelease.yaml",
                b"apiVersion: helm.toolkit.fluxcd.io/v2\nkind: HelmRelease\nmetadata: {name: sonarr}\nspec:\n  values:\n    image:\n      repository: lscr.io/linuxserver/sonarr\n      tag: latest\n",
            )])))
            .mount(&gitea)
            .await;
        // GitHub answers directly here (the redirect is covered by the fetcher's tests).
        Mock::given(method("GET"))
            .and(path("/repos/me/infra/tarball"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive(&[(
                "compose.yaml",
                b"services:\n  g:\n    image: grafana/grafana:11\n",
            )])))
            .mount(&github)
            .await;

        let flux = add_source(&store, Provider::Gitea, "me/flux").await;
        let infra = add_source(&store, Provider::Github, "me/infra").await;
        scanner.scan_source(flux).await.unwrap();
        scanner.scan_source(infra).await.unwrap();
        assert_eq!(
            names(&store).await,
            ["grafana-grafana", "linuxserver-docker-sonarr"]
        );
        let flux = store.get_source(flux).await.unwrap().unwrap();
        assert_eq!(
            flux.last_scan_status.as_deref(),
            Some("1 images, 1 projects")
        );
    }

    #[tokio::test]
    async fn a_missing_server_is_reported_per_provider() {
        let s = setup().await; // only GitLab is configured
        let gitea = add_source(&s.store, Provider::Gitea, "me/flux").await;
        let err = s.scanner.scan_source(gitea).await.unwrap_err();
        assert!(err.to_string().contains("--gitea-url"), "{err}");
    }

    #[tokio::test]
    async fn unsupported_sources_say_why() {
        let s = setup().await;
        let github = s
            .store
            .create_source(
                &NewSource {
                    provider: Provider::Github,
                    repo: "me/infra".into(),
                    weight: 1.0,
                    scan_method: ScanMethod::Images,
                },
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        let err = s.scanner.scan_source(github.id).await.unwrap_err();
        assert!(err.to_string().contains("GitHub"), "{err}");
        let lockfiles = s
            .store
            .create_source(
                &NewSource {
                    provider: Provider::Gitlab,
                    repo: "me/libs".into(),
                    weight: 1.0,
                    scan_method: ScanMethod::Lockfiles,
                },
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        let err = s.scanner.scan_source(lockfiles.id).await.unwrap_err();
        assert!(err.to_string().contains("lockfiles"), "{err}");
        assert!(matches!(
            s.scanner.scan_source(9999).await,
            Err(ScanError::NotFound)
        ));
    }

    #[tokio::test]
    async fn disabled_sources_are_not_scanned_and_scans_do_not_overlap() {
        let s = setup().await;
        s.store
            .update_source(
                s.id,
                &crate::model::SourceUpdate {
                    weight: 1.0,
                    scan_method: ScanMethod::Images,
                    enabled: false,
                },
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        let err = s.scanner.scan_source(s.id).await.unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");

        // While a scan holds the source, a second one is refused.
        s.scanner.running.lock().unwrap().insert(s.id);
        assert!(s.scanner.is_running(s.id));
        assert!(matches!(
            s.scanner.scan_source(s.id).await,
            Err(ScanError::AlreadyRunning)
        ));
        s.scanner.running.lock().unwrap().remove(&s.id);
        assert!(!s.scanner.is_running(s.id));
    }

    #[tokio::test]
    async fn scan_all_skips_what_cannot_be_scanned() {
        let s = setup().await;
        s.store
            .create_source(
                &NewSource {
                    provider: Provider::Github,
                    repo: "me/infra".into(),
                    weight: 1.0,
                    scan_method: ScanMethod::Images,
                },
                &Actor::new("u1", "alice"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        gitlab_repo(
            &s.gitlab,
            "me%2Fmedia-server",
            &[("compose.yaml", b"services: {}\n")],
        )
        .await;
        s.scanner.scan_all().await;
        let sources = s.store.list_sources().await.unwrap();
        let gitlab = sources
            .iter()
            .find(|x| x.provider == Provider::Gitlab)
            .unwrap();
        let github = sources
            .iter()
            .find(|x| x.provider == Provider::Github)
            .unwrap();
        assert_eq!(gitlab.last_scan_ok, Some(true));
        assert_eq!(
            github.last_scan_ok, None,
            "an unsupported source is left alone"
        );
    }
}
