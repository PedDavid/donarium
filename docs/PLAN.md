# donarium — implementation plan

A self-hosted, Kubernetes-native tracker for donations to open-source projects.
It finds out which OSS projects your own repositories depend on most, ranks them
by who is most worth supporting next, records each donation, and links the
donation to its receipt. Receipts are referenced where they already exist
(Paperless-ngx) or stored in S3. Donarium never moves money: you donate on the
project's own page, then record it.

It is built like [clavium](https://github.com/PedDavid/clavium): one Rust
binary, axum + askama + htmx + Basecoat UI, OIDC login, Prometheus metrics, a
Helm chart. Unlike clavium it has no CRDs: everything it knows lives in a
database, and it needs no access to the Kubernetes API.

## 1. Decisions

| Topic | Decision |
|---|---|
| Scope | Tracking, ranking and receipts. No payments are made by the app |
| Platform | Kubernetes (Helm chart). The app itself needs no Kubernetes API access, so no RBAC and no CRDs |
| Storage | One database: SQLite on a PVC by default, Postgres optional (`--database-url`). It holds sources, discovered projects, overrides, scan results, donations and receipt references |
| Sources and overrides | Entered by admins in the UI and stored in the database. CRDs and a GitOps layer are a possible later addition, not part of the first versions |
| Usage data | Scanned from your own repos' dependencies, stored in the database, recomputed on a schedule |
| Receipts | Pluggable `ReceiptStore`: **reference** a Paperless-ngx document (default flow), or **upload** to an S3-compatible bucket |
| Auth | OIDC, as in clavium. Everyone who logs in can view; admins (group claim) can record donations and edit weights |
| Language / UI | Rust; askama + Tailwind + Basecoat + htmx; single binary |
| Alerting | Metrics only (reminders such as "no donation in 90 days"). No notifier of its own |

## 2. Architecture

```
 admins (UI) ──sources, overrides, donations──┐
                                                       ▼
 GitHub / GitLab ◀──dependency graph, SBOM, lockfiles── donarium ──▶ deps.dev / ecosyste.ms
                                                       │  │  (package → upstream repo, funding links)
                                  SQLite / Postgres ◀──┘  └──▶ Paperless-ngx (search + link)
                                  (usage, donations)      └──▶ S3 (upload)
                                                       ▼
                                                   /metrics ──▶ Prometheus
```

- **One replica**, with a scheduler task for scans. SQLite on a `ReadWriteOnce`
  PVC forces `replicas: 1`; Postgres lifts that, but nothing needs it yet.
- **Why a database and not CRDs:** donations are append-only events the app
  creates, and sources and overrides are small, rarely changed settings tied to
  rows the scan discovers (you decide to ignore a project while looking at the
  ranking). Keeping all of it in one store means one thing to back up and no
  split between git and the database. Clavium's "no database" approach doesn't
  fit append-only records with attachments.
- Backups: the Helm chart documents a `VolumeSnapshot` / `litestream` option.
  Donations, sources and overrides can't be rebuilt, only scan results can, so
  the database is the one thing to back up.

## 3. Data model

Everything below is stored in the database and changed through the UI. Only
admins can write; everyone who logs in can read.

**Sources** are the repositories you want scanned. An admin adds one with a
provider (`github` or `gitlab`), the repository (`owner/name`) and a weight
(how much that repo counts: a production service 1.0, a toy 0.2). The scan
method (`dependency-graph`, `sbom` or `lockfiles`) defaults to the best one
available. A source can be disabled without deleting it, and "Scan now" runs
it on demand.

**Projects** are **never created by hand**. Scanning the sources discovers the
upstream projects they depend on, and each gets a row identified by its
canonical repository (e.g. `github.com/tokio-rs/tokio`).

**Overrides** are optional per-project settings, edited on the project's
detail page:

- `manual_usage`: usage you assign yourself, added to what scans found. It is how
  a project that no scan can see (an application you run) gets a score.
- `ignore`: e.g. a company-backed project you won't fund.
- extra funding channels, merged with the discovered ones.
- `aliases`: correct a wrong package → repository mapping.
- notes.

```sql
source(id, provider, repo, weight, scan_method, enabled, last_scan_at, last_scan_ok,
       last_scan_status, created_by, created_at, updated_by, updated_at)
project(id, name /* URL slug */, repo UNIQUE, origin /* discovered | manual */,
        first_seen_at, last_seen_at)
usage(source_id, project_id, via /* images | dependencies */,
      kind /* direct | transitive */)                                     -- scan output
source_image(source_id, registry, repository, tag, digest, file, origin)  -- what a scan found
source_skipped(source_id, file, value, reason)
image_resolution(registry, repository, upstream, label, error, retry, resolved_at) -- registry cache
image_mapping(registry, repository, project_repo, created_by, created_at) -- admin decisions
project_funding(project_id, platform, handle, url, origin /* discovered | manual */)
project_override(project_id, manual_usage, ignore, notes, updated_by, updated_at)
project_alias(alias_repo, project_id)
```

`source`, `project`, `usage` and `project_funding` exist since milestone 2
(`migrations/0001_init.sql`), the image-scan tables since `0002`; the override
and alias tables arrive with the project detail view. A project's `usage` can
come from several scans (`via`); when more than one found it in the same source,
the stronger kind counts once. A project's usage score is the sum over enabled sources of
the source's weight, in full for a `direct` usage and times 0.25 for a
`transitive` one.

Changes to sources and overrides are recorded with who made them and when,
like donations. If CRDs are added later (for example a `Source` resource applied
by GitOps), they would write into these same tables.

## 4. Usage scanning

There are two kinds of scan, chosen per source with its scan method:

- **Dependencies** (`dependency-graph`, `sbom`, `lockfiles`): the libraries your
  code depends on (4.1).
- **Images** (`images`): the container images your deployment repositories run
  (4.2), for Docker Compose projects such as a media server and for Kubernetes
  repositories managed with Flux. This one is built first.

`auto` picks dependencies for GitHub repositories and images for none; image
scanning is always chosen explicitly.

### 4.1 Dependency scans

For each `Source`:

1. **Collect dependencies**, in order of preference:
   - GitHub dependency graph / SBOM export API (`/dependency-graph/sbom`),
     which includes transitive dependencies.
   - Otherwise fetch lockfiles (Cargo.lock, package-lock.json, go.sum, uv.lock,
     …) and parse them. A `Parser` trait with one module per ecosystem keeps
     this extensible.
2. **Resolve package → upstream repo** with deps.dev (PURL lookup returns the
   source repo) and ecosyste.ms as a fallback. Cache by PURL in the database.
3. **Aggregate** per upstream project:
   - `direct` count: sources that depend on it directly.
   - `transitive` count: sources that pull it in indirectly.
   - Sum of source weights.
   - `usage_score = Σ_sources weight × (1.0 direct | 0.25 transitive)`,
     multiplied by `Project.usageWeight`. The factors are configurable.
4. **Discover funding** for each project: `FUNDING.yml` in the repo, the GitHub
   Sponsors API, and deps.dev/ecosyste.ms metadata. Normalise to
   `(platform, handle, url)`: GitHub Sponsors, Open Collective, Liberapay, Ko-fi,
   Patreon, thanks.dev, custom URL.

### 4.2 Image scans

1. **Fetch the repository as one archive** and read the YAML files from it in
   memory. Nothing is written to disk and nothing is extracted, so archive paths
   cannot escape anywhere. Download size, expanded size, file size and file
   count are capped.
   - GitLab: `archive.tar.gz` of the default branch.
   - Gitea / Forgejo: the default branch from the repository API, then its
     archive.
   - GitHub: the `tarball` endpoint, which answers with a redirect to
     `codeload.github.com`. That one hop is followed, only to that host over
     https, and without the token.
   Redirects are otherwise refused, so a token only ever goes to its own host.
2. **Extract image references** (`src/scan/`):
   - Docker Compose files, recognised by their content (`services:` and no
     `apiVersion`), not by file name. `${VAR:-default}` uses the default; a
     variable only in the tag drops the tag and keeps the repository; a
     variable in the name is listed as skipped.
   - Kubernetes manifests: `containers`, `initContainers` and
     `ephemeralContainers` of any kind (Deployments, CronJobs, …).
   - Flux and Kustomize: `images:` overrides in Kustomizations, and `image`
     blocks (`repository` / `tag` / `digest`) in HelmRelease values and Helm
     `values*.yaml` files.
   - ConfigMaps, Secrets and CRDs are ignored. Helm charts are not rendered, so
     an image a chart chooses itself is not seen.
3. **Resolve an image to an upstream project**, first match wins:
   1. **An admin's mapping** for the image.
   2. **The image's own label**: `org.opencontainers.image.source` (or the older
      `org.label-schema.vcs-url`), read anonymously from the registry through the
      standard token flow, from the image config or the manifest annotations, and
      cached per repository (a month when found, a day otherwise).
   3. **A guess from the name**: `ghcr.io/owner/name` is
      `github.com/owner/name`, `registry.gitlab.com/group/name` is
      `gitlab.com/group/name`, and linuxserver.io images are
      `github.com/linuxserver/docker-<app>`. Shown as a guess.
   - **Packager images** resolve to the packaging repository, as their labels
     say: `linuxserver/*` and, as the live checks show, Jellyfin
     (`jellyfin/jellyfin-packaging`). An admin can remap them to the
     application's own repository. A curated table of such corrections could be
     added later; it is deliberately not part of the first version.
   - **Only known registries are contacted** (Docker Hub, ghcr.io, lscr.io,
     quay.io, registry.gitlab.com, gcr.io, registry.k8s.io, mcr.microsoft.com,
     public.ecr.aws, docker.elastic.co, codeberg.org, plus `--registry-allow`),
     because registry hosts come from repository files and must not steer the
     server towards internal addresses. Token endpoints must be public https
     hosts, and responses are size limited.
   - **Rate limits.** Anonymous Docker Hub allows about 100 manifest pulls per six
     hours per address, and each lookup of a multi-architecture image takes two.
     On a 429 the rest of that registry's images are left for the next scan
     instead of being recorded as failures. Later scans are cheap, because
     answers are cached. Authenticated pulls (a Docker Hub token) would raise the
     limit and are not implemented.
   - Images that cannot be resolved are listed on the source's page with the
     reason and the file they came from, so an admin can map them by hand.
4. **Write usage:** one `direct` usage per resolved project (an image you run is
   a direct use), weighted by the source's weight like any other.

Scans run on a schedule (default daily, `--scan-interval`, `0s` turns it off)
and on demand from the source's page ("Scan now", in the background). Two scans
of one source never overlap. A failed scan keeps what the previous one found and
marks the source as failed; the metrics `donarium_source_last_scan_ok` and
`donarium_source_last_scan_timestamp_seconds` let Prometheus alert on that.

**Forges and credentials.** The host of every forge comes from the server
configuration, never from a form: GitHub is `github.com`, GitLab is
`--gitlab-url` (default `gitlab.com`), and a self-hosted **Gitea or Forgejo** is
`--gitea-url` (one instance; Gitea sources cannot be added without it). The
source's provider is `gitea` (it covers Forgejo, whose API is compatible), and
moving a repository from Gitea to Forgejo only means changing that URL. Pasted
repository URLs must be on the configured host. Each forge has its own
read-only token, mounted from a Secret and sent only to its own host. Public
repositories (the GitLab media server) need no token.

Known limits to state in the README: the dependency graph only sees what's
committed, so unpinned or vendored dependencies are missed. Monorepo packages
count as one upstream project. Package → repo mapping can be wrong; an
override can correct it through an alias.

## 5. Ranking

The **list view** is sorted by one number and one question: *who is most overdue
relative to how much you use them?*

```
priority = usage_share × staleness
usage_share = usage_score / Σ usage_score        (0..1)
staleness   = 1 + days_since_last_donation / 365   (capped, never donated = max)
```

- Projects with no way to donate and `ignore: true` projects are excluded from
  the main list, and shown in a collapsed section.
- The staleness factor is deliberately the main driver in the list: it spreads
  donations across projects instead of repeatedly paying the biggest one.
- The formula lives in one function with unit tests, so it can change.

The **detail view** shows the full picture behind the number:

- Usage: which of your sources use it, direct or transitive, and the score.
- Time: last donation, and a timeline of all donations.
- Fair share: total donated vs usage share, as a "target vs actual" bar
  (`target = usage_share × total_ever_donated`).
- Funding: discovered channels with links, and which are not available.
- Need/health: maintainers, last release and commit, whether the project is
  company-backed (from the GitHub API and ecosyste.ms). Informational only
  at first; not part of `priority`.

## 6. Donations

Recording flow (admin only):

1. From a project's page, **Donate** opens its funding link (same pattern as
   clavium's on-demand "Create" link: the click is recorded, then it redirects).
2. **Record donation**: project, platform, amount, currency, date, recurring or
   one-off, optional note, and the receipt (section 7).
3. Rows are append-only. Corrections are a new `void` row that references the
   original, so history stays auditable.

```sql
donation(id, project_id, platform, amount_minor, currency, donated_on /* a date */,
         base_amount_minor NULL, fx_rate NULL, fx_source /* same | ecb | manual */ NULL,
         kind /* one_off | recurring */, note, recorded_by, recorded_at,
         voids_id NULL)
meta(key, value)                                  -- the database's base currency
fx_rate(date, currency, base, rate, rate_date)    -- cache of fetched rates
```

Donations are append-only, enforced by the database and not only by the
application: triggers refuse any delete, and any update except filling in a
pending base amount once. A mistake is corrected with a **void entry** (a row
with `voids_id` set); the donation stays in the history, marked as voided, and
stops counting. A project with donations cannot be deleted or pruned.
Amounts are whole minor units (cents), never floats, and a currency's decimal
places are respected (yen have none, Kuwaiti dinars three). The base currency
is remembered in `meta` the first time the database is used, and a different
`--base-currency` is refused, since totals in two currencies would mean nothing.
Receipts (section 7) arrive with their own table.

A recurring donation repeats as a monthly "expected" entry that you confirm,
because the app can't see your bank.

### 6.1 Currency conversion

- Each donation keeps its **original** `amount_minor` + `currency` (the figure
  on the receipt) and a **base amount** (`base_amount_minor`, `fx_rate`,
  `fx_source`) converted once, when it is recorded. Totals never shift when
  rates move later.
- `--base-currency` (default `EUR`) is the currency for totals, the fair-share
  bars and metrics. Changing it later only affects donations recorded after the
  change; a `donarium reconvert` subcommand re-derives base amounts from the
  cached rates if needed.
- **Rates** come from [Frankfurter](https://frankfurter.dev) (ECB reference
  rates): free, no key, historical by date. A weekend or holiday uses the
  previous business day. The host is fixed in code (`--fx-url` overrides it for
  tests and self-hosted mirrors). Results are cached in `fx_rate`.
- **Override.** The record form pre-fills the rate and base amount. An admin can
  edit them when the real charge differed (card fees, a different rate), which
  sets `fx_source = manual`. Currencies the ECB doesn't publish (e.g. crypto)
  require a manual rate.
- **Failure handling.** If the lookup fails, recording is not blocked: the
  donation is saved with the base amount empty, and a retry task fills it in
  later. Donations without a base amount are excluded from base totals, shown
  with a marker, and counted in a metric.
- Original amounts per currency are always shown alongside base totals in the
  detail view. This is a tracking aid, not tax advice: the receipt and bank
  statement stay the source of truth. A CSV export with the original amount,
  rate and receipt reference is a later milestone.

## 7. Receipts

```rust
#[async_trait]
pub trait ReceiptStore: Send + Sync {
    async fn search(&self, query: &ReceiptQuery) -> Result<Vec<ReceiptHit>, StoreError>;
    async fn link(&self, reference: &str) -> Result<ReceiptRef, StoreError>;    // reference only
    async fn upload(&self, file: Upload) -> Result<ReceiptRef, StoreError>;     // optional
    async fn open(&self, r: &ReceiptRef) -> Result<Download, StoreError>;       // stream / redirect
}
```

- **Paperless-ngx (reference-first).** Email receipts usually already arrive
  in Paperless through its mail rules. Recording a donation therefore
  **searches** Paperless (`/api/documents/?query=…` with correspondent, date
  range and amount hints from the form) and lets you pick a match, which stores
  only the document ID. Nothing is copied.
  - Auto-suggest: after a donation is recorded, show candidate documents
    within ±7 days of the date, matching the platform name or the amount.
  - A background **matcher** can attach receipts that arrive later; it only
    suggests, an admin confirms.
  - Optional tag on link (`donarium`) so the receipts show in Paperless too.
  - Auth: API token from a Secret. Files are streamed through Donarium or
    linked to Paperless's own UI, depending on `--paperless-open=proxy|link`.
    The default is `link`, so Donarium never serves documents that Paperless
    already protects.
- **S3 (upload).** For receipts that exist nowhere else (a PDF from a donation
  page). Uploaded to `<bucket>/<year>/<donation-id>/<filename>` with SSE and a
  SHA-256, served through short-lived presigned URLs after an OIDC check.
  Limits: size cap, `application/pdf` and common image types, filename
  sanitising.
- Both can be enabled at once. Each donation's receipt records which store it
  came from. A donation without a receipt is allowed and flagged, and there is
  a metric for it.

## 8. Metrics and alerts

| Metric | Meaning |
|---|---|
| `donarium_project_usage_score{project}` | Current usage score |
| `donarium_project_priority{project}` | Ranking value (top N only, to bound cardinality) |
| `donarium_last_donation_timestamp_seconds{project}` | |
| `donarium_donated_base_total` | Sum of non-void donations in the base currency (gauge) |
| `donarium_donated_original_total{currency}` | Sum in the original currencies |
| `donarium_donations_without_conversion` | Donations still missing a base amount |
| `donarium_fx_requests_total{result}` | Rate lookups |
| `donarium_donations_without_receipt` | Receipt hygiene |
| `donarium_scan_last_success_timestamp_seconds{source}` | Alert when scans go stale |
| `donarium_scan_requests_total{provider,result}` | |
| `donarium_receipt_store_healthy{store}` | |

Shipped `PrometheusRule`s: scans stale, receipt store unhealthy, donations
without receipts, and an optional "no donation in N days" reminder.

## 9. Security

- OIDC with PKCE, encrypted cookie sessions, CSRF and same-origin checks, strict
  CSP and `no-store` responses, optional step-up for recording: all copied
  from clavium's `web::auth`.
- Secrets (GitHub token, Paperless token, S3 credentials) come from mounted
  files, held in `SecretString`, and never logged.
- Provider and Paperless/S3 hosts are fixed by config, never taken from
  user-entered data, so a source or override cannot redirect a credential. The
  hosts of self-hosted forges (Gitea, Forgejo) are part of that configuration, and
  a source can only name one of them. Source repositories are validated as
  `owner/name` against the configured host. Each forge token is sent only to its
  own host. Registry lookups for image labels are anonymous.
- Uploads are size-limited and type-checked, and are never rendered inline.

## 10. Milestones

1. **Skeleton.** Copy clavium's scaffolding: config, CLI, tracing, OIDC/auth,
   axum + askama + Basecoat shell, `--demo` mode, Helm chart, CI (fmt, clippy,
   tests, chart release), Dockerfile.
2. **Storage.** SQLite schema and migrations (`sqlx`), a `Repository` backed by
   it, the PVC in the Helm chart, and the UI to add, edit, disable and delete
   sources (admin only, with audit fields).
3. **Scanner.** Image scans first:
   - 3a (done): image reference parsing and extraction from Compose, Kubernetes
     and Flux files, tested on realistic fixtures.
   - 3b-1 (done): the GitLab path: archive fetch with limits, label lookup,
     resolution and mappings, usage, scheduler and "Scan now", and the source
     page for images, skipped references and mappings. Tested against mocked
     GitLab and registries, plus `tests/live.rs` (`--ignored`) against the real
     ones.
   - 3b-2 (done): Gitea/Forgejo (`gitea` provider, host from `--gitea-url`,
     token) and GitHub, with the forge hosts moved into configuration. Checked
     live against GitHub (through a repository this environment may reach) as
     well as with mocks; there is no Gitea or Forgejo to try it on here, so that
     path is tested against mocks of its API only.
   - 3c: dependency scans: GitHub dependency-graph/SBOM, deps.dev resolution,
     aggregation.
   `wiremock` tests with recorded fixtures.
4. **Ranking and views.** List by `priority`, detail view with usage, timeline,
   fair share and funding; snapshot and screenshot tests as in clavium.
5. **Donations.** Record, void, totals, recurring entries, `FUNDING.yml`
   discovery, currency conversion (Frankfurter client, rate cache, manual
   override, retry task).
6. **Receipts.** `ReceiptStore` trait, Paperless backend (search, link,
   suggestions), then S3 backend (upload, presigned download); integration
   tests against Paperless and MinIO containers.
7. **Metrics, alerts, dashboard, docs.** Rules checked with `promtool`, Grafana
   dashboard, README, example manifests.
8. **Runtime usage (roadmap).** Count what you actually use from your
   reverse proxy's metrics, and map it to projects. See section 11.
9. **Later.** Lockfile parsers for ecosystems without SBOM support, GitLab,
   CSV/tax-year export, a Paperless matcher for late
   receipts.

## 11. Runtime usage signal (roadmap)

Scanning finds what you *depend on*. It cannot tell what you actually *use*,
and it never sees the applications you only run (Paperless-ngx, Grafana,
Jellyfin, …), because they are not dependencies of your own code. A second,
runtime signal fills that gap: which of your self-hosted services get used,
and how much.

**Source of the signal: Traefik's own metrics first, a plugin only if needed.**
Traefik already exports per-router and per-service request counters (Prometheus
`traefik_service_requests_total` and friends) and can write JSON access logs.
Donarium can read them without anything running inside the proxy:

- **Prometheus query (default).** Donarium queries an existing Prometheus
  (`--prometheus-url`) once a day for the previous day's request counts per
  Traefik service, split by status class. Any proxy that exports similar
  metrics (Caddy, ingress-nginx) works the same way.
- **Access logs (alternative)**, for setups without Prometheus.
- **A Traefik plugin** would only add value if the metrics turn out to be too
  coarse (for example distinct visitors or paths). It is also the costlier
  option: plugins are Go code run by Traefik's Yaegi interpreter, which limits
  the standard library and dependencies, they are installed from a public
  GitHub repository through the Traefik plugin catalog, they run inside every
  request path of the proxy, and they would need their own release and
  maintenance. Not planned unless the metrics approach proves insufficient.

**Mapping a service to a project is the real work**, not the counting:

- An admin maps a Traefik service (or hostname) to a project in the UI.
  Suggestions come from matching the service name against known projects.
- Container images in manifests of a scanned source (for example a homelab
  GitOps repository) resolve to an upstream repository through the image's
  `org.opencontainers.image.source` label. This reuses the scanner, needs no
  Kubernetes access, and gives the mapping for free where it works.

**From counts to a score.** Raw request counts say little (a chatty API versus a
tool you open twice a week). The signal is **active days**: days in the last 90
with enough successful requests, after excluding health checks, static assets
and obvious scanner traffic (4xx floods). The runtime score is added to the
static usage score with its own configurable factor, and the detail view shows
both ("declared in 3 repositories, used on 61 of the last 90 days").

**Privacy.** Only per-service daily aggregates are stored. No URLs, client IPs
or user identifiers leave the proxy's metrics.

Data model sketch, added when this is built:

```sql
service_mapping(service_key, project_id, origin /* manual | image | suggested */)
runtime_usage(service_key, day, requests_ok, requests_error)
```

It does not block any earlier milestone: scanning, ranking and donations work
without it.

## 12. Open points

- CRDs and GitOps for sources/overrides are deferred. If wanted later, they
  write into the same tables, so nothing built before then changes.
- Weights for transitive dependencies (0.25 is a guess) need tuning against
  real data once milestone 3 runs on your repos.
- Whether to bring in code from clavium as a shared crate (auth, metrics
  helpers, layout) or copy it. Copying is faster now; a shared crate pays off if
  a third app appears.
