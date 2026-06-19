/// SQLite clipboard database — schema, migrations, and all queries.
///
/// Used by both `clipd` (read-write) and `clipboard` (read-only via WAL).
/// WAL mode is set on every connection open to allow concurrent access.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A single clipboard entry as stored in the database.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub content: Vec<u8>,
    pub content_hash: i64,
    pub mime: String,
    pub source_app: Option<String>,
    pub created_at: i64,
    pub last_used: i64,
    pub pinned: bool,
}

/// Default maximum number of entries to keep.
const MAX_ENTRIES: i64 = 1000;

/// Default maximum age in seconds (30 days).
const MAX_AGE_SECS: i64 = 30 * 24 * 3600;

/// Returns the default database path: `$XDG_CACHE_HOME/clipd/db.sqlite`
pub fn default_db_path() -> PathBuf {
    let cache = dirs::cache_dir().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".cache")
    });
    cache.join("clipd").join("db.sqlite")
}

/// Apply pragmas that must be set on every connection.
fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -2000;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;"
    )
}

/// Create the schema if it doesn't exist.
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            content       BLOB NOT NULL,
            content_hash  INTEGER NOT NULL,
            mime          TEXT NOT NULL DEFAULT 'text/plain',
            source_app    TEXT,
            created_at    INTEGER NOT NULL,
            last_used     INTEGER NOT NULL,
            pinned        INTEGER NOT NULL DEFAULT 0
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_hash ON entries(content_hash);
        CREATE INDEX IF NOT EXISTS idx_last_used ON entries(last_used);"
    )
}

/// Handle to the clipboard database.
pub struct ClipboardDb {
    conn: Connection,
}

impl ClipboardDb {
    /// Open (or create) the database at the given path.
    /// Sets WAL mode and creates schema if needed.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Open the default database path.
    pub fn open_default() -> rusqlite::Result<Self> {
        Self::open(&default_db_path())
    }

    /// Returns the current unix timestamp in seconds.
    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }

    /// Store a new clipboard entry, or bump `last_used` if content already exists.
    /// Returns the row id (new or existing).
    pub fn store(
        &self,
        content: &[u8],
        content_hash: i64,
        mime: &str,
        source_app: Option<&str>,
    ) -> rusqlite::Result<i64> {
        let now = Self::now();

        // Upsert: insert or update last_used on hash conflict
        self.conn.execute(
            "INSERT INTO entries (content, content_hash, mime, source_app, created_at, last_used)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(content_hash) DO UPDATE SET
                last_used = excluded.last_used,
                source_app = COALESCE(excluded.source_app, entries.source_app)",
            params![content, content_hash, mime, source_app, now],
        )?;

        let id: i64 = self.conn.query_row(
            "SELECT id FROM entries WHERE content_hash = ?1",
            params![content_hash],
            |row| row.get(0),
        )?;

        Ok(id)
    }

    /// Store an entry with explicit timestamps (used for migration).
    pub fn store_with_timestamps(
        &self,
        content: &[u8],
        content_hash: i64,
        mime: &str,
        created_at: i64,
        last_used: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO entries (content, content_hash, mime, source_app, created_at, last_used)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)
             ON CONFLICT(content_hash) DO NOTHING",
            params![content, content_hash, mime, created_at, last_used],
        )?;

        let id: i64 = self.conn.query_row(
            "SELECT id FROM entries WHERE content_hash = ?1",
            params![content_hash],
            |row| row.get(0),
        )?;

        Ok(id)
    }

    /// List entries ordered by last_used descending.
    /// Returns entries without content blobs (for UI listing).
    pub fn list(&self, limit: i64) -> rusqlite::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, content_hash, mime, source_app, created_at, last_used, pinned
             FROM entries
             ORDER BY last_used DESC
             LIMIT ?1"
        )?;

        let entries = stmt.query_map(params![limit], |row| {
            Ok(Entry {
                id: row.get(0)?,
                content: row.get(1)?,
                content_hash: row.get(2)?,
                mime: row.get(3)?,
                source_app: row.get(4)?,
                created_at: row.get(5)?,
                last_used: row.get(6)?,
                pinned: row.get::<_, i64>(7)? != 0,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Like [`list`](Self::list), but does not pull full image blobs into
    /// memory: for `image/*` rows only the first 64 KiB is returned (enough for
    /// `imagesize` to read the dimensions), while text rows keep their full
    /// content. The `clipboard` overlay used to hold every history entry's full
    /// bytes resident — hundreds of MB of screenshots — which swapped out and
    /// made the pop-up slow to fault back in; it now keeps only metadata and a
    /// lazily built thumbnail, re-reading the full blob by id (see
    /// [`get`](Self::get)) when a preview or paste actually needs it.
    pub fn list_meta(&self, limit: i64) -> rusqlite::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,
                    CASE WHEN mime LIKE 'image/%' THEN substr(content, 1, 65536) ELSE content END,
                    content_hash, mime, source_app, created_at, last_used, pinned
             FROM entries
             ORDER BY last_used DESC
             LIMIT ?1"
        )?;

        let entries = stmt.query_map(params![limit], |row| {
            Ok(Entry {
                id: row.get(0)?,
                content: row.get(1)?,
                content_hash: row.get(2)?,
                mime: row.get(3)?,
                source_app: row.get(4)?,
                created_at: row.get(5)?,
                last_used: row.get(6)?,
                pinned: row.get::<_, i64>(7)? != 0,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Search entries by text content (case-insensitive substring match).
    pub fn search(&self, query: &str, limit: i64) -> rusqlite::Result<Vec<Entry>> {
        let pattern = format!("%{}%", query);
        let mut stmt = self.conn.prepare(
            "SELECT id, content, content_hash, mime, source_app, created_at, last_used, pinned
             FROM entries
             WHERE mime LIKE 'text/%' AND CAST(content AS TEXT) LIKE ?1
             ORDER BY last_used DESC
             LIMIT ?2"
        )?;

        let entries = stmt.query_map(params![pattern, limit], |row| {
            Ok(Entry {
                id: row.get(0)?,
                content: row.get(1)?,
                content_hash: row.get(2)?,
                mime: row.get(3)?,
                source_app: row.get(4)?,
                created_at: row.get(5)?,
                last_used: row.get(6)?,
                pinned: row.get::<_, i64>(7)? != 0,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// Get a single entry by id.
    pub fn get(&self, id: i64) -> rusqlite::Result<Option<Entry>> {
        self.conn.query_row(
            "SELECT id, content, content_hash, mime, source_app, created_at, last_used, pinned
             FROM entries WHERE id = ?1",
            params![id],
            |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    content_hash: row.get(2)?,
                    mime: row.get(3)?,
                    source_app: row.get(4)?,
                    created_at: row.get(5)?,
                    last_used: row.get(6)?,
                    pinned: row.get::<_, i64>(7)? != 0,
                })
            },
        ).optional()
    }

    /// Get a single entry by its content hash.
    ///
    /// `content_hash` is uniquely indexed, so this matches at most one row.
    /// Used by clip-sync as the merge-key lookup.
    pub fn get_by_hash(&self, content_hash: i64) -> rusqlite::Result<Option<Entry>> {
        self.conn.query_row(
            "SELECT id, content, content_hash, mime, source_app, created_at, last_used, pinned
             FROM entries WHERE content_hash = ?1",
            params![content_hash],
            |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    content_hash: row.get(2)?,
                    mime: row.get(3)?,
                    source_app: row.get(4)?,
                    created_at: row.get(5)?,
                    last_used: row.get(6)?,
                    pinned: row.get::<_, i64>(7)? != 0,
                })
            },
        ).optional()
    }

    /// All content hashes currently stored, in no particular order.
    ///
    /// This is the set clip-sync advertises to its peer for reconciliation.
    pub fn all_hashes(&self) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT content_hash FROM entries")?;
        let hashes = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hashes)
    }

    /// The highest row id currently in the table, or 0 if empty.
    ///
    /// clip-sync's DB observer uses this as a cheap high-water mark to detect
    /// rows clipd has appended since the last poll.
    pub fn max_id(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM entries", [], |row| row.get(0))
    }

    /// Content hashes of all rows with `id` strictly greater than `after_id`.
    ///
    /// Returns the newly-appended hashes so the observer can push just those to
    /// the peer without re-scanning the whole table.
    pub fn hashes_after_id(&self, after_id: i64) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash FROM entries WHERE id > ?1 ORDER BY id")?;
        let hashes = stmt
            .query_map(params![after_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hashes)
    }

    /// The most recent `created_at` across all entries, or 0 if empty.
    ///
    /// clip-sync uses this to decide whether an entry merged from the peer is
    /// the newest copy known to either machine and should take the live
    /// clipboard.
    pub fn max_created_at(&self) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COALESCE(MAX(created_at), 0) FROM entries",
            [],
            |row| row.get(0),
        )
    }

    /// Update the last_used timestamp for an entry (called on paste).
    pub fn update_last_used(&self, id: i64) -> rusqlite::Result<()> {
        let now = Self::now();
        self.conn.execute(
            "UPDATE entries SET last_used = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Toggle the pinned flag on an entry.
    pub fn toggle_pin(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn.execute(
            "UPDATE entries SET pinned = 1 - pinned WHERE id = ?1",
            params![id],
        )?;
        let pinned: bool = self.conn.query_row(
            "SELECT pinned FROM entries WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0).map(|v| v != 0),
        )?;
        Ok(pinned)
    }

    /// Delete an entry by id.
    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Cleanup old/excess entries. Called after each store.
    /// Keeps pinned entries, removes:
    /// - entries beyond max_entries count
    /// - entries older than max_age seconds
    pub fn cleanup(&self) -> rusqlite::Result<usize> {
        let now = Self::now();
        let cutoff = now - MAX_AGE_SECS;

        let deleted = self.conn.execute(
            "DELETE FROM entries
             WHERE pinned = 0
               AND (id NOT IN (SELECT id FROM entries ORDER BY last_used DESC LIMIT ?1)
                    OR last_used < ?2)",
            params![MAX_ENTRIES, cutoff],
        )?;

        Ok(deleted)
    }

    /// Returns the total number of entries.
    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
    }

    /// Check if the database is empty (for migration detection).
    pub fn is_empty(&self) -> rusqlite::Result<bool> {
        self.count().map(|c| c == 0)
    }
}
