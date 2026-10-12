//! The database: sources, discovered projects and (later) donations. SQLite
//! on a volume; the schema lives in `migrations/`.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use jiff::Timestamp;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use crate::model::{
    Actor, NewSource, Project, Provider, ScanMethod, Source, SourceUpdate, TRANSITIVE_FACTOR,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("that source already exists")]
    Duplicate,
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

    // --- projects ---

    /// All discovered projects, ordered by name. The usage score counts
    /// enabled sources only: their weight, in full for a direct dependency and
    /// scaled by [`TRANSITIVE_FACTOR`] for a transitive one.
    pub async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let rows = sqlx::query(
            "SELECT p.name, p.repo,
                    COALESCE(SUM(CASE WHEN s.enabled = 1 THEN
                        s.weight * CASE u.kind WHEN 'direct' THEN 1.0 ELSE ?1 END
                    END), 0.0) AS usage_score
               FROM project p
               LEFT JOIN usage u ON u.project_id = p.id
               LEFT JOIN source s ON s.id = u.source_id
              GROUP BY p.id
              ORDER BY p.name",
        )
        .bind(TRANSITIVE_FACTOR)
        .fetch_all(&self.pool)
        .await?;
        let mut funding: HashMap<String, Vec<String>> = HashMap::new();
        for row in sqlx::query(
            "SELECT p.name, f.platform
               FROM project_funding f JOIN project p ON p.id = f.project_id
              ORDER BY p.name, f.platform",
        )
        .fetch_all(&self.pool)
        .await?
        {
            let name: String = row.try_get("name")?;
            let platform: String = row.try_get("platform")?;
            let list = funding.entry(name).or_default();
            if !list.contains(&platform) {
                list.push(platform);
            }
        }
        rows.iter()
            .map(|row| {
                let name: String = row.try_get("name")?;
                Ok(Project {
                    funding: funding.remove(&name).unwrap_or_default(),
                    repo: row.try_get("repo")?,
                    usage_score: row.try_get("usage_score")?,
                    // Filled in once donations are stored.
                    last_donation: None,
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
            sqlx::query("INSERT INTO usage (source_id, project_id, kind) VALUES (?, ?, ?)")
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
        assert_eq!(projects[0].funding, ["github-sponsors"]);
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
