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
> Projects can also be added by hand, and donations are recorded with their
> receipts' amounts converted to a base currency. Dependency scans, ranking and
> receipts are next. See [docs/PLAN.md](docs/PLAN.md) for the full design and
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

## Donations

You donate on a project's own page, then record it here (admins only). Each
donation keeps what was paid, exactly, in the currency it was paid in.

- **Append-only.** Donations are never edited or deleted: the database refuses
  both. A mistake is *voided*, which keeps it in the history, marked, and stops it
  counting. A project with donations cannot be deleted.
- **Base currency.** Totals are kept in `--base-currency` (default `EUR`). It is
  fixed for a database the first time it is used.
- **Conversion.** A donation in another currency is converted once, when it is
  recorded, with the European Central Bank's rate for the day it was made, from a
  [Frankfurter](https://frankfurter.dev) server (`--fx-url`). A weekend or holiday
  uses the previous business day's rate. You can type the rate instead, for what
  the card was really charged.
  - If the service cannot be reached, the donation is still recorded, marked
    *conversion pending*, and filled in automatically (hourly).
  - For a currency the ECB does not publish (crypto, say), or with
    `--fx-disabled`, an admin sets the rate on the donation, once.
  - This is a tracking aid. The receipt and your bank statement are the source of
    truth; it is not tax advice.
- `--step-up-max-age` makes recording and voiding require a recent login.

Every donation that counts can be downloaded as a CSV file
(`/donations/export.csv`, or `?year=2026` for a tax year) with the amount as
paid, the amount in the base currency, the rate used and where it came from.
Voided donations are left out.

## Receipts

With `--paperless-url` and `--paperless-token-file` (an API token), each
donation gets a *Find receipt* button. It lists the Paperless documents dated
within a week of the donation, or searches by text, and links the one you pick.
Only the document's id and title are kept: nothing is copied, and Paperless
still decides who can open the file. Unlinking never touches the document.
`donarium_donations_without_receipt` counts the donations that have none.

For a receipt that exists nowhere else, set `--s3-endpoint`, `--s3-bucket`,
`--s3-access-key-file` and `--s3-secret-key-file` (and `--s3-region`, `--s3-sse`)
and the same page offers an upload: a PDF, PNG or JPEG of up to 10 MB, checked by
its content, kept as `receipts/<year>/<donation>/<hash>-<name>`. Logged-in users
open it through a five-minute presigned link. Unlinking a receipt forgets it but
leaves the object in the bucket. Requests are signed by Donarium itself (SigV4,
checked against AWS's published example) and use path-style addressing, so
MinIO and similar services work; it has not been run against a real bucket.

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
