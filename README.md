# donarium

A self-hosted, Kubernetes-native tracker for donations to the open-source
projects you depend on. It scans your own repositories' dependencies, ranks the
upstream projects by how much you use them and how long ago you last donated,
records each donation, and links it to its receipt (Paperless-ngx or S3).

It never moves money: you donate on the project's own page, then record it.

> **Status:** early. The web shell, OIDC login, metrics and Helm chart are in
> place and run on sample data. Storage, scanning, ranking, donations and
> receipts are next. See [docs/PLAN.md](docs/PLAN.md) for the full design and
> milestones.

## Try it

```sh
cargo run -- --demo        # http://localhost:8080, sample data, no login
```

Demo mode keeps everything in memory and treats every visitor as an admin.
Press <kbd>⌘K</kbd> or <kbd>/</kbd> to search.

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
