# donarium — implementation plan

A self-hosted, Kubernetes-native tracker for donations to open-source projects.
It finds out which OSS projects your own repositories depend on most, ranks them
by who is most worth supporting next, records each donation, and links the
donation to its receipt. Receipts are referenced where they already exist
(Paperless-ngx) or stored in S3. Donarium never moves money: you donate on the
project's own page, then record it.

It is built like [donarium](https://github.com/PedDavid/donarium): one Rust
binary, axum + askama + htmx + Basecoat UI, OIDC login, Prometheus metrics, a
Helm chart and a CRD group under `prdv.cloud`.

## 1. Decisions

| Topic | Decision |
|---|---|
| Scope | Tracking, ranking and receipts. No payments are made by the app |
| Platform | Kubernetes only. CRD group `donarium.prdv.cloud` |
| Inventory (git) | `Project` and `Source` CRs (see section 3), applied by GitOps like donarium's `ApiKey` |
| Donations | SQLite on a PVC by default, Postgres optional (`--database-url`). Append-only, because the app creates them and they don't belong in git or etcd |
| Usage data | Scanned from your own repos' dependencies, stored in the database, recomputed on a schedule |
| Receipts | Pluggable `ReceiptStore`: **reference** a Paperless-ngx document (default flow), or **upload** to an S3-compatible bucket |
| Auth | OIDC, as in donarium. Everyone who logs in can view; admins (group claim) can record donations and edit weights |
| Language / UI | Rust; askama + Tailwind + Basecoat + htmx; single binary |
| Alerting | Metrics only (reminders such as "no donation in 90 days"). No notifier of its own |

## 2. Architecture

```
 git (Project/Source YAML) ──Argo/Flux──▶ CRs ──watch──┐
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
- **Why hybrid:** what you *watch* (which repos to scan, which projects to
  pin or ignore) is declarative and reviewable, so it lives in git. What you
  *did* (donations, scan results) is event data the app writes, so it lives in
  the database. Donarium's "no database" rule doesn't fit append-only records
  with attachments.
- Backups: the Helm chart documents a `VolumeSnapshot` / `litestream` option.
  Donations are the one thing here that can't be rebuilt, because scan results
  can be re-created and CRs are in git.

## 3. Resources (git)

```yaml
apiVersion: donarium.prdv.cloud/v1alpha1
kind: Source                    # a place to scan for dependencies
metadata: { name: infra, namespace: donarium }
spec:
  provider: github              # github | gitlab
  repo: PedDavid/infra
  weight: 1.0                   # how much this repo counts (prod service > toy)
  scan: [dependency-graph]      # dependency-graph | sbom | lockfiles
---
apiVersion: donarium.prdv.cloud/v1alpha1
kind: Project                   # optional overrides for an upstream project
metadata: { name: tokio-rs-tokio, namespace: donarium }
spec:
  repo: github.com/tokio-rs/tokio   # canonical identity
  usageWeight: 2.0              # manual multiplier on the scanned usage
  ignore: false                 # e.g. company-backed projects you won't fund
  funding:                      # extra channels, merged with discovered ones
    - { platform: opencollective, handle: tokio }
  notes: Used by most of my Rust services.
```

Projects are **discovered** by scans and exist in the database without a CR.
A `Project` CR only adds overrides. Status (last scan, validation) goes in
`.status` as in donarium, with the `status` subresource and a generated CRD.

## 4. Usage scanning

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

Scans run on a schedule (default daily, `--scan-interval`), on demand from the
UI, and after a `Source` changes. Credentials are a GitHub token (read-only
metadata and dependency graph) from a mounted Secret. Hosts are fixed in code
and requests go through a rate-limit-aware client with ETag caching.

Known limits to state in the README: the dependency graph only sees what's
committed, so unpinned or vendored dependencies are missed. Monorepo packages
count as one upstream project. Package → repo mapping can be wrong; the
`Project` CR can correct it through `aliases`.

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
   donarium's on-demand "Create" link: the click is recorded, then it redirects).
2. **Record donation**: project, platform, amount, currency, date, recurring or
   one-off, optional note, and the receipt (section 7).
3. Rows are append-only. Corrections are a new `void` row that references the
   original, so history stays auditable.

```sql
donation(id, project, platform, amount_minor, currency, donated_at,
         base_amount_minor NULL, fx_rate NULL, fx_source /* ecb | manual */ NULL,
         kind /* one_off | recurring */, note, recorded_by, recorded_at,
         voids_id NULL, receipt_id NULL)
receipt(id, donation_id, store /* paperless | s3 */, ref /* doc id or key */,
        url, filename, content_type, sha256 NULL, created_at)
fx_rate(date, currency, rate_to_base, base)   -- cache of fetched rates
```

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
  from donarium's `web::auth`.
- Secrets (GitHub token, Paperless token, S3 credentials) come from mounted
  files, held in `SecretString`, and never logged.
- Provider and Paperless/S3 hosts are fixed by config, never taken from a CR,
  so a spec cannot redirect a credential.
- Uploads are size-limited and type-checked, and are never rendered inline.

## 10. Milestones

1. **Skeleton.** Copy donarium's scaffolding: config, CLI, tracing, OIDC/auth,
   axum + askama + Basecoat shell, `--demo` mode, Helm chart, CI (fmt, clippy,
   tests, CRD up-to-date check, chart release), Dockerfile.
2. **Projects and sources.** `Source`/`Project` CRDs with a generated CRD and
   status, reflector cache, SQLite schema and migrations (`sqlx`).
3. **Scanner.** GitHub dependency-graph/SBOM first, then deps.dev resolution,
   aggregation, and the usage table. `wiremock` tests with recorded fixtures.
4. **Ranking and views.** List by `priority`, detail view with usage, timeline,
   fair share and funding; snapshot and screenshot tests as in donarium.
5. **Donations.** Record, void, totals, recurring entries, `FUNDING.yml`
   discovery, currency conversion (Frankfurter client, rate cache, manual
   override, retry task).
6. **Receipts.** `ReceiptStore` trait, Paperless backend (search, link,
   suggestions), then S3 backend (upload, presigned download); integration
   tests against Paperless and MinIO containers.
7. **Metrics, alerts, dashboard, docs.** Rules checked with `promtool`, Grafana
   dashboard, README, example manifests.
8. **Later.** Lockfile parsers for ecosystems without SBOM support, GitLab,
   CSV/tax-year export, a Paperless matcher for late
   receipts.

## 11. Open points

- Weights for transitive dependencies (0.25 is a guess) need tuning against
  real data once milestone 3 runs on your repos.
- Whether to bring in code from donarium as a shared crate (auth, metrics
  helpers, layout) or copy it. Copying is faster now; a shared crate pays off if
  a third app appears.
