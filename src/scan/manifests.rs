//! Finds container image references in the YAML files of a repository:
//! Docker Compose files, Kubernetes manifests (including Flux resources such as
//! HelmReleases and Kustomizations) and Helm `values` files.
//!
//! Only what is written in the files is read. Helm templates are not rendered,
//! so an image a chart picks by itself is not seen.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};

use super::image::{ImageError, ImageRef};

/// Where an image reference was found.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImageUse {
    pub image: ImageRef,
    pub file: String,
    /// What in the file refers to it, e.g. `compose service sonarr` or
    /// `Deployment/web container app`.
    pub origin: String,
}

/// A reference that was found but could not be used.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Skipped {
    pub file: String,
    pub value: String,
    pub reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Findings {
    pub images: Vec<ImageUse>,
    pub skipped: Vec<Skipped>,
}

/// Kinds whose `image` keys are not container images.
const IGNORED_KINDS: [&str; 3] = ["ConfigMap", "Secret", "CustomResourceDefinition"];

/// True for the files worth parsing: YAML, outside VCS and dependency folders.
pub fn is_candidate(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    (lower.ends_with(".yaml") || lower.ends_with(".yml"))
        && !path
            .split('/')
            .any(|part| matches!(part, ".git" | "node_modules" | "vendor"))
}

/// Scans many files and returns the images found, each (image, file, origin)
/// once, in a stable order.
pub fn scan_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Findings {
    let mut images = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    for (path, content) in files {
        if !is_candidate(path) {
            continue;
        }
        let found = scan_file(path, content);
        images.extend(found.images);
        skipped.extend(found.skipped);
    }
    Findings {
        images: images.into_iter().collect(),
        skipped: skipped.into_iter().collect(),
    }
}

pub fn scan_file(path: &str, content: &str) -> Findings {
    let mut out = Collector {
        file: path,
        findings: Findings::default(),
    };
    // Cheap exit for the vast majority of YAML files.
    if !content.contains("image") && !content.contains("repository") {
        return out.findings;
    }
    let is_values_file = path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("values"));
    for document in serde_yaml_ng::Deserializer::from_str(content) {
        // A document that does not parse (Helm templates, for example) ends
        // the file: the rest cannot be trusted either.
        let Ok(value) = Value::deserialize(document) else {
            break;
        };
        let Some(root) = value.as_mapping() else {
            continue;
        };
        out.document(root, is_values_file);
    }
    out.findings
}

struct Collector<'a> {
    file: &'a str,
    findings: Findings,
}

fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(key)
}

fn text<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    get(map, key).and_then(Value::as_str)
}

impl Collector<'_> {
    fn document(&mut self, root: &Mapping, is_values_file: bool) {
        if let Some(kind) = text(root, "kind") {
            if IGNORED_KINDS.contains(&kind) {
                return;
            }
            let name = get(root, "metadata")
                .and_then(Value::as_mapping)
                .and_then(|m| text(m, "name"))
                .unwrap_or("?");
            let label = format!("{kind}/{name}");
            if kind == "Kustomization" {
                // kustomize.config.k8s.io has `images` at the top, Flux's
                // Kustomization has it under `spec`.
                if let Some(list) = get(root, "images") {
                    self.kustomize_images(list, &label);
                }
                if let Some(list) = get(root, "spec")
                    .and_then(Value::as_mapping)
                    .and_then(|spec| get(spec, "images"))
                {
                    self.kustomize_images(list, &label);
                }
            }
            self.walk(&Value::Mapping(root.clone()), &label);
        } else if get(root, "services").is_some_and(Value::is_mapping)
            && get(root, "apiVersion").is_none()
        {
            self.compose(root);
        } else if is_values_file {
            self.walk(&Value::Mapping(root.clone()), "Helm values");
        }
    }

    fn add(&mut self, raw: &str, origin: String) {
        match ImageRef::parse_lenient(raw) {
            Ok(image) => self.findings.images.push(ImageUse {
                image,
                file: self.file.to_string(),
                origin,
            }),
            Err(e) => self.skip(raw, e),
        }
    }

    fn skip(&mut self, raw: &str, error: ImageError) {
        // An empty value is not a reference that went wrong.
        if matches!(error, ImageError::Empty) {
            return;
        }
        self.findings.skipped.push(Skipped {
            file: self.file.to_string(),
            value: raw.chars().take(200).collect(),
            reason: error.to_string(),
        });
    }

    fn compose(&mut self, root: &Mapping) {
        let Some(services) = get(root, "services").and_then(Value::as_mapping) else {
            return;
        };
        for (name, service) in services {
            let Some(name) = name.as_str() else { continue };
            let Some(image) = service.as_mapping().and_then(|s| text(s, "image")) else {
                continue; // built locally, or an extension field
            };
            self.add(image, format!("compose service {name}"));
        }
    }

    fn kustomize_images(&mut self, list: &Value, label: &str) {
        let Some(items) = list.as_sequence() else {
            return;
        };
        for item in items.iter().filter_map(Value::as_mapping) {
            let name = text(item, "newName").or_else(|| text(item, "name"));
            let (tag, digest) = (text(item, "newTag"), text(item, "digest"));
            // An entry that changes neither the name, tag nor digest only
            // names an image that is declared elsewhere.
            if text(item, "newName").is_none() && tag.is_none() && digest.is_none() {
                continue;
            }
            let Some(name) = name else { continue };
            let mut raw = name.to_string();
            if let Some(t) = tag {
                raw.push(':');
                raw.push_str(t);
            }
            if let Some(d) = digest {
                raw.push('@');
                raw.push_str(d);
            }
            self.add(&raw, format!("{label} images"));
        }
    }

    /// Looks through a whole document for container lists and `image` keys.
    fn walk(&mut self, value: &Value, label: &str) {
        match value {
            Value::Mapping(map) => {
                for (key, child) in map {
                    let key = key.as_str().unwrap_or_default();
                    match (key, child) {
                        (
                            "containers" | "initContainers" | "ephemeralContainers",
                            Value::Sequence(items),
                        ) => {
                            for item in items.iter().filter_map(Value::as_mapping) {
                                if let Some(image) = text(item, "image") {
                                    let name = text(item, "name").unwrap_or("?");
                                    self.add(image, format!("{label} container {name}"));
                                }
                            }
                        }
                        ("image", Value::String(raw)) => {
                            self.add(raw, format!("{label} image"));
                        }
                        ("image", Value::Mapping(spec)) => {
                            if let Some(raw) = chart_image(spec) {
                                self.add(&raw, format!("{label} image values"));
                            }
                            self.walk(child, label);
                        }
                        _ => self.walk(child, label),
                    }
                }
            }
            Value::Sequence(items) => {
                for item in items {
                    self.walk(item, label);
                }
            }
            _ => {}
        }
    }
}

/// An `image:` block as most Helm charts write it:
/// `{registry?, repository, tag?, digest?}`.
fn chart_image(spec: &Mapping) -> Option<String> {
    let repository = text(spec, "repository")?;
    let mut raw = String::new();
    if let Some(registry) = text(spec, "registry").filter(|r| !r.is_empty()) {
        raw.push_str(registry.trim_end_matches('/'));
        raw.push('/');
    }
    raw.push_str(repository);
    // Charts write tags as strings or as numbers (`tag: 1.2`).
    let tag = match get(spec, "tag") {
        Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    if let Some(t) = tag {
        raw.push(':');
        raw.push_str(&t);
    }
    if let Some(d) = text(spec, "digest").filter(|d| !d.is_empty()) {
        raw.push('@');
        raw.push_str(d);
    }
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn images(path: &str, content: &str) -> Vec<(String, String)> {
        scan_file(path, content)
            .images
            .into_iter()
            .map(|u| (u.image.to_string(), u.origin))
            .collect()
    }

    fn names(path: &str, content: &str) -> Vec<String> {
        let mut v: Vec<String> = images(path, content).into_iter().map(|(i, _)| i).collect();
        v.sort();
        v
    }

    const MEDIA_SERVER: &str = r#"
name: media
x-common: &common
  restart: unless-stopped
  environment:
    - PUID=1000
services:
  gluetun:
    image: qmcgaw/gluetun
    cap_add: [NET_ADMIN]
  sonarr:
    <<: *common
    image: lscr.io/linuxserver/sonarr:${SONARR_TAG:-latest}
    network_mode: service:gluetun
  jellyfin:
    image: "jellyfin/jellyfin:10.9.11"
  radarr:
    image: lscr.io/linuxserver/radarr:${RADARR_TAG}
  custom:
    build: ./custom
  proxy:
    image: ${PROXY_IMAGE}
  db:
    image: postgres:16-alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"#;

    #[test]
    fn docker_compose_media_server() {
        let found = scan_file("media-server/compose.yaml", MEDIA_SERVER);
        let mut got: Vec<String> = found.images.iter().map(|u| u.image.to_string()).collect();
        got.sort();
        assert_eq!(
            got,
            [
                "docker.io/jellyfin/jellyfin:10.9.11",
                "docker.io/library/postgres:16-alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "docker.io/qmcgaw/gluetun",
                "lscr.io/linuxserver/radarr",
                "lscr.io/linuxserver/sonarr:latest",
            ]
        );
        assert!(
            found
                .images
                .iter()
                .any(|u| u.origin == "compose service sonarr")
        );
        // Only the one whose name depends on a variable is unusable.
        assert_eq!(found.skipped.len(), 1);
        assert_eq!(found.skipped[0].value, "${PROXY_IMAGE}");
        assert!(found.skipped[0].reason.contains("variable"));
    }

    #[test]
    fn compose_is_recognised_by_content_not_file_name() {
        for name in [
            "docker-compose.yml",
            "compose.override.yaml",
            "stacks/arr/stack.yml",
        ] {
            assert_eq!(
                names(
                    name,
                    "services:
  a:
    image: nginx
"
                ),
                ["docker.io/library/nginx"],
                "{name}"
            );
        }
    }

    const DEPLOYMENT: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      initContainers:
        - name: migrate
          image: ghcr.io/example/web-migrate:v1
      containers:
        - name: app
          image: ghcr.io/example/web:v1.4.2
          env:
            - name: IMAGE
              value: not-an-image
        - name: sidecar
          image: quay.io/oauth2-proxy/oauth2-proxy:v7.6.0
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: backup
spec:
  jobTemplate:
    spec:
      template:
        spec:
          containers:
            - name: restic
              image: restic/restic:0.17.1
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
data:
  image: this-is-data-not-an-image
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  ports: [{port: 80}]
"#;

    #[test]
    fn kubernetes_workloads() {
        let found = images("apps/web.yaml", DEPLOYMENT);
        let mut got: Vec<&str> = found.iter().map(|(i, _)| i.as_str()).collect();
        got.sort();
        assert_eq!(
            got,
            [
                "docker.io/restic/restic:0.17.1",
                "ghcr.io/example/web-migrate:v1",
                "ghcr.io/example/web:v1.4.2",
                "quay.io/oauth2-proxy/oauth2-proxy:v7.6.0",
            ]
        );
        assert!(
            found
                .iter()
                .any(|(_, o)| o == "Deployment/web container app")
        );
        assert!(
            found
                .iter()
                .any(|(_, o)| o == "CronJob/backup container restic")
        );
    }

    const FLUX: &str = r#"
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: paperless
  namespace: apps
spec:
  chart:
    spec:
      chart: app-template
      sourceRef: {kind: HelmRepository, name: bjw-s}
  values:
    controllers:
      main:
        containers:
          main:
            image:
              repository: ghcr.io/paperless-ngx/paperless-ngx
              tag: 2.13.5
          redis:
            image:
              repository: docker.io/library/redis
              tag: "7.4"
    image: {registry: quay.io, repository: example/helper, digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
    sidecar:
      image: busybox:1.36
---
apiVersion: kustomize.toolkit.fluxcd.io/v1
kind: Kustomization
metadata:
  name: apps
spec:
  path: ./apps
  images:
    - name: ghcr.io/example/web
      newTag: v2.0.0
    - name: nginx
      newName: registry.example.com/mirror/nginx
      newTag: "1.27"
    - name: only-a-name
---
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
images:
  - name: alpine
    digest: sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"#;

    #[test]
    fn flux_helm_releases_and_kustomizations() {
        let got = names("clusters/main/apps/paperless.yaml", FLUX);
        assert_eq!(
            got,
            [
                "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "docker.io/library/busybox:1.36",
                "docker.io/library/redis:7.4",
                "ghcr.io/example/web:v2.0.0",
                "ghcr.io/paperless-ngx/paperless-ngx:2.13.5",
                "quay.io/example/helper@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "registry.example.com/mirror/nginx:1.27",
            ]
        );
    }

    #[test]
    fn helm_values_files_are_read_by_name() {
        let values = "image:
  repository: ghcr.io/o/app
  tag: 1.0
replicas: 2
";
        assert_eq!(
            names("charts/app/values.yaml", values),
            ["ghcr.io/o/app:1.0"]
        );
        assert_eq!(names("values-prod.yml", values), ["ghcr.io/o/app:1.0"]);
        // The same content elsewhere is just some YAML.
        assert!(names("config/settings.yaml", values).is_empty());
    }

    #[test]
    fn unrelated_and_broken_files_are_ignored() {
        assert!(
            names(
                "a.yaml",
                "name: x
version: 1
"
            )
            .is_empty()
        );
        // Helm templates are not YAML.
        assert!(
            names(
                "templates/deploy.yaml",
                "image: {{ .Values.image }}
{{- if x }}
"
            )
            .is_empty()
        );
        assert!(names("empty.yaml", "").is_empty());
        assert!(
            names(
                "list.yaml",
                "- image: nginx
"
            )
            .is_empty()
        );
        // A CRD describes an `image` property, it does not use one.
        let crd = "apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
spec:
  schema:
    properties:
      image:
        type: string
";
        assert!(names("crd.yaml", crd).is_empty());
    }

    #[test]
    fn candidates_and_scanning_many_files() {
        assert!(is_candidate("a/b/compose.yaml"));
        assert!(is_candidate("X.YML"));
        assert!(!is_candidate("README.md"));
        assert!(!is_candidate("node_modules/x/compose.yaml"));
        assert!(!is_candidate(".git/config.yaml"));
        assert!(!is_candidate("charts/vendor/x.yaml"));

        let both = [
            (
                "a/compose.yaml",
                "services:
  x:
    image: nginx
",
            ),
            (
                "b/compose.yaml",
                "services:
  x:
    image: nginx
",
            ),
            (
                "a/README.md",
                "services:
  x:
    image: ignored
",
            ),
        ];
        let found = scan_files(both.iter().map(|(p, c)| (*p, *c)));
        // The same image in two files is two uses.
        assert_eq!(found.images.len(), 2);
        let again = scan_files(both.iter().chain(both.iter()).map(|(p, c)| (*p, *c)));
        assert_eq!(again, found, "scanning is deterministic and de-duplicated");
    }
}
