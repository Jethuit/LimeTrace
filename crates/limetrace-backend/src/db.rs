use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SegmentInsert {
    pub start_ts: i64,
    pub end_ts: i64,
    pub app_id: Option<i64>,
    pub title_id: Option<i64>,
    pub is_idle: bool,
    pub pid: Option<u32>,
    pub pid_create_time: Option<u64>,
}

pub struct Database {
    conn: Connection,
    app_cache: HashMap<(String, String), i64>,
    title_cache: HashMap<String, i64>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create SQLite directory: {}",
                    parent.display()
                )
            })?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("failed to set busy timeout")?;

        conn.execute_batch(
            "\
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS apps (
              id INTEGER PRIMARY KEY,
              exe_name TEXT NOT NULL,
              process_path TEXT NOT NULL,
              UNIQUE(exe_name, process_path)
            );

            CREATE TABLE IF NOT EXISTS titles (
              id INTEGER PRIMARY KEY,
              title TEXT NOT NULL UNIQUE
            );

            CREATE TABLE IF NOT EXISTS segments (
              id INTEGER PRIMARY KEY,
              start_ts INTEGER NOT NULL,
              end_ts INTEGER NOT NULL CHECK (end_ts >= start_ts),
              app_id INTEGER,
              title_id INTEGER,
              is_idle INTEGER NOT NULL DEFAULT 0,
              pid INTEGER,
              pid_create_time INTEGER,
              FOREIGN KEY(app_id) REFERENCES apps(id),
              FOREIGN KEY(title_id) REFERENCES titles(id)
            );

            CREATE INDEX IF NOT EXISTS idx_segments_start ON segments(start_ts);
            CREATE INDEX IF NOT EXISTS idx_segments_app_start ON segments(app_id, start_ts);
            CREATE INDEX IF NOT EXISTS idx_segments_idle_start ON segments(is_idle, start_ts);",
        )
        .context("failed to initialize schema")?;

        Ok(Self {
            conn,
            app_cache: HashMap::new(),
            title_cache: HashMap::new(),
        })
    }

    pub fn upsert_app(&mut self, exe_name: &str, process_path: &str) -> Result<i64> {
        let key = (exe_name.to_owned(), process_path.to_owned());
        if let Some(id) = self.app_cache.get(&key) {
            return Ok(*id);
        }

        self.conn
            .execute(
                "\
                INSERT INTO apps (exe_name, process_path)
                VALUES (?1, ?2)
                ON CONFLICT(exe_name, process_path) DO NOTHING",
                params![exe_name, process_path],
            )
            .context("failed to upsert apps row")?;

        let app_id = self
            .conn
            .query_row(
                "SELECT id FROM apps WHERE exe_name = ?1 AND process_path = ?2",
                params![exe_name, process_path],
                |row| row.get::<_, i64>(0),
            )
            .context("failed to read apps.id after upsert")?;

        self.app_cache.insert(key, app_id);
        Ok(app_id)
    }

    pub fn upsert_title(&mut self, title: &str) -> Result<i64> {
        if let Some(id) = self.title_cache.get(title) {
            return Ok(*id);
        }

        self.conn
            .execute(
                "\
                INSERT INTO titles (title)
                VALUES (?1)
                ON CONFLICT(title) DO NOTHING",
                params![title],
            )
            .context("failed to upsert titles row")?;

        let title_id = self
            .conn
            .query_row(
                "SELECT id FROM titles WHERE title = ?1",
                params![title],
                |row| row.get::<_, i64>(0),
            )
            .context("failed to read titles.id after upsert")?;

        self.title_cache.insert(title.to_owned(), title_id);
        Ok(title_id)
    }

    pub fn insert_segment(&mut self, segment: &SegmentInsert) -> Result<()> {
        if segment.end_ts <= segment.start_ts {
            return Ok(());
        }

        self.conn
            .execute(
                "\
                INSERT INTO segments (
                  start_ts,
                  end_ts,
                  app_id,
                  title_id,
                  is_idle,
                  pid,
                  pid_create_time
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    segment.start_ts,
                    segment.end_ts,
                    segment.app_id,
                    segment.title_id,
                    bool_to_i64(segment.is_idle),
                    segment.pid.map(i64::from),
                    segment.pid_create_time.map(|v| v as i64),
                ],
            )
            .context("failed to insert segment")?;

        Ok(())
    }

    pub fn truncate_active_segments_from(&mut self, cutoff_ts: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .context("failed to start truncate_active_segments_from transaction")?;

        tx.execute(
            "\
            DELETE FROM segments
            WHERE is_idle = 0
              AND start_ts >= ?1",
            params![cutoff_ts],
        )
        .context("failed to delete active segments after idle cutoff")?;

        tx.execute(
            "\
            UPDATE segments
            SET end_ts = ?1
            WHERE is_idle = 0
              AND start_ts < ?1
              AND end_ts > ?1",
            params![cutoff_ts],
        )
        .context("failed to trim active segments at idle cutoff")?;

        tx.commit()
            .context("failed to commit truncate_active_segments_from transaction")?;

        Ok(())
    }
}

fn bool_to_i64(v: bool) -> i64 {
    if v {
        1
    } else {
        0
    }
}
