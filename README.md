# donarium

A self-hosted, Kubernetes-native tracker for donations to the open-source
projects you depend on. It scans your own repositories' dependencies, ranks the
upstream projects by how much you use them and how long ago you last donated,
records each donation, and links it to its receipt (Paperless-ngx or S3).

It never moves money: you donate on the project's own page, then record it.

> **Status:** early. The web shell, OIDC login, metrics, Helm chart and the
> SQLite database are in place, and admins can manage **sources** (the
> repositories to scan) in the UI. Sources on GitLab, GitHub and Gitea/Forgejo
> can be scanned for the container images in their Docker Compose and
> Kubernetes (Flux) files.
> Dependency scans, ranking, donations and receipts are next. See [docs/PLAN.md](docs/PLAN.md) for the full design and
> milestones.

## Try it

```sh
cargo run -- --demo        # http://localhost:8080, sample data, no login
```

Demo mode keeps everything in memory and treats every visitor as an admin.
Press <kbd>⌘K</kbd> or <kbd>/</kbd> to search.

## Storage

Everything is stored in one SQLite database (`--database-url`, default
`sqlite://donarium.db`, created if missing; the Helm chart puts it on a
volume at `/data`). Migrations in `migrations/` run on start. There are no
CRDs and the app needs no Kubernetes API access.

Back the volume up: donations (and your sources) can't be rebuilt, only scan
results can. The chart keeps the claim when the release is uninstalled.

## Scanning

A source with the scan method `images` (or `auto`) is scanned for the container
images it runs:

- **Files read:** Docker Compose files, Kubernetes manifests, Flux `HelmRelease`
  values, Kustomization `images:` and Helm `values*.yaml`. Helm charts are not
  rendered.
- **Which project an image belongs to:** an admin's mapping, then the image's own
  `org.opencontainers.image.source` label (read anonymously from its registry),
  then a guess from the image name. Images without a project are listed on the
  source's page, where an admin can map them. Packager images (linuxserver,
  Jellyfin) point to their packaging repository until remapped.
- **Forges:** GitLab, GitHub and Gitea or Forgejo. Each repository is downloaded
  as one archive. The host of every forge comes from the server's
  configuration, never from a form, and its token is only sent to that host:
  - GitLab: `--gitlab-url` (default `https://gitlab.com`), `--gitlab-token-file`.
  - GitHub: `--github-token-file`. Public repositories work without one, within
    GitHub's anonymous limit of 60 requests an hour.
  - Gitea or Forgejo: `--gitea-url` (e.g. `https://git.example.com`) and
    `--gitea-token-file`. Without `--gitea-url`, Gitea sources cannot be added.
    Moving from Gitea to Forgejo only means changing that URL.
  - Private repositories need a read-only token for their forge.
- **Schedule:** `--scan-interval` (default `24h`, `0s` for never) and "Scan now"
  on the source's page.
- **Registries:** only well-known public ones are contacted
  (`--registry-allow` adds more), anonymously. Docker Hub limits anonymous pulls,
  so the first scan of many images may leave some for the next one.

`cargo test --test live -- --ignored --nocapture` checks the registry and GitLab
code against the real services.

## Development

- **Backend:** Rust with axum. Pages are rendered on the server with askama.
- **UI:** Tailwind v4 with vendored [Basecoat](https://basecoatui.com) and
  htmx. No Node.js is needed. `make css` rebuilds `assets/dist/app.css` with the
  [standalone Tailwind CLI](https://github.com/tailwindlabs/tailwindcss/releases/tag/v4.3.3).
- `make check` runs what CI runs (fmt, clippy, tests). `make helm-lint` needs
  Helm and `yq`.

Ports: the UI listens on `:8080`; `/metrics`, `/healthz` and `/readyz` are on
`:9090` without authentication, so don't expose that port.

Authentication is OIDC (`--oidc-issuer`, `--oidc-client-id`,
`--oidc-client-secret-file`). Admins are users whose `groups` claim contains
`donarium-admins`; everyone else gets read-only access. Every flag has a
`DONARIUM_*` environment variable, see `donarium --help`.
