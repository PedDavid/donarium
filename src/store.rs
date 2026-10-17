//! The database: sources, discovered projects and (later) donations. SQLite
//! on a volume; the schema lives in `migrations/`.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::time::Duration;

use jiff::Timestamp;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use crate::fx::Rate;
use crate::model::{
    Actor, Conversion, Donation, DonationKind, Funding, FxSource, NewDonation, NewSource, Origin,
    Project, ProjectUpdate, Provider, ScanMethod, Source, SourceUpdate, TRANSITIVE_FACTOR, Totals,
};
use crate::scan::manifests::{ImageUse, Skipped};
use crate::scan::resolve::slug;
use crate::scan::usage::{Lookup, Resolved, resolve};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("that source already exists")]
    Duplicate,
    /// Something that cannot be removed because other data depends on it.
    #[error("{0}")]
    InUse(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("{0}")]
    Corrupt(String),
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Opens (and creates if missing) the SQLite database at `url`, e.g.
    /// `sqlite:///data/donarium.db`, and applies pending migrations.
    pub async fn connect(url: &str) -> Result<Store, StoreError> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        Store::migrated(pool).await
    }

    /// A private in-memory database, for `--demo` and tests. It lives as long
    /// as its single connection, which the pool never recycles.
    pub async fn memory() -> Result<Store, StoreError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Store::migrated(pool).await
    }

    async fn migrated(pool: SqlitePool) -> Result<Store, StoreError> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool })
    }

    /// Direct access for seeding sample data.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// True when the database answers.
    pub async fn ready(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    // --- sources ---

    pub async fn list_sources(&self) -> Result<Vec<Source>, StoreError> {
        sqlx::query("SELECT * FROM source ORDER BY provider, repo COLLATE NOCASE")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(source_from_row)
            .collect()
    }

    pub async fn get_source(&self, id: i64) -> Result<Option<Source>, StoreError> {
        sqlx::query("SELECT * FROM source WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(source_from_row)
            .transpose()
    }

    pub async fn create_source(
        &self,
        new: &NewSource,
        by: &Actor,
        now: Timestamp,
    ) -> Result<Source, StoreError> {
        let now = now.to_string();
        let row = sqlx::query(
            "INSERT INTO source (provider, repo, weight, scan_method, enabled,
                                 created_by, created_by_name, created_at,
                                 updated_by, updated_by_name, updated_at)
             VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)
             RETURNING *",
        )
        .bind(new.provider.as_str())
        .bind(&new.repo)
        .bind(new.weight)
        .bind(new.scan_method.as_str())
        .bind(&by.sub)
        .bind(&by.name)
        .bind(&now)
        .bind(&by.sub)
        .bind(&by.name)
        .bind(&now)
        .fetch_one(&self.pool)
        .await
        .map_err(map_unique)?;
        source_from_row(&row)
    }

    pub async fn update_source(
        &self,
        id: i64,
        update: &SourceUpdate,
        by: &Actor,
        now: Timestamp,
    ) -> Result<Source, StoreError> {
        let row = sqlx::query(
            "UPDATE source
                SET weight = ?, scan_method = ?, enabled = ?,
                    updated_by = ?, updated_by_name = ?, updated_at = ?
              WHERE id = ?
          RETURNING *",
        )
        .bind(update.weight)
        .bind(update.scan_method.as_str())
        .bind(update.enabled)
        .bind(&by.sub)
        .bind(&by.name)
        .bind(now.to_string())
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        source_from_row(&row)
    }

    /// Deletes a source and the usage rows it produced. Projects only that
    /// source used stay until the next scan prunes them.
    pub async fn delete_source(&self, id: i64) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM source WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    // --- scans ---

    /// Stores what an image scan found in a source, replacing the previous
    /// findings. The scan's outcome is recorded separately, by
    /// [`Store::finish_scan`], once the lookups that follow are done.
    pub async fn store_scan(
        &self,
        source_id: i64,
        images: &[ImageUse],
        skipped: &[Skipped],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM source_image WHERE source_id = ?")
            .bind(source_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM source_skipped WHERE source_id = ?")
            .bind(source_id)
            .execute(&mut *tx)
            .await?;
        for u in images {
            sqlx::query(
                "INSERT INTO source_image (source_id, registry, repository, tag, digest, file, origin)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(source_id)
            .bind(&u.image.registry)
            .bind(&u.image.repository)
            .bind(&u.image.tag)
            .bind(&u.image.digest)
            .bind(&u.file)
            .bind(&u.origin)
            .execute(&mut *tx)
            .await?;
        }
        for s in skipped {
            sqlx::query(
                "INSERT INTO source_skipped (source_id, file, value, reason) VALUES (?, ?, ?, ?)",
            )
            .bind(source_id)
            .bind(&s.file)
            .bind(&s.value)
            .bind(&s.reason)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records how the last scan of a source ended.
    pub async fn finish_scan(
        &self,
        source_id: i64,
        at: Timestamp,
        ok: bool,
        status: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE source SET last_scan_at = ?, last_scan_ok = ?, last_scan_status = ? WHERE id = ?",
        )
        .bind(at.to_string())
        .bind(ok)
        .bind(status)
        .bind(source_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn skipped_images(&self, source_id: i64) -> Result<Vec<Skipped>, StoreError> {
        sqlx::query("SELECT file, value, reason FROM source_skipped WHERE source_id = ? ORDER BY file, value")
            .bind(source_id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(Skipped {
                    file: r.try_get("file")?,
                    value: r.try_get("value")?,
                    reason: r.try_get("reason")?,
                })
            })
            .collect()
    }

    /// The distinct image repositories found in sources, with a tag or digest
    /// to look them up by, that have no lookup newer than the cutoffs: a
    /// lookup that found a label is fresh for `ok_for`, any other for `retry_after`.
    pub async fn images_to_look_up(
        &self,
        now: Timestamp,
        ok_for: jiff::SignedDuration,
        retry_after: jiff::SignedDuration,
    ) -> Result<Vec<LookupTarget>, StoreError> {
        let rows = sqlx::query(
            "SELECT i.registry, i.repository, MAX(i.tag) AS tag, MAX(i.digest) AS digest,
                    r.upstream AS upstream, r.resolved_at AS resolved_at,
                    COALESCE(r.retry, 0) AS retry
               FROM source_image i
               LEFT JOIN image_resolution r
                      ON r.registry = i.registry AND r.repository = i.repository
              GROUP BY i.registry, i.repository
              ORDER BY i.registry, i.repository",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in rows {
            let resolved_at: Option<String> = row.try_get("resolved_at")?;
            let upstream: Option<String> = row.try_get("upstream")?;
            let retry: bool = row.try_get("retry")?;
            let fresh = match resolved_at {
                None => false,
                // A temporary failure is always asked again.
                Some(_) if retry => false,
                Some(raw) => {
                    let at: Timestamp = raw.parse().map_err(|_| corrupt("resolved_at", &raw))?;
                    let ttl = if upstream.is_some() {
                        ok_for
                    } else {
                        retry_after
                    };
                    now.duration_since(at) < ttl
                }
            };
            if !fresh {
                out.push(LookupTarget {
                    registry: row.try_get("registry")?,
                    repository: row.try_get("repository")?,
                    tag: row.try_get("tag")?,
                    digest: row.try_get("digest")?,
                });
            }
        }
        Ok(out)
    }

    pub async fn save_lookup(
        &self,
        registry: &str,
        repository: &str,
        lookup: &Lookup,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO image_resolution (registry, repository, upstream, label, error, retry, resolved_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (registry, repository) DO UPDATE SET
                 upstream = excluded.upstream, label = excluded.label,
                 error = excluded.error, retry = excluded.retry,
                 resolved_at = excluded.resolved_at",
        )
        .bind(registry)
        .bind(repository)
        .bind(&lookup.upstream)
        .bind(&lookup.label)
        .bind(&lookup.error)
        .bind(lookup.retry)
        .bind(now.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn lookups(&self) -> Result<HashMap<(String, String), Lookup>, StoreError> {
        let mut map = HashMap::new();
        for r in sqlx::query(
            "SELECT registry, repository, upstream, label, error, retry FROM image_resolution",
        )
        .fetch_all(&self.pool)
        .await?
        {
            map.insert(
                (r.try_get("registry")?, r.try_get("repository")?),
                Lookup {
                    upstream: r.try_get("upstream")?,
                    label: r.try_get("label")?,
                    error: r.try_get("error")?,
                    retry: r.try_get("retry")?,
                },
            );
        }
        Ok(map)
    }

    // --- image mappings ---

    pub async fn list_mappings(&self) -> Result<Vec<ImageMapping>, StoreError> {
        sqlx::query("SELECT * FROM image_mapping ORDER BY registry, repository")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(ImageMapping {
                    registry: r.try_get("registry")?,
                    repository: r.try_get("repository")?,
                    project_repo: r.try_get("project_repo")?,
                    created_by: Actor::new(
                        r.try_get::<String, _>("created_by")?,
                        r.try_get::<String, _>("created_by_name")?,
                    ),
                    created_at: timestamp(r, "created_at")?,
                })
            })
            .collect()
    }

    /// Says which project an image belongs to. Overwrites an earlier mapping.
    pub async fn set_mapping(
        &self,
        registry: &str,
        repository: &str,
        project_repo: &str,
        by: &Actor,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO image_mapping (registry, repository, project_repo, created_by, created_by_name, created_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (registry, repository) DO UPDATE SET
                 project_repo = excluded.project_repo, created_by = excluded.created_by,
                 created_by_name = excluded.created_by_name, created_at = excluded.created_at",
        )
        .bind(registry)
        .bind(repository)
        .bind(project_repo)
        .bind(&by.sub)
        .bind(&by.name)
        .bind(now.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_mapping(&self, registry: &str, repository: &str) -> Result<(), StoreError> {
        let done = sqlx::query("DELETE FROM image_mapping WHERE registry = ? AND repository = ?")
            .bind(registry)
            .bind(repository)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    // --- images and usage ---

    /// The images found in a source, one row per image repository, with the
    /// project each one resolves to.
    pub async fn image_report(&self, source_id: i64) -> Result<Vec<ImageReport>, StoreError> {
        let mappings: HashMap<(String, String), String> = self
            .list_mappings()
            .await?
            .into_iter()
            .map(|m| ((m.registry, m.repository), m.project_repo))
            .collect();
        let lookups = self.lookups().await?;
        let rows = sqlx::query(
            "SELECT registry, repository, GROUP_CONCAT(DISTINCT tag) AS tags,
                    MIN(file) AS file, COUNT(*) AS uses
               FROM source_image WHERE source_id = ?
              GROUP BY registry, repository
              ORDER BY registry, repository",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let registry: String = r.try_get("registry")?;
                let repository: String = r.try_get("repository")?;
                let key = (registry.clone(), repository.clone());
                let resolved = resolve(
                    &registry,
                    &repository,
                    mappings.get(&key).map(String::as_str),
                    lookups.get(&key),
                );
                Ok(ImageReport {
                    tags: r.try_get::<Option<String>, _>("tags")?.unwrap_or_default(),
                    file: r.try_get("file")?,
                    uses: r.try_get("uses")?,
                    registry,
                    repository,
                    resolved,
                })
            })
            .collect()
    }

    /// Rewrites the usage that image scans produce, from what is stored: the
    /// images each source contains, the admin mappings and the cached
    /// lookups. No network. Projects that are no longer used by any source
    /// (and were discovered, not added by hand) are removed.
    pub async fn rebuild_image_usage(&self, now: Timestamp) -> Result<UsageSummary, StoreError> {
        let mappings: HashMap<(String, String), String> = self
            .list_mappings()
            .await?
            .into_iter()
            .map(|m| ((m.registry, m.repository), m.project_repo))
            .collect();
        let lookups = self.lookups().await?;
        let images = sqlx::query(
            "SELECT DISTINCT source_id, registry, repository FROM source_image ORDER BY source_id, registry, repository",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut used: BTreeSet<(i64, String)> = BTreeSet::new();
        let mut unresolved = BTreeSet::new();
        for row in &images {
            let source_id: i64 = row.try_get("source_id")?;
            let registry: String = row.try_get("registry")?;
            let repository: String = row.try_get("repository")?;
            let key = (registry.clone(), repository.clone());
            let Resolved { project, .. } = resolve(
                &registry,
                &repository,
                mappings.get(&key).map(String::as_str),
                lookups.get(&key),
            );
            match project {
                Some(project) => {
                    used.insert((source_id, project));
                }
                None => {
                    unresolved.insert(key);
                }
            }
        }

        let stamp = now.to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM usage WHERE via = 'images'")
            .execute(&mut *tx)
            .await?;
        let mut project_ids: HashMap<String, i64> = HashMap::new();
        for (source_id, project) in &used {
            let id = match project_ids.get(project) {
                Some(id) => *id,
                None => {
                    let id = upsert_project(&mut tx, project, &stamp).await?;
                    project_ids.insert(project.clone(), id);
                    id
                }
            };
            sqlx::query(
                "INSERT INTO usage (source_id, project_id, via, kind) VALUES (?, ?, 'images', 'direct')",
            )
            .bind(source_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        let pruned = sqlx::query(
            "DELETE FROM project
              WHERE origin = 'discovered'
                AND id NOT IN (SELECT project_id FROM usage)
                -- A project you donated to stays, whatever happens to its sources.
                AND id NOT IN (SELECT project_id FROM donation)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(UsageSummary {
            projects: project_ids.len(),
            unresolved: unresolved.len(),
            pruned: pruned as usize,
        })
    }

    // --- donations ---

    /// Remembers the base currency of this database on first use, and refuses
    /// a different one later: totals are kept in it, and mixing two would make
    /// them meaningless.
    pub async fn ensure_base_currency(&self, base: &str) -> Result<(), StoreError> {
        sqlx::query("INSERT OR IGNORE INTO meta (key, value) VALUES ('base_currency', ?)")
            .bind(base)
            .execute(&self.pool)
            .await?;
        let stored: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key = 'base_currency'")
                .fetch_one(&self.pool)
                .await?;
        if stored != base {
            return Err(StoreError::InUse(format!(
                "this database keeps its totals in {stored}, but --base-currency is {base}; \
                 changing the base currency is not supported yet"
            )));
        }
        Ok(())
    }

    /// Records a donation. The project must exist.
    pub async fn record_donation(
        &self,
        new: &NewDonation,
        by: &Actor,
        now: Timestamp,
    ) -> Result<Donation, StoreError> {
        let mut tx = self.pool.begin().await?;
        let project = project_id(&mut tx, &new.project).await?;
        let (base, rate, source) = match &new.conversion {
            Some(c) => (Some(c.base_minor), Some(c.rate), Some(c.source.as_str())),
            None => (None, None, None),
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO donation (project_id, platform, amount_minor, currency, donated_on,
                                   base_amount_minor, fx_rate, fx_source, kind, note,
                                   recorded_by, recorded_by_name, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(project)
        .bind(&new.platform)
        .bind(new.amount_minor)
        .bind(&new.currency)
        .bind(new.donated_on.to_string())
        .bind(base)
        .bind(rate)
        .bind(source)
        .bind(new.kind.as_str())
        .bind(&new.note)
        .bind(&by.sub)
        .bind(&by.name)
        .bind(now.to_string())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        self.get_donation(id).await?.ok_or(StoreError::NotFound)
    }

    /// Cancels a donation with a void entry. The donation stays in the history,
    /// marked as voided, and stops counting.
    pub async fn void_donation(
        &self,
        id: i64,
        by: &Actor,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let is_void: Option<bool> =
            sqlx::query_scalar("SELECT voids_id IS NOT NULL FROM donation WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        match is_void {
            None => return Err(StoreError::NotFound),
            Some(true) => return Err(StoreError::InUse("a void entry cannot be voided".into())),
            Some(false) => {}
        }
        sqlx::query(
            "INSERT INTO donation (project_id, platform, amount_minor, currency, donated_on,
                                   base_amount_minor, fx_rate, fx_source, kind, note,
                                   recorded_by, recorded_by_name, recorded_at, voids_id)
             SELECT project_id, platform, amount_minor, currency, donated_on,
                    base_amount_minor, fx_rate, fx_source, kind, note, ?, ?, ?, id
               FROM donation WHERE id = ?",
        )
        .bind(&by.sub)
        .bind(&by.name)
        .bind(now.to_string())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| match e.as_database_error() {
            Some(db) if db.is_unique_violation() => {
                StoreError::InUse("this donation is already voided".into())
            }
            _ => StoreError::Db(e),
        })?;
        Ok(())
    }

    const DONATION_SELECT: &'static str =
        "SELECT d.*, p.name AS project_name, p.repo AS project_repo,
                v.recorded_at AS voided_at, v.recorded_by AS voided_by,
                v.recorded_by_name AS voided_by_name
           FROM donation d
           JOIN project p ON p.id = d.project_id
           LEFT JOIN donation v ON v.voids_id = d.id";

    pub async fn get_donation(&self, id: i64) -> Result<Option<Donation>, StoreError> {
        sqlx::query(&format!(
            "{} WHERE d.id = ? AND d.voids_id IS NULL",
            Self::DONATION_SELECT
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(donation_from_row)
        .transpose()
    }

    /// Donations, newest first, with voided ones marked. Void entries are not
    /// listed: they are the mark on the donation they cancel. Limited to one
    /// project if `project` is given.
    pub async fn list_donations(&self, project: Option<&str>) -> Result<Vec<Donation>, StoreError> {
        let filter = if project.is_some() {
            "AND p.name = ?"
        } else {
            ""
        };
        let sql = format!(
            "{} WHERE d.voids_id IS NULL {filter} ORDER BY d.donated_on DESC, d.id DESC",
            Self::DONATION_SELECT
        );
        let mut query = sqlx::query(&sql);
        if let Some(name) = project {
            query = query.bind(name);
        }
        query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(donation_from_row)
            .collect()
    }

    /// What the donations that count add up to, for all projects or one.
    pub async fn donation_totals(&self, project: Option<&str>) -> Result<Totals, StoreError> {
        let filter = if project.is_some() {
            "AND p.name = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT d.currency, d.donated_on, d.amount_minor, d.base_amount_minor
               FROM donation d JOIN project p ON p.id = d.project_id
              WHERE d.voids_id IS NULL
                AND NOT EXISTS (SELECT 1 FROM donation v WHERE v.voids_id = d.id)
                {filter}"
        );
        let mut query = sqlx::query(&sql);
        if let Some(name) = project {
            query = query.bind(name);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut totals = Totals::default();
        let mut by_currency: HashMap<String, i64> = HashMap::new();
        let mut by_year: HashMap<i32, i64> = HashMap::new();
        for row in &rows {
            totals.count += 1;
            let currency: String = row.try_get("currency")?;
            let amount: i64 = row.try_get("amount_minor")?;
            *by_currency.entry(currency).or_default() += amount;
            match row.try_get::<Option<i64>, _>("base_amount_minor")? {
                Some(base) => {
                    totals.base_minor += base;
                    let day: String = row.try_get("donated_on")?;
                    let year = day
                        .get(..4)
                        .and_then(|y| y.parse().ok())
                        .ok_or_else(|| corrupt("donation date", &day))?;
                    *by_year.entry(year).or_default() += base;
                }
                None => totals.unconverted += 1,
            }
        }
        totals.by_currency = by_currency.into_iter().collect();
        totals
            .by_currency
            .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        totals.by_year = by_year.into_iter().collect();
        totals.by_year.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(totals)
    }

    // --- exchange rates ---

    /// A rate fetched earlier for this day, currency and base.
    pub async fn cached_rate(
        &self,
        date: jiff::civil::Date,
        currency: &str,
        base: &str,
    ) -> Result<Option<f64>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT rate FROM fx_rate WHERE date = ? AND currency = ? AND base = ?",
        )
        .bind(date.to_string())
        .bind(currency)
        .bind(base)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn save_rate(
        &self,
        date: jiff::civil::Date,
        currency: &str,
        base: &str,
        rate: &Rate,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO fx_rate (date, currency, base, rate, rate_date) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (date, currency, base) DO NOTHING",
        )
        .bind(date.to_string())
        .bind(currency)
        .bind(base)
        .bind(rate.rate)
        .bind(rate.rate_date.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Donations that count and are still waiting for their amount in the
    /// base currency, oldest first.
    pub async fn pending_conversions(
        &self,
        limit: i64,
    ) -> Result<Vec<PendingConversion>, StoreError> {
        sqlx::query(
            "SELECT d.id, d.amount_minor, d.currency, d.donated_on
               FROM donation d
              WHERE d.base_amount_minor IS NULL AND d.voids_id IS NULL
                AND NOT EXISTS (SELECT 1 FROM donation v WHERE v.voids_id = d.id)
              ORDER BY d.donated_on, d.id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            let day: String = r.try_get("donated_on")?;
            Ok(PendingConversion {
                id: r.try_get("id")?,
                amount_minor: r.try_get("amount_minor")?,
                currency: r.try_get("currency")?,
                donated_on: day.parse().map_err(|_| corrupt("donated_on", &day))?,
            })
        })
        .collect()
    }

    /// Fills in a pending conversion. This is the one change a donation allows,
    /// and only once: a donation that already has its base amount is refused.
    pub async fn set_conversion(&self, id: i64, conversion: &Conversion) -> Result<(), StoreError> {
        let done = sqlx::query(
            "UPDATE donation SET base_amount_minor = ?, fx_rate = ?, fx_source = ?
              WHERE id = ? AND base_amount_minor IS NULL AND voids_id IS NULL",
        )
        .bind(conversion.base_minor)
        .bind(conversion.rate)
        .bind(conversion.source.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return match self.get_donation(id).await? {
                None => Err(StoreError::NotFound),
                Some(_) => Err(StoreError::InUse(
                    "this donation is already converted".into(),
                )),
            };
        }
        Ok(())
    }

    /// Which sources use a project, and how much each contributes to its score
    /// (a disabled source contributes nothing).
    pub async fn usage_breakdown(&self, name: &str) -> Result<Vec<UsageEntry>, StoreError> {
        sqlx::query(
            "SELECT s.id AS source_id, s.provider, s.repo, s.enabled, s.weight, u.via, u.kind
               FROM usage u
               JOIN source s ON s.id = u.source_id
               JOIN project p ON p.id = u.project_id
              WHERE p.name = ?
              ORDER BY s.repo COLLATE NOCASE, u.via",
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            let provider: String = r.try_get("provider")?;
            let kind: String = r.try_get("kind")?;
            let enabled: bool = r.try_get("enabled")?;
            let weight: f64 = r.try_get("weight")?;
            let factor = if kind == "direct" {
                1.0
            } else {
                TRANSITIVE_FACTOR
            };
            Ok(UsageEntry {
                source_id: r.try_get("source_id")?,
                provider: Provider::parse(&provider)
                    .ok_or_else(|| corrupt("provider", &provider))?,
                repo: r.try_get("repo")?,
                enabled,
                weight,
                via: r.try_get("via")?,
                direct: kind == "direct",
                contribution: if enabled { weight * factor } else { 0.0 },
            })
        })
        .collect()
    }

    // --- projects ---

    /// All projects, ordered by name. The usage score is what enabled sources
    /// contribute (their weight, in full for a direct dependency and scaled by
    /// [`TRANSITIVE_FACTOR`] for a transitive one) plus the usage an admin
    /// assigned by hand.
    pub async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let rows = sqlx::query(
            "SELECT p.name, p.repo, p.origin,
                    COALESCE(SUM(per.weight * per.factor), 0.0) AS scanned,
                    COALESCE(o.manual_usage, 0.0) AS manual_usage,
                    COALESCE(o.ignore, 0) AS ignore,
                    COALESCE(o.notes, '') AS notes,
                    COALESCE(don.n, 0) AS donation_count,
                    COALESCE(don.base, 0) AS donated_base,
                    don.last_on AS last_on
               FROM project p
               LEFT JOIN project_override o ON o.project_id = p.id
               LEFT JOIN (
                    SELECT d.project_id, COUNT(*) AS n,
                           COALESCE(SUM(d.base_amount_minor), 0) AS base,
                           MAX(d.donated_on) AS last_on
                      FROM donation d
                     WHERE d.voids_id IS NULL
                       AND NOT EXISTS (SELECT 1 FROM donation v WHERE v.voids_id = d.id)
                     GROUP BY d.project_id
               ) don ON don.project_id = p.id
               LEFT JOIN (
                    -- One row per (source, project): when both an image scan and
                    -- a dependency scan found it, the stronger kind counts once.
                    SELECT u.project_id, u.source_id, s.weight,
                           MAX(CASE u.kind WHEN 'direct' THEN 1.0 ELSE ?1 END) AS factor
                      FROM usage u
                      JOIN source s ON s.id = u.source_id AND s.enabled = 1
                     GROUP BY u.project_id, u.source_id
               ) per ON per.project_id = p.id
              GROUP BY p.id
              ORDER BY p.name",
        )
        .bind(TRANSITIVE_FACTOR)
        .fetch_all(&self.pool)
        .await?;
        let mut funding: HashMap<String, Vec<Funding>> = HashMap::new();
        for row in sqlx::query(
            "SELECT p.name, f.platform, f.handle, f.url, f.origin
               FROM project_funding f JOIN project p ON p.id = f.project_id
              ORDER BY p.name, f.platform, f.handle",
        )
        .fetch_all(&self.pool)
        .await?
        {
            let name: String = row.try_get("name")?;
            let origin: String = row.try_get("origin")?;
            funding.entry(name).or_default().push(Funding {
                platform: row.try_get("platform")?,
                handle: row.try_get("handle")?,
                url: row.try_get("url")?,
                manual: origin == "manual",
            });
        }
        rows.iter()
            .map(|row| {
                let name: String = row.try_get("name")?;
                let origin: String = row.try_get("origin")?;
                let scanned: f64 = row.try_get("scanned")?;
                let manual_usage: f64 = row.try_get("manual_usage")?;
                Ok(Project {
                    funding: funding.remove(&name).unwrap_or_default(),
                    repo: row.try_get("repo")?,
                    origin: Origin::parse(&origin).ok_or_else(|| corrupt("origin", &origin))?,
                    usage_score: scanned + manual_usage,
                    manual_usage,
                    ignored: row.try_get("ignore")?,
                    notes: row.try_get("notes")?,
                    last_donation: row
                        .try_get::<Option<String>, _>("last_on")?
                        .map(|day| {
                            format!("{day}T00:00:00Z")
                                .parse()
                                .map_err(|_| corrupt("donation date", &day))
                        })
                        .transpose()?,
                    donation_count: row.try_get("donation_count")?,
                    donated_base_minor: row.try_get("donated_base")?,
                    name,
                })
            })
            .collect()
    }

    pub async fn get_project(&self, name: &str) -> Result<Option<Project>, StoreError> {
        Ok(self
            .list_projects()
            .await?
            .into_iter()
            .find(|p| p.name == name))
    }

    /// Adds a project by hand. `repo` is canonical (see
    /// [`crate::scan::resolve::parse_project_repo`]).
    pub async fn create_project(
        &self,
        repo: &str,
        manual_usage: f64,
        notes: &str,
        by: &Actor,
        now: Timestamp,
    ) -> Result<Project, StoreError> {
        let stamp = now.to_string();
        let mut tx = self.pool.begin().await?;
        if sqlx::query_scalar::<_, i64>("SELECT id FROM project WHERE repo = ?")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(StoreError::Duplicate);
        }
        let name = free_name(&mut tx, repo).await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO project (name, repo, origin, first_seen_at, last_seen_at)
             VALUES (?, ?, 'manual', ?, ?) RETURNING id",
        )
        .bind(&name)
        .bind(repo)
        .bind(&stamp)
        .bind(&stamp)
        .fetch_one(&mut *tx)
        .await?;
        write_override(&mut tx, id, manual_usage, false, notes, by, &stamp).await?;
        tx.commit().await?;
        self.get_project(&name).await?.ok_or(StoreError::NotFound)
    }

    /// Changes an admin's settings for a project (any project, not only manual ones).
    pub async fn update_project(
        &self,
        name: &str,
        update: &ProjectUpdate,
        by: &Actor,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let id = project_id(&mut tx, name).await?;
        write_override(
            &mut tx,
            id,
            update.manual_usage,
            update.ignore,
            &update.notes,
            by,
            &now.to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes a project that was added by hand. A discovered one goes away
    /// by itself when no source uses it, so it cannot be deleted here.
    pub async fn delete_project(&self, name: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let id = project_id(&mut tx, name).await?;
        let origin: String = sqlx::query_scalar("SELECT origin FROM project WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if origin != "manual" {
            return Err(StoreError::InUse(
                "this project was found by a scan; it disappears when no source uses it".into(),
            ));
        }
        sqlx::query("DELETE FROM project WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if is_foreign_key_violation(&e) {
                    StoreError::InUse("this project has donations, which are never deleted".into())
                } else {
                    StoreError::Db(e)
                }
            })?;
        tx.commit().await?;
        Ok(())
    }

    /// Adds a way to donate by hand. `handle` and `url` are validated (see
    /// [`crate::funding::parse`]).
    pub async fn add_funding(
        &self,
        name: &str,
        platform: &str,
        handle: &str,
        url: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let id = project_id(&mut tx, name).await?;
        sqlx::query(
            "INSERT INTO project_funding (project_id, platform, handle, url, origin)
             VALUES (?, ?, ?, ?, 'manual')",
        )
        .bind(id)
        .bind(platform)
        .bind(handle)
        .bind(url)
        .execute(&mut *tx)
        .await
        .map_err(map_unique)?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes a way to donate that was added by hand.
    pub async fn remove_funding(
        &self,
        name: &str,
        platform: &str,
        handle: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let id = project_id(&mut tx, name).await?;
        let done = sqlx::query(
            "DELETE FROM project_funding
              WHERE project_id = ? AND platform = ? AND handle = ? AND origin = 'manual'",
        )
        .bind(id)
        .bind(platform)
        .bind(handle)
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn project_id(tx: &mut sqlx::SqliteConnection, name: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT id FROM project WHERE name = ?")
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)
}

async fn write_override(
    tx: &mut sqlx::SqliteConnection,
    project_id: i64,
    manual_usage: f64,
    ignore: bool,
    notes: &str,
    by: &Actor,
    stamp: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO project_override (project_id, manual_usage, ignore, notes, updated_by, updated_by_name, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (project_id) DO UPDATE SET
             manual_usage = excluded.manual_usage, ignore = excluded.ignore,
             notes = excluded.notes, updated_by = excluded.updated_by,
             updated_by_name = excluded.updated_by_name, updated_at = excluded.updated_at",
    )
    .bind(project_id)
    .bind(manual_usage)
    .bind(ignore)
    .bind(notes)
    .bind(&by.sub)
    .bind(&by.name)
    .bind(stamp)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The slug for a new project: the short one, or one including the host when
/// another repository already has it.
async fn free_name(tx: &mut sqlx::SqliteConnection, repo: &str) -> Result<String, StoreError> {
    let short = slug(repo, false);
    let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM project WHERE name = ?")
        .bind(&short)
        .fetch_optional(&mut *tx)
        .await?;
    Ok(if taken.is_some() || short.is_empty() {
        slug(repo, true)
    } else {
        short
    })
}

/// Inserts the project if it is new, otherwise marks it as seen, and returns
/// its id. A new project's slug falls back to one that includes the host when
/// another repository already has the short one.
async fn upsert_project(
    tx: &mut sqlx::SqliteConnection,
    repo: &str,
    stamp: &str,
) -> Result<i64, StoreError> {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM project WHERE repo = ?")
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?
    {
        sqlx::query("UPDATE project SET last_seen_at = ? WHERE id = ?")
            .bind(stamp)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        return Ok(id);
    }
    let name = free_name(tx, repo).await?;
    Ok(sqlx::query_scalar(
        "INSERT INTO project (name, repo, first_seen_at, last_seen_at) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(repo)
    .bind(stamp)
    .bind(stamp)
    .fetch_one(&mut *tx)
    .await?)
}

/// One source's use of a project.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageEntry {
    pub source_id: i64,
    pub provider: Provider,
    pub repo: String,
    pub enabled: bool,
    pub weight: f64,
    /// Which scan found it: `images` or `dependencies`.
    pub via: String,
    pub direct: bool,
    /// What it adds to the project's usage score.
    pub contribution: f64,
}

/// A donation waiting for its amount in the base currency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingConversion {
    pub id: i64,
    pub amount_minor: i64,
    pub currency: String,
    pub donated_on: jiff::civil::Date,
}

/// An image repository to ask its registry about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupTarget {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageMapping {
    pub registry: String,
    pub repository: String,
    pub project_repo: String,
    pub created_by: Actor,
    pub created_at: Timestamp,
}

/// One image repository in a source, as the UI lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageReport {
    pub registry: String,
    pub repository: String,
    /// Comma-separated tags seen, if any.
    pub tags: String,
    /// The first file it appears in.
    pub file: String,
    /// How many places in the source use it.
    pub uses: i64,
    pub resolved: Resolved,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSummary {
    pub projects: usize,
    /// Image repositories no project could be found for.
    pub unresolved: usize,
    /// Projects removed because no source uses them any more.
    pub pruned: usize,
}

/// SQLite reports a refused `ON DELETE RESTRICT` as a constraint error of its
/// trigger variety (code 1811) rather than the foreign-key one (787), so
/// sqlx does not call it a foreign-key violation; the message is the same.
fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().is_some_and(|db| {
        db.is_foreign_key_violation() || db.message().contains("FOREIGN KEY constraint failed")
    })
}

fn map_unique(e: sqlx::Error) -> StoreError {
    match e.as_database_error() {
        Some(db) if db.is_unique_violation() => StoreError::Duplicate,
        _ => StoreError::Db(e),
    }
}

fn corrupt(what: &str, value: &str) -> StoreError {
    StoreError::Corrupt(format!("invalid {what} {value:?} in the database"))
}

fn timestamp(row: &SqliteRow, column: &str) -> Result<Timestamp, StoreError> {
    let raw: String = row.try_get(column)?;
    raw.parse().map_err(|_| corrupt(column, &raw))
}

fn donation_from_row(row: &SqliteRow) -> Result<Donation, StoreError> {
    let day: String = row.try_get("donated_on")?;
    let kind: String = row.try_get("kind")?;
    let conversion = match (
        row.try_get::<Option<i64>, _>("base_amount_minor")?,
        row.try_get::<Option<f64>, _>("fx_rate")?,
        row.try_get::<Option<String>, _>("fx_source")?,
    ) {
        (Some(base_minor), Some(rate), Some(source)) => Some(Conversion {
            base_minor,
            rate,
            source: FxSource::parse(&source).ok_or_else(|| corrupt("fx source", &source))?,
        }),
        _ => None,
    };
    let voided = match row.try_get::<Option<String>, _>("voided_at")? {
        Some(at) => Some((
            Actor::new(
                row.try_get::<String, _>("voided_by")?,
                row.try_get::<String, _>("voided_by_name")?,
            ),
            at.parse().map_err(|_| corrupt("voided_at", &at))?,
        )),
        None => None,
    };
    Ok(Donation {
        id: row.try_get("id")?,
        project: row.try_get("project_name")?,
        project_repo: row.try_get("project_repo")?,
        platform: row.try_get("platform")?,
        amount_minor: row.try_get("amount_minor")?,
        currency: row.try_get("currency")?,
        donated_on: day.parse().map_err(|_| corrupt("donated_on", &day))?,
        conversion,
        kind: DonationKind::parse(&kind).ok_or_else(|| corrupt("donation kind", &kind))?,
        note: row.try_get("note")?,
        recorded_by: Actor::new(
            row.try_get::<String, _>("recorded_by")?,
            row.try_get::<String, _>("recorded_by_name")?,
        ),
        recorded_at: timestamp(row, "recorded_at")?,
        voided,
    })
}

fn source_from_row(row: &SqliteRow) -> Result<Source, StoreError> {
    let provider: String = row.try_get("provider")?;
    let method: String = row.try_get("scan_method")?;
    let last_scan_at: Option<String> = row.try_get("last_scan_at")?;
    Ok(Source {
        id: row.try_get("id")?,
        provider: Provider::parse(&provider).ok_or_else(|| corrupt("provider", &provider))?,
        repo: row.try_get("repo")?,
        weight: row.try_get("weight")?,
        scan_method: ScanMethod::parse(&method).ok_or_else(|| corrupt("scan method", &method))?,
        enabled: row.try_get("enabled")?,
        last_scan_at: last_scan_at
            .map(|raw| raw.parse().map_err(|_| corrupt("last_scan_at", &raw)))
            .transpose()?,
        last_scan_ok: row.try_get("last_scan_ok")?,
        last_scan_status: row.try_get("last_scan_status")?,
        created_by: Actor::new(
            row.try_get::<String, _>("created_by")?,
            row.try_get::<String, _>("created_by_name")?,
        ),
        created_at: timestamp(row, "created_at")?,
        updated_by: Actor::new(
            row.try_get::<String, _>("updated_by")?,
            row.try_get::<String, _>("updated_by_name")?,
        ),
        updated_at: timestamp(row, "updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Actor {
        Actor::new("u1", "alice")
    }

    fn now() -> Timestamp {
        "2026-09-26T12:00:00Z".parse().unwrap()
    }

    fn new_source(repo: &str) -> NewSource {
        NewSource {
            provider: Provider::Github,
            repo: repo.into(),
            weight: 1.0,
            scan_method: ScanMethod::Auto,
        }
    }

    #[tokio::test]
    async fn sources_round_trip() {
        let store = Store::memory().await.unwrap();
        assert!(store.ready().await);
        assert!(store.list_sources().await.unwrap().is_empty());

        let created = store
            .create_source(&new_source("PedDavid/infra"), &actor(), now())
            .await
            .unwrap();
        assert_eq!(created.repo, "PedDavid/infra");
        assert!(created.enabled);
        assert_eq!(created.created_by, actor());
        assert_eq!(created.created_at, now());
        assert_eq!(
            store.get_source(created.id).await.unwrap(),
            Some(created.clone())
        );

        let later: Timestamp = "2026-10-01T08:30:00Z".parse().unwrap();
        let bob = Actor::new("u2", "bob");
        let updated = store
            .update_source(
                created.id,
                &SourceUpdate {
                    weight: 0.5,
                    scan_method: ScanMethod::Lockfiles,
                    enabled: false,
                },
                &bob,
                later,
            )
            .await
            .unwrap();
        assert_eq!(updated.weight, 0.5);
        assert_eq!(updated.scan_method, ScanMethod::Lockfiles);
        assert!(!updated.enabled);
        assert_eq!(updated.updated_by, bob);
        assert_eq!(updated.updated_at, later);
        // Creation details are kept.
        assert_eq!(updated.created_by, actor());
        assert_eq!(updated.created_at, now());

        store.delete_source(created.id).await.unwrap();
        assert!(store.get_source(created.id).await.unwrap().is_none());
        assert!(matches!(
            store.delete_source(created.id).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn data_survives_reopening_the_database_file() {
        let dir = std::env::temp_dir().join(format!(
            "donarium-test-{}-{}",
            std::process::id(),
            Timestamp::now().as_nanosecond()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}", dir.join("donarium.db").display());

        let first = Store::connect(&url).await.unwrap();
        let created = first
            .create_source(&new_source("PedDavid/infra"), &actor(), now())
            .await
            .unwrap();
        first.pool().close().await;

        // Reopening applies no migration twice and finds the data.
        let second = Store::connect(&url).await.unwrap();
        assert_eq!(second.list_sources().await.unwrap(), [created]);
        second.pool().close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The 0002 migration rebuilds `source` and `usage`. Dropping `source` with
    /// foreign keys on would cascade-delete every usage row, so check that real
    /// data from the 0001 schema survives it.
    #[tokio::test]
    async fn migrating_to_image_scans_keeps_existing_data() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                SqliteConnectOptions::from_str("sqlite::memory:")
                    .unwrap()
                    .foreign_keys(true),
            )
            .await
            .unwrap();

        // Only the first migration, as an older version of the app left it.
        let mut first = sqlx::migrate!("./migrations");
        first.migrations = first.migrations.iter().take(1).cloned().collect();
        first.run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO source (provider, repo, weight, scan_method, enabled, created_by, created_at, updated_by, updated_at)
             VALUES ('github', 'me/infra', 0.5, 'lockfiles', 0, 'u1', '2026-09-26T12:00:00Z', 'u1', '2026-09-26T12:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO project (id, name, repo, first_seen_at, last_seen_at) VALUES (1, 'a-b', 'github.com/a/b', 'x', 'x')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage (source_id, project_id, kind) VALUES (1, 1, 'transitive')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO project_funding (project_id, platform, handle) VALUES (1, 'github-sponsors', 'a')")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let store = Store { pool };
        let sources = store.list_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].repo, "me/infra");
        assert_eq!(sources[0].weight, 0.5);
        assert_eq!(sources[0].scan_method, ScanMethod::Lockfiles);
        assert!(!sources[0].enabled);
        assert_eq!(sources[0].last_scan_ok, None);
        let usage: (String, String) =
            sqlx::query_as("SELECT via, kind FROM usage WHERE source_id = 1")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(usage, ("dependencies".into(), "transitive".into()));
        let funding: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_funding")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(funding, 1, "tables that reference project are untouched");
        let origin: String = sqlx::query_scalar("SELECT origin FROM project WHERE id = 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(origin, "discovered");
        // Foreign keys are back on and consistent.
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(violations.is_empty());
        let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(on, 1);
        // The new scan method is accepted, an unknown one still is not.
        sqlx::query("UPDATE source SET scan_method = 'images'")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE source SET scan_method = 'nonsense'")
                .execute(store.pool())
                .await
                .is_err()
        );
        // And deleting the source still cascades to its usage.
        store.delete_source(1).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn usage_breakdown_lists_each_source_with_its_contribution() {
        let store = Store::memory().await.unwrap();
        let heavy = store
            .create_source(&new_source("me/heavy"), &actor(), now())
            .await
            .unwrap();
        let mut light = new_source("me/light");
        light.weight = 0.4;
        let light = store.create_source(&light, &actor(), now()).await.unwrap();
        sqlx::query("INSERT INTO project (id, name, repo, first_seen_at, last_seen_at) VALUES (1, 'a-b', 'github.com/a/b', 'x', 'x')")
            .execute(store.pool())
            .await
            .unwrap();
        for (source, via, kind) in [
            (heavy.id, "images", "direct"),
            (heavy.id, "dependencies", "transitive"),
            (light.id, "dependencies", "transitive"),
        ] {
            sqlx::query("INSERT INTO usage (source_id, project_id, via, kind) VALUES (?, 1, ?, ?)")
                .bind(source)
                .bind(via)
                .bind(kind)
                .execute(store.pool())
                .await
                .unwrap();
        }
        let usage = store.usage_breakdown("a-b").await.unwrap();
        assert_eq!(usage.len(), 3);
        assert_eq!(usage[0].repo, "me/heavy");
        assert!(
            usage
                .iter()
                .any(|u| u.via == "images" && u.direct && u.contribution == 1.0)
        );
        assert!(
            usage
                .iter()
                .any(|u| u.repo == "me/light" && !u.direct && (u.contribution - 0.1).abs() < 1e-9)
        );
        // A disabled source contributes nothing, but is still listed.
        store
            .update_source(
                light.id,
                &SourceUpdate {
                    weight: 0.4,
                    scan_method: ScanMethod::Auto,
                    enabled: false,
                },
                &actor(),
                now(),
            )
            .await
            .unwrap();
        let usage = store.usage_breakdown("a-b").await.unwrap();
        assert!(
            usage
                .iter()
                .filter(|u| u.repo == "me/light")
                .all(|u| !u.enabled && u.contribution == 0.0)
        );
        assert!(store.usage_breakdown("nope").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_projects_have_their_own_usage_and_are_never_pruned() {
        let store = Store::memory().await.unwrap();
        let alice = actor();
        let project = store
            .create_project(
                "github.com/paperless-ngx/paperless-ngx",
                3.0,
                "runs my documents",
                &alice,
                now(),
            )
            .await
            .unwrap();
        assert_eq!(project.name, "paperless-ngx-paperless-ngx");
        assert_eq!(project.origin, Origin::Manual);
        assert_eq!(project.usage_score, 3.0);
        assert_eq!(project.manual_usage, 3.0);
        assert_eq!(project.notes, "runs my documents");
        assert!(!project.ignored);

        // Adding the same repository again is refused.
        assert!(matches!(
            store
                .create_project(
                    "github.com/paperless-ngx/paperless-ngx",
                    1.0,
                    "",
                    &alice,
                    now()
                )
                .await,
            Err(StoreError::Duplicate)
        ));

        // No source uses it, and a rebuild does not remove it.
        store.rebuild_image_usage(now()).await.unwrap();
        assert!(store.get_project(&project.name).await.unwrap().is_some());

        // What a scan finds adds to what was assigned by hand.
        let source = store
            .create_source(&new_source("me/infra"), &alice, now())
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage (source_id, project_id, via, kind) SELECT ?, id, 'images', 'direct' FROM project")
            .bind(source.id)
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store
                .get_project(&project.name)
                .await
                .unwrap()
                .unwrap()
                .usage_score,
            4.0
        );

        // Changing settings, including ignoring it.
        store
            .update_project(
                &project.name,
                &ProjectUpdate {
                    manual_usage: 0.5,
                    ignore: true,
                    notes: "x".into(),
                },
                &Actor::new("u2", "bob"),
                now(),
            )
            .await
            .unwrap();
        let changed = store.get_project(&project.name).await.unwrap().unwrap();
        assert_eq!(
            (changed.usage_score, changed.ignored, changed.notes.as_str()),
            (1.5, true, "x")
        );
        assert!(matches!(
            store
                .update_project(
                    "nope",
                    &ProjectUpdate {
                        manual_usage: 0.0,
                        ignore: false,
                        notes: String::new()
                    },
                    &alice,
                    now()
                )
                .await,
            Err(StoreError::NotFound)
        ));

        // Deleting a manual project works; its settings go with it.
        store.delete_project(&project.name).await.unwrap();
        assert!(store.get_project(&project.name).await.unwrap().is_none());
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_override")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert!(matches!(
            store.delete_project(&project.name).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn discovered_projects_cannot_be_deleted_by_hand() {
        let store = Store::memory().await.unwrap();
        sqlx::query("INSERT INTO project (name, repo, first_seen_at, last_seen_at) VALUES ('a-b', 'github.com/a/b', 'x', 'x')")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(matches!(
            store.delete_project("a-b").await,
            Err(StoreError::InUse(_))
        ));
        assert!(store.get_project("a-b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn ways_to_donate_added_by_hand() {
        let store = Store::memory().await.unwrap();
        store
            .create_project("github.com/a/b", 1.0, "", &actor(), now())
            .await
            .unwrap();
        store
            .add_funding("a-b", "opencollective", "ab", None)
            .await
            .unwrap();
        store
            .add_funding(
                "a-b",
                "other",
                "https://example.org/donate",
                Some("https://example.org/donate"),
            )
            .await
            .unwrap();
        assert!(matches!(
            store.add_funding("a-b", "opencollective", "ab", None).await,
            Err(StoreError::Duplicate)
        ));
        let funding = store.get_project("a-b").await.unwrap().unwrap().funding;
        assert_eq!(funding.len(), 2);
        assert!(funding.iter().all(|f| f.manual));

        // A discovered way to donate cannot be removed by hand.
        sqlx::query("INSERT INTO project_funding (project_id, platform, handle, origin) SELECT id, 'github-sponsors', 'ab', 'discovered' FROM project")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(matches!(
            store.remove_funding("a-b", "github-sponsors", "ab").await,
            Err(StoreError::NotFound)
        ));
        store
            .remove_funding("a-b", "opencollective", "ab")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_project("a-b")
                .await
                .unwrap()
                .unwrap()
                .funding
                .len(),
            2
        );
        assert!(matches!(
            store.add_funding("nope", "kofi", "x", None).await,
            Err(StoreError::NotFound)
        ));
    }

    // --- donations ---

    use jiff::civil::date;

    async fn store_with_project() -> Store {
        let store = Store::memory().await.unwrap();
        store
            .create_project("github.com/tokio-rs/tokio", 1.0, "", &actor(), now())
            .await
            .unwrap();
        store
            .create_project("github.com/serde-rs/serde", 1.0, "", &actor(), now())
            .await
            .unwrap();
        store
    }

    fn donation(
        project: &str,
        minor: i64,
        currency: &str,
        day: jiff::civil::Date,
        base: Option<i64>,
    ) -> NewDonation {
        NewDonation {
            project: project.into(),
            platform: "github-sponsors".into(),
            amount_minor: minor,
            currency: currency.into(),
            donated_on: day,
            kind: DonationKind::OneOff,
            note: String::new(),
            conversion: base.map(|b| Conversion {
                base_minor: b,
                rate: b as f64 / minor as f64,
                source: if currency == "EUR" {
                    FxSource::Same
                } else {
                    FxSource::Manual
                },
            }),
        }
    }

    #[tokio::test]
    async fn donations_are_recorded_and_listed_newest_first() {
        let store = store_with_project().await;
        let first = store
            .record_donation(
                &donation("tokio-rs-tokio", 1000, "EUR", date(2026, 3, 1), Some(1000)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        assert_eq!(first.project, "tokio-rs-tokio");
        assert_eq!(first.project_repo, "github.com/tokio-rs/tokio");
        assert_eq!(first.amount_minor, 1000);
        assert_eq!(first.donated_on, date(2026, 3, 1));
        assert_eq!(first.conversion.unwrap().source, FxSource::Same);
        assert_eq!(first.recorded_by, actor());
        assert_eq!(first.voided, None);
        store
            .record_donation(
                &donation("serde-rs-serde", 500, "USD", date(2026, 9, 1), Some(460)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        store
            .record_donation(
                &donation("tokio-rs-tokio", 250, "USD", date(2026, 6, 1), None),
                &actor(),
                now(),
            )
            .await
            .unwrap();

        let all = store.list_donations(None).await.unwrap();
        let days: Vec<_> = all.iter().map(|d| d.donated_on).collect();
        assert_eq!(days, [date(2026, 9, 1), date(2026, 6, 1), date(2026, 3, 1)]);
        assert_eq!(all[1].conversion, None, "pending until converted");
        let tokio = store.list_donations(Some("tokio-rs-tokio")).await.unwrap();
        assert_eq!(tokio.len(), 2);
        assert!(store.list_donations(Some("nope")).await.unwrap().is_empty());
        assert!(matches!(
            store
                .record_donation(
                    &donation("nope", 100, "EUR", date(2026, 1, 1), Some(100)),
                    &actor(),
                    now()
                )
                .await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_database_refuses_nonsense_donations() {
        let store = store_with_project().await;
        let record = |d: NewDonation| {
            let store = store.clone();
            async move { store.record_donation(&d, &actor(), now()).await }
        };
        assert!(
            record(donation("tokio-rs-tokio", 0, "EUR", date(2026, 1, 1), None))
                .await
                .is_err()
        );
        assert!(
            record(donation(
                "tokio-rs-tokio",
                -5,
                "EUR",
                date(2026, 1, 1),
                None
            ))
            .await
            .is_err()
        );
        assert!(
            record(donation(
                "tokio-rs-tokio",
                100,
                "eur",
                date(2026, 1, 1),
                None
            ))
            .await
            .is_err()
        );
        assert!(
            record(donation(
                "tokio-rs-tokio",
                100,
                "EURO",
                date(2026, 1, 1),
                None
            ))
            .await
            .is_err()
        );
        // A base amount without a rate (or the other way round) is inconsistent.
        let mut half = donation("tokio-rs-tokio", 100, "EUR", date(2026, 1, 1), Some(100));
        sqlx::query("DELETE FROM project WHERE 0")
            .execute(store.pool())
            .await
            .unwrap();
        let ok = record(half.clone()).await.unwrap();
        assert!(ok.conversion.is_some());
        let broken = sqlx::query(
            "INSERT INTO donation (project_id, platform, amount_minor, currency, donated_on, base_amount_minor,
                                   kind, note, recorded_by, recorded_at)
             SELECT id, 'x', 100, 'EUR', '2026-01-01', 100, 'one_off', '', 'u', 'x' FROM project LIMIT 1",
        )
        .execute(store.pool())
        .await;
        assert!(broken.is_err(), "a base amount needs its rate and source");
        half.conversion = None;
        assert!(record(half).await.is_ok());
    }

    #[tokio::test]
    async fn donations_are_append_only() {
        let store = store_with_project().await;
        let d = store
            .record_donation(
                &donation("tokio-rs-tokio", 1000, "EUR", date(2026, 3, 1), Some(1000)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        let pending = store
            .record_donation(
                &donation("tokio-rs-tokio", 500, "USD", date(2026, 3, 2), None),
                &actor(),
                now(),
            )
            .await
            .unwrap();

        // Nothing can be deleted...
        let delete = sqlx::query("DELETE FROM donation WHERE id = ?")
            .bind(d.id)
            .execute(store.pool())
            .await;
        assert!(delete.unwrap_err().to_string().contains("never deleted"));
        // ...or edited, whatever the column.
        for column_change in [
            "amount_minor = 1",
            "currency = 'USD'",
            "donated_on = '2000-01-01'",
            "note = 'changed'",
            "platform = 'other'",
            "kind = 'recurring'",
            "recorded_by = 'someone else'",
            "base_amount_minor = 1, fx_rate = 1.0, fx_source = 'manual'",
        ] {
            let result = sqlx::query(&format!("UPDATE donation SET {column_change} WHERE id = ?"))
                .bind(d.id)
                .execute(store.pool())
                .await;
            assert!(result.is_err(), "{column_change} must be refused");
        }
        assert_eq!(store.get_donation(d.id).await.unwrap().unwrap(), d);

        // The one exception: a pending conversion can be filled in, once.
        sqlx::query("UPDATE donation SET base_amount_minor = 460, fx_rate = 0.92, fx_source = 'ecb' WHERE id = ?")
            .bind(pending.id)
            .execute(store.pool())
            .await
            .unwrap();
        let filled = store.get_donation(pending.id).await.unwrap().unwrap();
        assert_eq!(filled.conversion.unwrap().base_minor, 460);
        let again = sqlx::query("UPDATE donation SET base_amount_minor = 1, fx_rate = 1.0, fx_source = 'manual' WHERE id = ?")
            .bind(pending.id)
            .execute(store.pool())
            .await;
        assert!(again.is_err(), "a conversion is not changed afterwards");
        let other = sqlx::query("UPDATE donation SET note = 'x' WHERE id = ?")
            .bind(pending.id)
            .execute(store.pool())
            .await;
        assert!(other.is_err());
    }

    #[tokio::test]
    async fn voiding_keeps_the_history_and_stops_counting() {
        let store = store_with_project().await;
        let keep = store
            .record_donation(
                &donation("tokio-rs-tokio", 1000, "EUR", date(2026, 3, 1), Some(1000)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        let wrong = store
            .record_donation(
                &donation("tokio-rs-tokio", 9000, "EUR", date(2026, 4, 1), Some(9000)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        let bob = Actor::new("u2", "bob");
        let later: Timestamp = "2026-10-01T08:00:00Z".parse().unwrap();
        store.void_donation(wrong.id, &bob, later).await.unwrap();

        // Still listed, marked, with who and when; the void entry is not listed.
        let all = store.list_donations(None).await.unwrap();
        assert_eq!(all.len(), 2);
        let voided = all.iter().find(|d| d.id == wrong.id).unwrap();
        assert_eq!(voided.voided, Some((bob.clone(), later)));
        assert_eq!(all.iter().find(|d| d.id == keep.id).unwrap().voided, None);
        let raw: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM donation")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(raw, 3, "the void is its own row");

        // It no longer counts anywhere.
        let totals = store.donation_totals(None).await.unwrap();
        assert_eq!((totals.count, totals.base_minor), (1, 1000));
        let project = store.get_project("tokio-rs-tokio").await.unwrap().unwrap();
        assert_eq!(
            (project.donation_count, project.donated_base_minor),
            (1, 1000)
        );
        assert_eq!(
            project.last_donation,
            Some("2026-03-01T00:00:00Z".parse().unwrap())
        );

        // Not twice, not a void, not something that does not exist.
        assert!(
            matches!(store.void_donation(wrong.id, &bob, later).await, Err(StoreError::InUse(m)) if m.contains("already"))
        );
        let void_id: i64 = sqlx::query_scalar("SELECT id FROM donation WHERE voids_id = ?")
            .bind(wrong.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(matches!(
            store.void_donation(void_id, &bob, later).await,
            Err(StoreError::InUse(_))
        ));
        assert!(matches!(
            store.void_donation(9999, &bob, later).await,
            Err(StoreError::NotFound)
        ));
        assert_eq!(
            store.get_donation(void_id).await.unwrap(),
            None,
            "a void entry is not a donation"
        );
    }

    #[tokio::test]
    async fn totals_by_currency_and_year() {
        let store = store_with_project().await;
        let record = |p: &str, minor, cur: &str, day, base| {
            let (store, d) = (store.clone(), donation(p, minor, cur, day, base));
            async move { store.record_donation(&d, &actor(), now()).await.unwrap() }
        };
        record(
            "tokio-rs-tokio",
            1000,
            "EUR",
            date(2025, 12, 31),
            Some(1000),
        )
        .await;
        record("tokio-rs-tokio", 2000, "EUR", date(2026, 1, 1), Some(2000)).await;
        record("serde-rs-serde", 500, "USD", date(2026, 2, 1), Some(460)).await;
        record("serde-rs-serde", 300, "USD", date(2026, 2, 2), None).await;
        record(
            "serde-rs-serde",
            150_000,
            "JPY",
            date(2026, 3, 3),
            Some(900),
        )
        .await;

        let t = store.donation_totals(None).await.unwrap();
        assert_eq!(t.count, 5);
        assert_eq!(t.base_minor, 1000 + 2000 + 460 + 900);
        assert_eq!(t.unconverted, 1);
        assert_eq!(t.by_year, [(2026, 3360), (2025, 1000)]);
        // In the currencies they were paid in, largest first.
        assert_eq!(
            t.by_currency,
            [
                ("JPY".to_string(), 150_000),
                ("EUR".to_string(), 3000),
                ("USD".to_string(), 800)
            ]
        );

        // Limited to one project.
        let serde = store.donation_totals(Some("serde-rs-serde")).await.unwrap();
        assert_eq!(
            (serde.count, serde.base_minor, serde.unconverted),
            (3, 460 + 900, 1)
        );
        assert_eq!(
            store.donation_totals(Some("nope")).await.unwrap(),
            Totals::default()
        );

        let empty = Store::memory()
            .await
            .unwrap()
            .donation_totals(None)
            .await
            .unwrap();
        assert_eq!(empty, Totals::default());
    }

    #[tokio::test]
    async fn a_project_with_donations_is_never_pruned_or_deleted() {
        let store = Store::memory().await.unwrap();
        // A discovered project, used by a source.
        let source = store
            .create_source(&new_source("me/infra"), &actor(), now())
            .await
            .unwrap();
        sqlx::query("INSERT INTO project (id, name, repo, first_seen_at, last_seen_at) VALUES (1, 'a-b', 'github.com/a/b', 'x', 'x')")
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage (source_id, project_id, via, kind) VALUES (?, 1, 'images', 'direct')")
            .bind(source.id)
            .execute(store.pool())
            .await
            .unwrap();
        store
            .record_donation(
                &donation("a-b", 1000, "EUR", date(2026, 1, 1), Some(1000)),
                &actor(),
                now(),
            )
            .await
            .unwrap();

        // The source goes away, and with it all usage: the project would be
        // pruned, but you donated to it.
        store.delete_source(source.id).await.unwrap();
        store.rebuild_image_usage(now()).await.unwrap();
        let project = store
            .get_project("a-b")
            .await
            .unwrap()
            .expect("kept because of its donation");
        assert_eq!(project.usage_score, 0.0);
        assert_eq!(project.donation_count, 1);

        // The database itself refuses to delete it too.
        let direct = sqlx::query("DELETE FROM project WHERE name = 'a-b'")
            .execute(store.pool())
            .await;
        assert!(direct.is_err());
    }

    #[tokio::test]
    async fn a_manual_project_with_donations_cannot_be_deleted() {
        let store = store_with_project().await;
        store
            .record_donation(
                &donation("serde-rs-serde", 100, "EUR", date(2026, 1, 1), Some(100)),
                &actor(),
                now(),
            )
            .await
            .unwrap();
        let result = store.delete_project("serde-rs-serde").await;
        assert!(matches!(
            result,
            Err(StoreError::InUse(m)) if m.contains("donations")
        ));
        assert!(store.get_project("serde-rs-serde").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_base_currency_is_fixed_for_a_database() {
        let store = Store::memory().await.unwrap();
        store.ensure_base_currency("EUR").await.unwrap();
        store.ensure_base_currency("EUR").await.unwrap();
        let err = store.ensure_base_currency("USD").await.unwrap_err();
        assert!(err.to_string().contains("keeps its totals in EUR"), "{err}");
    }

    #[tokio::test]
    async fn duplicate_sources_are_rejected_ignoring_case() {
        let store = Store::memory().await.unwrap();
        store
            .create_source(&new_source("PedDavid/infra"), &actor(), now())
            .await
            .unwrap();
        let err = store
            .create_source(&new_source("peddavid/INFRA"), &actor(), now())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Duplicate), "{err:?}");
        // The same name on another provider is a different repository.
        let mut gitlab = new_source("peddavid/infra");
        gitlab.provider = Provider::Gitlab;
        store.create_source(&gitlab, &actor(), now()).await.unwrap();
    }

    #[tokio::test]
    async fn updating_a_missing_source_is_not_found() {
        let store = Store::memory().await.unwrap();
        let err = store
            .update_source(
                7,
                &SourceUpdate {
                    weight: 1.0,
                    scan_method: ScanMethod::Auto,
                    enabled: true,
                },
                &actor(),
                now(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    #[tokio::test]
    async fn the_database_rejects_out_of_range_weights() {
        let store = Store::memory().await.unwrap();
        let mut bad = new_source("a/b");
        bad.weight = 0.0;
        assert!(store.create_source(&bad, &actor(), now()).await.is_err());
    }

    #[tokio::test]
    async fn project_usage_scores_follow_source_weights() {
        let store = Store::memory().await.unwrap();
        let heavy = store
            .create_source(&new_source("me/heavy"), &actor(), now())
            .await
            .unwrap();
        let mut light = new_source("me/light");
        light.weight = 0.2;
        let light = store.create_source(&light, &actor(), now()).await.unwrap();

        let pool = store.pool();
        sqlx::query(
            "INSERT INTO project (id, name, repo, first_seen_at, last_seen_at) VALUES
             (1, 'a-b', 'github.com/a/b', 'x', 'x'), (2, 'c-d', 'github.com/c/d', 'x', 'x')",
        )
        .execute(pool)
        .await
        .unwrap();
        for (source, project, kind) in [
            (heavy.id, 1, "direct"),
            (light.id, 1, "direct"),
            (heavy.id, 2, "transitive"),
        ] {
            sqlx::query(
                "INSERT INTO usage (source_id, project_id, via, kind) VALUES (?, ?, 'dependencies', ?)",
            )
                .bind(source)
                .bind(project)
                .bind(kind)
                .execute(pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO project_funding (project_id, platform, handle) VALUES (1, 'github-sponsors', 'a')")
            .execute(pool)
            .await
            .unwrap();

        let projects = store.list_projects().await.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "a-b");
        assert!((projects[0].usage_score - 1.2).abs() < 1e-9);
        assert_eq!(projects[0].funding.len(), 1);
        assert_eq!(projects[0].funding[0].platform, "github-sponsors");
        assert!(!projects[0].funding[0].manual);
        assert!((projects[1].usage_score - TRANSITIVE_FACTOR).abs() < 1e-9);
        assert!(projects[1].funding.is_empty());

        // A disabled source stops counting; deleting one drops its usage.
        store
            .update_source(
                light.id,
                &SourceUpdate {
                    weight: 0.2,
                    scan_method: ScanMethod::Auto,
                    enabled: false,
                },
                &actor(),
                now(),
            )
            .await
            .unwrap();
        assert!((store.list_projects().await.unwrap()[0].usage_score - 1.0).abs() < 1e-9);
        store.delete_source(heavy.id).await.unwrap();
        assert_eq!(store.list_projects().await.unwrap()[0].usage_score, 0.0);

        assert!(store.get_project("c-d").await.unwrap().is_some());
        assert!(store.get_project("nope").await.unwrap().is_none());
    }
}
