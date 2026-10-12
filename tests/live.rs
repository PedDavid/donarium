//! Checks against the real registries and GitLab, which the mocked tests
//! cannot give: that the protocol handling matches what these services do.
//! They need network access, so they are ignored by default:
//!
//!     cargo test --test live -- --ignored --nocapture
//!
//! Registries rate limit anonymous requests (Docker Hub allows about 100
//! manifest pulls per six hours per address), so a lookup that is refused is
//! reported and skipped rather than failing the test.

use donarium::scan::fetch::GitlabFetcher;
use donarium::scan::image::ImageRef;
use donarium::scan::manifests::scan_files;
use donarium::scan::registry::{LookupError, RegistryClient};
use donarium::scan::resolve::canonical_repo;

#[tokio::test]
#[ignore = "needs network access"]
async fn live_registries_name_their_sources() {
    let client = RegistryClient::new(vec![]).unwrap();
    // (image, the project its label should name)
    let cases = [
        ("grafana/grafana:latest", "github.com/grafana/grafana"),
        (
            "jellyfin/jellyfin:10.10.7",
            "github.com/jellyfin/jellyfin-packaging",
        ),
        (
            "lscr.io/linuxserver/sonarr:latest",
            "github.com/linuxserver/docker-sonarr",
        ),
        (
            "ghcr.io/paperless-ngx/paperless-ngx:latest",
            "github.com/paperless-ngx/paperless-ngx",
        ),
    ];
    let mut checked = 0;
    for (image, expected) in cases {
        let image = ImageRef::parse(image).unwrap();
        match client.source_label(&image).await {
            Ok(label) => {
                let canonical = label.as_deref().and_then(canonical_repo);
                eprintln!("{image}: {label:?} -> {canonical:?}");
                assert_eq!(canonical.as_deref(), Some(expected), "{image}");
                checked += 1;
            }
            Err(e @ (LookupError::RateLimited | LookupError::Http(_))) => {
                eprintln!("{image}: skipped ({e})");
            }
            Err(e) => panic!("{image}: {e}"),
        }
    }
    assert!(checked > 0, "no registry answered at all");
}

#[tokio::test]
#[ignore = "needs network access"]
async fn live_gitlab_project_is_read_and_scanned() {
    // A small public Helm chart whose values.yaml has an `image:` block.
    let repo = std::env::var("DONARIUM_LIVE_GITLAB_REPO")
        .unwrap_or_else(|_| "gitlab-org/charts/auto-deploy-app".into());
    let fetcher = GitlabFetcher::new("https://gitlab.com".parse().unwrap(), None).unwrap();
    let fetched = fetcher.fetch(&repo).await.unwrap();
    assert!(!fetched.files.is_empty(), "{repo} has YAML files");
    let found = scan_files(fetched.files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    eprintln!("{repo}: {} yaml files", fetched.files.len());
    for u in &found.images {
        eprintln!("   {}   ({} · {})", u.image, u.file, u.origin);
    }
    assert!(
        found.images.iter().any(|u| u.file == "values.yaml"),
        "the chart's values.yaml image block is found"
    );
}
