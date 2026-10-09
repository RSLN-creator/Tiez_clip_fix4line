use crate::database::{
    calc_image_hash, calc_text_hash, has_sensitive_tag, is_text_type, save_image_to_file,
    ENCRYPT_PREFIX,
};
use crate::domain::models::ClipboardEntry;
use crate::infrastructure::encryption;
use crate::infrastructure::repository::settings_repo::SqliteSettingsRepository;
use rusqlite::params;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use urlencoding::decode;

const RICH_IMAGE_FALLBACK_PREFIX: &str = "<!--TIEZ_RICH_IMAGE:";
const RICH_IMAGE_FALLBACK_SUFFIX: &str = "-->";

/// Canonical projection for clipboard rows. Column order must stay in sync with
/// [`SqliteClipboardRepository::row_to_entry`].
pub(crate) const ENTRY_COLUMNS: &str = "ch.id, ch.content_type, ch.content, ch.html_content, \
     ch.source_app, ch.timestamp, ch.preview, ch.is_pinned, ch.tags, ch.use_count, \
     ch.is_external, ch.pinned_order, ch.source_app_path";

/// Tags whose entries are encrypted at rest and therefore cannot be matched by a
/// plaintext index; they are searched separately after DPAPI decryption.
pub(crate) fn sensitive_tags_sql() -> String {
    let tags = crate::database::SENSITIVE_TAGS;
    let parts: Vec<String> = tags
        .iter()
        .map(|t| format!("'{}'", t.replace('\'', "''")))
        .collect();
    format!("({})", parts.join(","))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn is_syncable_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text" | "code" | "url" | "rich_text" | "image"
    )
}

pub trait ClipboardRepository {
    fn save(
        &self,
        entry: &ClipboardEntry,
        data_dir: Option<&std::path::Path>,
    ) -> Result<i64, String>;
    fn get_history(
        &self,
        limit: i32,
        offset: i32,
        content_type: Option<&str>,
    ) -> Result<Vec<ClipboardEntry>, String>;
    fn search(&self, query: &str, limit: i32) -> Result<Vec<ClipboardEntry>, String>;
    fn delete(&self, id: i64, data_dir: Option<&std::path::Path>) -> Result<(), String>;
    fn clear(&self, data_dir: Option<&std::path::Path>) -> Result<(), String>;
    fn get_count(&self) -> Result<i64, String>;
    fn increment_use_count(&self, id: i64) -> Result<(), String>;
    fn touch_entry(&self, id: i64, timestamp: i64) -> Result<(), String>;
    fn toggle_pin(&self, id: i64, is_pinned: bool) -> Result<(), String>;
    fn update_pinned_order(&self, orders: Vec<(i64, i64)>) -> Result<(), String>;
    fn get_entry_by_id(&self, id: i64) -> Result<Option<ClipboardEntry>, String>;
    fn get_entry_by_content(
        &self,
        content: &str,
        content_type: Option<&str>,
    ) -> Result<Option<i64>, String>;
    fn update_entry_content(&self, id: i64, content: &str, preview: &str) -> Result<(), String>;
    fn get_entry_content(&self, id: i64) -> Result<Option<String>, String>;
    fn get_entry_content_full(&self, id: i64) -> Result<Option<(String, String)>, String>;
    fn get_entry_content_with_html(
        &self,
        id: i64,
    ) -> Result<Option<(String, String, Option<String>)>, String>;
}

pub struct SqliteClipboardRepository {
    conn: Arc<Mutex<Connection>>,
    /// Decrypted text of encrypted-at-rest (sensitive) entries, keyed by entry id.
    ///
    /// Entries are DPAPI-protected, so matching their plaintext requires one
    /// `CryptUnprotectData` call per row (measured ~0.4 ms). With a few hundred
    /// sensitive rows that cost dominated every search (~1.2 s) even after the query
    /// itself became cheap. Caching the decrypted, lower-cased text removes it.
    ///
    /// Invalidation is implicit: the cache stores the row's `content_hash`, which the
    /// repository recomputes from the *plaintext* whenever the row is re-encrypted, so
    /// a changed row simply misses the cache. Nothing else has to be hooked.
    sensitive_text_cache: Mutex<HashMap<i64, SensitiveCacheEntry>>,
}

/// See [`SqliteClipboardRepository::sensitive_text_cache`].
struct SensitiveCacheEntry {
    content_hash: i64,
    content_lower: String,
    source_app_lower: String,
    tags_lower: Vec<String>,
}

impl SqliteClipboardRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            sensitive_text_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Returns lower-cased matchable text for a sensitive row, decrypting only on a
    /// cache miss. The `content_hash` is taken from the encrypted row but was computed
    /// from the plaintext by `encrypt_entry_with_conn`, so it is a reliable cache key.
    fn sensitive_texts(
        &self,
        id: i64,
        content_hash: i64,
        content_raw: &str,
        source_app: &str,
        tags_json: &str,
    ) -> (String, String, Vec<String>) {
        let mut cache = self
            .sensitive_text_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(hit) = cache.get(&id) {
            if hit.content_hash == content_hash {
                return (
                    hit.content_lower.clone(),
                    hit.source_app_lower.clone(),
                    hit.tags_lower.clone(),
                );
            }
        }

        let plain = self.maybe_decrypt_text(content_raw);
        let entry = SensitiveCacheEntry {
            content_hash,
            content_lower: plain.to_lowercase(),
            source_app_lower: source_app.to_lowercase(),
            tags_lower: serde_json::from_str::<Vec<String>>(tags_json)
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.to_lowercase())
                .collect(),
        };

        let result = (
            entry.content_lower.clone(),
            entry.source_app_lower.clone(),
            entry.tags_lower.clone(),
        );
        cache.insert(id, entry);
        result
    }

    pub fn encrypt_entry_with_conn(&self, conn: &Connection, id: i64) -> Result<(), String> {
        let (content_raw, preview_raw, html_raw, content_type, content_hash): (String, String, Option<String>, String, i64) =
            conn.query_row(
                "SELECT content, preview, html_content, content_type, content_hash FROM clipboard_history WHERE id = ?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2).ok(), row.get(3)?, row.get(4)?)),
            ).map_err(|e| e.to_string())?;

        let already_encrypted = content_raw.starts_with(ENCRYPT_PREFIX)
            && preview_raw.starts_with(ENCRYPT_PREFIX)
            && html_raw
                .as_ref()
                .map(|h| h.starts_with(ENCRYPT_PREFIX))
                .unwrap_or(true);
        if already_encrypted {
            return Ok(());
        }

        let content_plain = self.maybe_decrypt_text(&content_raw);
        let preview_plain = self.maybe_decrypt_text(&preview_raw);
        let html_plain = html_raw.map(|h| self.maybe_decrypt_text(&h));

        let content_enc = self.maybe_encrypt_text(&content_plain);
        let preview_enc = self.maybe_encrypt_text(&preview_plain);
        let html_enc = html_plain.as_ref().map(|h| self.maybe_encrypt_text(h));
        let new_hash = if is_text_type(&content_type) {
            calc_text_hash(&content_plain) as i64
        } else {
            content_hash
        };

        conn.execute(
            "UPDATE clipboard_history SET content = ?, preview = ?, html_content = ?, content_hash = ? WHERE id = ?",
            params![content_enc, preview_enc, html_enc, new_hash, id],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn decrypt_entry_with_conn(&self, conn: &Connection, id: i64) -> Result<(), String> {
        let (content_raw, preview_raw, html_raw, content_type, content_hash): (String, String, Option<String>, String, i64) =
            conn.query_row(
                "SELECT content, preview, html_content, content_type, content_hash FROM clipboard_history WHERE id = ?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2).ok(), row.get(3)?, row.get(4)?)),
            ).map_err(|e| e.to_string())?;

        let any_encrypted = content_raw.starts_with(ENCRYPT_PREFIX)
            || preview_raw.starts_with(ENCRYPT_PREFIX)
            || html_raw
                .as_ref()
                .map(|h| h.starts_with(ENCRYPT_PREFIX))
                .unwrap_or(false);
        if !any_encrypted {
            return Ok(());
        }

        let content_plain = self.maybe_decrypt_text(&content_raw);
        let preview_plain = self.maybe_decrypt_text(&preview_raw);
        let html_plain = html_raw.map(|h| self.maybe_decrypt_text(&h));
        let new_hash = if is_text_type(&content_type) {
            calc_text_hash(&content_plain) as i64
        } else {
            content_hash
        };

        conn.execute(
            "UPDATE clipboard_history SET content = ?, preview = ?, html_content = ?, content_hash = ? WHERE id = ?",
            params![content_plain, preview_plain, html_plain, new_hash, id],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn sync_entry_tags_with_conn(
        &self,
        conn: &Connection,
        entry_id: i64,
        tags: &[String],
    ) -> Result<(), String> {
        conn.execute(
            "DELETE FROM entry_tags WHERE entry_id = ?",
            params![entry_id],
        )
        .map_err(|e| e.to_string())?;
        for tag in tags {
            let clean = tag.trim();
            if clean.is_empty() {
                continue;
            }
            conn.execute(
                "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
                params![entry_id, clean],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn upsert_tombstone_with_conn(
        &self,
        conn: &Connection,
        content_type: &str,
        content_hash: i64,
        deleted_at: i64,
    ) -> Result<(), String> {
        if !is_syncable_content_type(content_type) || content_hash == 0 {
            return Ok(());
        }

        conn.execute(
            "INSERT INTO cloud_sync_tombstones (content_type, content_hash, deleted_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(content_type, content_hash)
             DO UPDATE SET deleted_at = MAX(cloud_sync_tombstones.deleted_at, excluded.deleted_at)",
            params![content_type, content_hash, deleted_at],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn clear_tombstone_with_conn(
        &self,
        conn: &Connection,
        content_type: &str,
        content_hash: i64,
    ) -> Result<(), String> {
        if !is_syncable_content_type(content_type) || content_hash == 0 {
            return Ok(());
        }

        conn.execute(
            "DELETE FROM cloud_sync_tombstones WHERE content_type = ?1 AND content_hash = ?2",
            params![content_type, content_hash],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn maybe_encrypt_text(&self, value: &str) -> String {
        #[cfg(not(feature = "portable"))]
        {
            if value.starts_with(ENCRYPT_PREFIX) {
                return value.to_string();
            }
            encryption::encrypt_value(value).unwrap_or_else(|| value.to_string())
        }
        #[cfg(feature = "portable")]
        {
            value.to_string()
        }
    }

    fn maybe_decrypt_text(&self, value: &str) -> String {
        if value.starts_with(ENCRYPT_PREFIX) {
            encryption::decrypt_value(value).unwrap_or_else(|| value.to_string())
        } else {
            value.to_string()
        }
    }

    /// Maps a row selected with [`ENTRY_COLUMNS`] into a [`ClipboardEntry`],
    /// transparently decrypting any DPAPI-protected fields.
    fn row_to_entry(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipboardEntry> {
        let tags_str: String = row.get(8).unwrap_or_else(|_| "[]".to_string());
        let content_raw: String = row.get(2)?;
        let preview_raw: String = row.get(6)?;
        let html_raw: Option<String> = row.get(3).ok();

        Ok(ClipboardEntry {
            id: row.get(0)?,
            content_type: row.get(1)?,
            content: self.maybe_decrypt_text(&content_raw),
            html_content: html_raw.map(|v| self.maybe_decrypt_text(&v)),
            source_app: row.get(4)?,
            timestamp: row.get(5)?,
            preview: self.maybe_decrypt_text(&preview_raw),
            is_pinned: row.get::<_, i32>(7)? == 1,
            tags: serde_json::from_str(&tags_str).unwrap_or_default(),
            use_count: row.get(9).unwrap_or(0),
            is_external: row.get::<_, i32>(10)? == 1,
            pinned_order: row.get(11).unwrap_or(0),
            source_app_path: row.get(12).unwrap_or(None),
            file_preview_exists: true,
        })
    }

    /// Indexed search path: FTS5 narrows to candidate rowids, the small candidate
    /// set is re-verified with LIKE, and only the top `limit` rows are materialised.
    ///
    /// The two-phase shape matters. Selecting the wide columns before the LIMIT makes
    /// SQLite spill `content` + `html_content` into a TEMP B-TREE sort, which measured
    /// 5-7 s on the real 473 MB database - slower than the scan it replaced.
    fn search_indexed(
        &self,
        conn: &Connection,
        term: &str,
        limit: i32,
    ) -> Result<Vec<ClipboardEntry>, String> {
        let phrase = crate::database::fts_phrase(term);
        let sensitive = sensitive_tags_sql();

        // Phase 1: rank candidate ids with the inverted index alone. Only (id, timestamp)
        // enter the sorter.
        //
        // Verification is deliberately NOT done here. Running `LIKE` over every candidate
        // meant reading the `content` column of each one - 1936 rows for a term like
        // "http" - which cost more than the scan it replaced. Phase 2 already loads the
        // plaintext, so the check is free there and only runs for the rows we return.
        //
        // The UNION covers matches that only exist in a tag; `clipboard_fts` indexes
        // content + source_app, entry_tags is separate.
        let sql_ids = format!(
            "SELECT id FROM (
                 SELECT ch.id AS id, ch.timestamp AS ts
                   FROM clipboard_fts f
                   JOIN clipboard_history ch ON ch.id = f.rowid
                  WHERE clipboard_fts MATCH ?1
                    AND NOT EXISTS (
                        SELECT 1 FROM entry_tags se
                         WHERE se.entry_id = ch.id AND se.tag COLLATE NOCASE IN {sensitive}
                    )
                 UNION
                 SELECT ch.id AS id, ch.timestamp AS ts
                   FROM clipboard_history ch
                  WHERE NOT EXISTS (
                        SELECT 1 FROM entry_tags se
                         WHERE se.entry_id = ch.id AND se.tag COLLATE NOCASE IN {sensitive}
                    )
                    AND EXISTS (
                        SELECT 1 FROM entry_tags te
                         WHERE te.entry_id = ch.id AND te.tag LIKE '%' || ?2 || '%'
                    )
             )
             ORDER BY ts DESC, id DESC
             LIMIT ?3",
            sensitive = sensitive
        );

        let mut stmt = conn.prepare(&sql_ids).map_err(|e| e.to_string())?;
        let ids: Vec<i64> = stmt
            .query_map(params![phrase, term, limit], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Phase 2: materialise the full rows for the (bounded) candidate ids, and verify
        // the match against the real plaintext. This keeps the index an accelerator: any
        // stale entry (a row re-written while the one-time backfill was running) is
        // dropped here rather than surfaced to the user.
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let sql_rows = format!(
            "SELECT {cols} FROM clipboard_history ch WHERE ch.id IN ({placeholders})",
            cols = ENTRY_COLUMNS,
            placeholders = placeholders
        );

        let mut stmt = conn.prepare(&sql_rows).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| self.row_to_entry(row))
            .map_err(|e| e.to_string())?;

        let mut results = Vec::new();
        for row in rows {
            let entry = row.map_err(|e| e.to_string())?;
            let hit = entry.content.to_lowercase().contains(term)
                || entry.source_app.to_lowercase().contains(term)
                || entry
                    .tags
                    .iter()
                    .any(|t| t.to_lowercase().contains(term));
            if hit {
                results.push(entry);
            }
        }
        Ok(results)
    }

    /// Legacy linear path. Retained as the fallback for terms shorter than the
    /// trigram minimum and while the index backfill is still running.
    fn search_linear(
        &self,
        conn: &Connection,
        term: &str,
        limit: i32,
    ) -> Result<Vec<ClipboardEntry>, String> {
        let sensitive = sensitive_tags_sql();
        let sql = format!(
            "SELECT {cols}
               FROM clipboard_history ch
               LEFT JOIN entry_tags et ON ch.id = et.entry_id
              WHERE NOT EXISTS (
                  SELECT 1 FROM entry_tags se
                   WHERE se.entry_id = ch.id AND se.tag COLLATE NOCASE IN {sensitive}
              )
                AND (
                  ch.content LIKE '%' || ?1 || '%'
                  OR ch.source_app LIKE '%' || ?1 || '%'
                  OR et.tag LIKE '%' || ?1 || '%'
                )
              ORDER BY ch.timestamp DESC, ch.id DESC
              LIMIT ?2",
            cols = ENTRY_COLUMNS,
            sensitive = sensitive
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![term, limit], |row| self.row_to_entry(row))
            .map_err(|e| e.to_string())?;

        let mut results: Vec<ClipboardEntry> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for row in rows {
            let entry = row.map_err(|e| e.to_string())?;
            if seen.insert(entry.id) {
                results.push(entry);
            }
        }
        Ok(results)
    }

    /// Searches entries that are encrypted at rest (DPAPI).
    ///
    /// Two deliberate properties:
    ///
    /// 1. The predicate is just the indexed `EXISTS (sensitive tag)`. The previous
    ///    version OR'd in `content/preview/html_content LIKE 'dpapi:%'`, which forced
    ///    SQLite to read `html_content` for all 25,300 rows (351 MB of the 473 MB
    ///    database) and measured 1229 ms versus 9 ms - while selecting exactly the same
    ///    rows (verified: symmetric difference of the two sets is empty).
    ///
    /// 2. Only matching-relevant columns are fetched, decryption goes through
    ///    [`Self::sensitive_texts`] (cached), and the full entry - preview, HTML - is
    ///    materialised *only* for rows that actually match. Decrypting every candidate
    ///    row in full cost ~1.2 s per search on the reference database.
    fn search_sensitive_paged(
        &self,
        conn: &Connection,
        term: &str,
        limit: i32,
        results: &mut Vec<ClipboardEntry>,
        seen: &mut HashSet<i64>,
    ) -> Result<(), String> {
        let sensitive = sensitive_tags_sql();
        let sql = format!(
            "SELECT ch.id, ch.content_hash, ch.content, ch.source_app, ch.tags, ch.timestamp
               FROM clipboard_history ch
              WHERE EXISTS (
                  SELECT 1 FROM entry_tags se
                   WHERE se.entry_id = ch.id AND se.tag COLLATE NOCASE IN {sensitive}
              )
                AND ((ch.timestamp < ?1) OR (ch.timestamp = ?1 AND ch.id < ?2))
              ORDER BY ch.timestamp DESC, ch.id DESC
              LIMIT ?3",
            sensitive = sensitive
        );

        let mut cursor_ts = i64::MAX;
        let mut cursor_id = i64::MAX;
        const BATCH_SIZE: i32 = 500;

        loop {
            let mut matched: Vec<i64> = Vec::new();
            let mut next_cursor: Option<(i64, i64)> = None;

            {
                let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![cursor_ts, cursor_id, BATCH_SIZE], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1).unwrap_or(0),
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4).unwrap_or_else(|_| "[]".to_string()),
                            row.get::<_, i64>(5)?,
                        ))
                    })
                    .map_err(|e| e.to_string())?;

                let mut batch: Vec<(i64, i64, String, String, String, i64)> = Vec::new();
                for row in rows {
                    batch.push(row.map_err(|e| e.to_string())?);
                }

                if batch.is_empty() {
                    return Ok(());
                }
                if let Some(last) = batch.last() {
                    next_cursor = Some((last.5, last.0));
                }

                for (id, hash, content_raw, source_app, tags_json, _ts) in batch.iter() {
                    if seen.contains(id) {
                        continue;
                    }
                    let (content_lower, app_lower, tags_lower) = self.sensitive_texts(
                        *id,
                        *hash,
                        content_raw,
                        source_app,
                        tags_json,
                    );
                    let hit = content_lower.contains(term)
                        || app_lower.contains(term)
                        || tags_lower.iter().any(|t| t.contains(term));
                    if hit {
                        matched.push(*id);
                    }
                }
            }

            for id in matched {
                if !seen.insert(id) {
                    continue;
                }
                if let Some(entry) = self.get_entry_by_id_with_conn(conn, id)? {
                    results.push(entry);
                    if results.len() >= limit as usize {
                        return Ok(());
                    }
                }
            }

            match next_cursor {
                Some((ts, id)) => {
                    cursor_ts = ts;
                    cursor_id = id;
                }
                None => return Ok(()),
            }
        }
    }

    fn extract_rich_image_fallback_payload(html: &str) -> Option<String> {
        if let Some(start) = html.rfind(RICH_IMAGE_FALLBACK_PREFIX) {
            let marker_start = start + RICH_IMAGE_FALLBACK_PREFIX.len();
            if let Some(end_rel) = html[marker_start..].find(RICH_IMAGE_FALLBACK_SUFFIX) {
                let marker_end = marker_start + end_rel;
                let payload = html[marker_start..marker_end].trim();
                if !payload.is_empty() {
                    return Some(payload.to_string());
                }
            }
        }
        None
    }

    fn fallback_payload_to_path(payload: &str) -> Option<PathBuf> {
        let value = payload.trim();
        if value.is_empty() || value.starts_with("data:image/") {
            return None;
        }

        let path_raw = if value.starts_with("file://") {
            value.trim_start_matches("file://")
        } else {
            value
        };

        let path_without_drive_prefix =
            if path_raw.starts_with('/') && path_raw.chars().nth(2) == Some(':') {
                &path_raw[1..]
            } else {
                path_raw
            };

        let decoded_path = decode(path_without_drive_prefix)
            .map(|p| p.into_owned())
            .unwrap_or_else(|_| path_without_drive_prefix.to_string());

        if decoded_path.is_empty() {
            None
        } else {
            Some(PathBuf::from(decoded_path))
        }
    }

    fn collect_attachment_paths_for_cleanup(
        &self,
        content_raw: &str,
        html_raw: Option<&str>,
        is_external: bool,
        attachments_dir: &std::path::Path,
    ) -> Vec<PathBuf> {
        let mut paths = HashSet::new();

        if is_external {
            let content_path = PathBuf::from(self.maybe_decrypt_text(content_raw));
            if content_path.starts_with(attachments_dir) {
                paths.insert(content_path);
            }
        }

        if let Some(html_raw_value) = html_raw {
            let html = self.maybe_decrypt_text(html_raw_value);
            if let Some(payload) = Self::extract_rich_image_fallback_payload(&html) {
                if let Some(path) = Self::fallback_payload_to_path(&payload) {
                    if path.starts_with(attachments_dir) {
                        paths.insert(path);
                    }
                }
            }
        }

        paths.into_iter().collect()
    }

    pub fn save_with_conn(
        &self,
        conn: &Connection,
        entry: &ClipboardEntry,
        data_dir: Option<&std::path::Path>,
    ) -> Result<i64, String> {
        // Encrypt only when explicitly marked as sensitive
        let should_encrypt = has_sensitive_tag(&entry.tags);

        let mut final_content = entry.content.clone();
        let mut final_is_external = entry.is_external;

        // Externalize image if possible
        if entry.content_type == "image" && entry.content.starts_with("data:image/") {
            if let Some(dir) = data_dir {
                if let Some(path) = save_image_to_file(&entry.content, dir) {
                    final_content = path;
                    final_is_external = true;
                }
            }
        }

        let calculated_hash = if entry.content_type == "image" {
            calc_image_hash(&final_content).unwrap_or(0)
        } else {
            calc_text_hash(&final_content) as i64
        };

        // Re-adding an item should clear an older delete tombstone for the same fingerprint.
        let _ = self.clear_tombstone_with_conn(conn, &entry.content_type, calculated_hash);

        let (content, preview, content_hash, html_content) = if should_encrypt {
            let encrypted_content = self.maybe_encrypt_text(&final_content);
            let encrypted_preview = self.maybe_encrypt_text(&entry.preview);
            let encrypted_html = entry
                .html_content
                .as_ref()
                .map(|html| self.maybe_encrypt_text(html));
            (
                encrypted_content,
                encrypted_preview,
                calculated_hash,
                encrypted_html,
            )
        } else {
            (
                final_content,
                entry.preview.clone(),
                calculated_hash,
                entry.html_content.clone(),
            )
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut cleaned_tags: Vec<String> = Vec::new();
        for tag in &entry.tags {
            let t = tag.trim();
            if t.is_empty() {
                continue;
            }
            let t_owned = t.to_string();
            if seen.insert(t_owned.clone()) {
                cleaned_tags.push(t_owned);
            }
        }

        if entry.id > 0 {
            // Update existing entry (Move to top logic)
            conn.execute(
                "UPDATE clipboard_history SET 
                    content_type = ?1, 
                    content = ?2, 
                    html_content = ?3, 
                    source_app = ?4, 
                    timestamp = ?5, 
                    preview = ?6, 
                    content_hash = ?7, 
                    tags = ?8, 
                    is_external = ?9,
                    source_app_path = ?10,
                    use_count = use_count + 1
                 WHERE id = ?11",
                params![
                    entry.content_type,
                    content,
                    html_content,
                    entry.source_app,
                    entry.timestamp,
                    preview,
                    content_hash,
                    serde_json::to_string(&cleaned_tags).unwrap_or_else(|_| "[]".to_string()),
                    if final_is_external { 1 } else { 0 },
                    entry.source_app_path.as_deref(),
                    entry.id
                ],
            )
            .map_err(|e| e.to_string())?;
            self.sync_entry_tags_with_conn(conn, entry.id, &cleaned_tags)?;
            Ok(entry.id)
        } else {
            // Insert new entry
            conn.execute(
                "INSERT INTO clipboard_history (content_type, content, html_content, source_app, timestamp, preview, is_pinned, content_hash, tags, is_external, pinned_order, source_app_path) 
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    entry.content_type,
                    content,
                    html_content,
                    entry.source_app,
                    entry.timestamp,
                    preview,
                    if entry.is_pinned { 1 } else { 0 },
                    content_hash,
                    serde_json::to_string(&cleaned_tags).unwrap_or_else(|_| "[]".to_string()),
                    if final_is_external { 1 } else { 0 },
                    entry.pinned_order,
                    entry.source_app_path.as_deref()
                ],
            ).map_err(|e| e.to_string())?;

            let new_id = conn.last_insert_rowid();
            self.sync_entry_tags_with_conn(conn, new_id, &cleaned_tags)?;
            Ok(new_id)
        }
    }

    pub fn delete_with_conn(
        &self,
        conn: &Connection,
        id: i64,
        data_dir: Option<&std::path::Path>,
    ) -> Result<(), String> {
        let mut tombstone: Option<(String, i64)> = None;
        // Check for external files to delete
        if let Some(dir) = data_dir {
            let attachments_dir = dir.join("attachments");
            let mut stmt = conn
                 .prepare("SELECT content, html_content, is_external, content_type, content_hash FROM clipboard_history WHERE id = ?")
                 .map_err(|e| e.to_string())?;

            if let Ok(entry) = stmt.query_row([id], |row| {
                let content_raw: String = row.get(0)?;
                let html_raw: Option<String> = row.get(1).ok();
                let is_ext: i32 = row.get(2)?;
                let content_type: String = row.get(3)?;
                let content_hash: i64 = row.get(4)?;
                Ok((
                    content_raw,
                    html_raw,
                    is_ext == 1,
                    content_type,
                    content_hash,
                ))
            }) {
                let files_to_remove = self.collect_attachment_paths_for_cleanup(
                    &entry.0,
                    entry.1.as_deref(),
                    entry.2,
                    &attachments_dir,
                );
                for path in files_to_remove {
                    if path.exists() {
                        let _ = std::fs::remove_file(path);
                    }
                }
                tombstone = Some((entry.3, entry.4));
            }
        } else {
            let mut stmt = conn
                .prepare("SELECT content_type, content_hash FROM clipboard_history WHERE id = ?")
                .map_err(|e| e.to_string())?;
            if let Ok(entry) = stmt.query_row([id], |row| {
                let content_type: String = row.get(0)?;
                let content_hash: i64 = row.get(1)?;
                Ok((content_type, content_hash))
            }) {
                tombstone = Some(entry);
            }
        }

        if let Some((content_type, content_hash)) = tombstone {
            let _ = self.upsert_tombstone_with_conn(conn, &content_type, content_hash, now_ms());
        }

        conn.execute("DELETE FROM clipboard_history WHERE id = ?", [id])
            .map_err(|e| e.to_string())?;
        let _ = conn.execute("DELETE FROM entry_tags WHERE entry_id = ?", params![id]);
        Ok(())
    }

    pub fn delete_metadata_with_conn(&self, conn: &Connection, id: i64) -> Result<(), String> {
        conn.execute("DELETE FROM clipboard_history WHERE id = ?", params![id])
            .map_err(|e| e.to_string())?;
        let _ = conn.execute("DELETE FROM entry_tags WHERE entry_id = ?", params![id]);
        Ok(())
    }

    pub fn find_by_content_with_conn(
        &self,
        conn: &Connection,
        content: &str,
        content_type: Option<&str>,
    ) -> Result<Option<i64>, String> {
        if content_type == Some("image") {
            if let Some(hash) = calc_image_hash(content) {
                let mut stmt = conn
                    .prepare(
                        "SELECT id FROM clipboard_history \
                     WHERE (content_type = 'image' AND content_hash = ?) OR content = ?",
                    )
                    .map_err(|e| e.to_string())?;
                let mut rows = stmt
                    .query(params![hash, content])
                    .map_err(|e| e.to_string())?;
                if let Some(row) = rows.next().map_err(|e| e.to_string())? {
                    return Ok(Some(row.get(0).map_err(|e| e.to_string())?));
                }

                let mut fallback_stmt = conn
                    .prepare(
                        "SELECT id, content FROM clipboard_history \
                     WHERE content_type = 'image' \
                     ORDER BY timestamp DESC \
                     LIMIT 200",
                    )
                    .map_err(|e| e.to_string())?;
                let mut fallback_rows = fallback_stmt.query([]).map_err(|e| e.to_string())?;
                while let Some(row) = fallback_rows.next().map_err(|e| e.to_string())? {
                    let id: i64 = row.get(0).map_err(|e| e.to_string())?;
                    let stored_content_raw: String = row.get(1).map_err(|e| e.to_string())?;
                    let stored_content = self.maybe_decrypt_text(&stored_content_raw);
                    if calc_image_hash(&stored_content) == Some(hash) {
                        return Ok(Some(id));
                    }
                }
                return Ok(None);
            }
        }

        let hash = calc_text_hash(content) as i64;

        if let Some(ct) = content_type {
            let mut stmt = conn.prepare(
                "SELECT id FROM clipboard_history \
                 WHERE (content_type = ? AND content_hash = ?) OR (content_type = ? AND content = ?)",
            ).map_err(|e| e.to_string())?;
            let mut rows = stmt
                .query(params![ct, hash, ct, content])
                .map_err(|e| e.to_string())?;
            if let Some(row) = rows.next().map_err(|e| e.to_string())? {
                Ok(Some(row.get(0).map_err(|e| e.to_string())?))
            } else {
                Ok(None)
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id FROM clipboard_history \
                 WHERE ((content_type IN ('text', 'rich_text', 'code', 'url')) AND content_hash = ?) OR content = ?",
            ).map_err(|e| e.to_string())?;
            let mut rows = stmt
                .query(params![hash, content])
                .map_err(|e| e.to_string())?;
            if let Some(row) = rows.next().map_err(|e| e.to_string())? {
                Ok(Some(row.get(0).map_err(|e| e.to_string())?))
            } else {
                Ok(None)
            }
        }
    }

    pub fn enforce_limit_with_conn(
        &self,
        conn: &Connection,
        data_dir: Option<&std::path::Path>,
    ) -> Result<Vec<i64>, String> {
        // Check if storage limit is enabled
        if let Ok(Some(limit_enabled_str)) =
            SqliteSettingsRepository::get_raw(conn, "app.persistent_limit_enabled")
        {
            if limit_enabled_str == "false" {
                return Ok(Vec::new());
            }
        }

        // Get the storage limit
        if let Ok(Some(limit_str)) = SqliteSettingsRepository::get_raw(conn, "app.persistent_limit")
        {
            if let Ok(limit) = limit_str.parse::<i32>() {
                // Count non-pinned entries that have no tags
                let count: i32 = conn.query_row(
                    "SELECT COUNT(*) FROM clipboard_history WHERE is_pinned = 0 AND (tags = '[]' OR tags IS NULL)",
                    [],
                    |row| row.get(0)
                ).map_err(|e| e.to_string())?;

                if count > limit {
                    // First, get the IDs that will be deleted
                    let to_delete = count - limit;
                    let deleted_ids: Vec<i64> = {
                        let mut stmt = conn
                            .prepare(
                                "SELECT id FROM clipboard_history 
                             WHERE is_pinned = 0 AND (tags = '[]' OR tags IS NULL)
                             ORDER BY timestamp ASC 
                             LIMIT ?",
                            )
                            .map_err(|e| e.to_string())?;

                        let rows = stmt
                            .query_map([to_delete], |row| row.get(0))
                            .map_err(|e| e.to_string())?;
                        rows.filter_map(|r| r.ok()).collect()
                    };
                    // Actually delete records (and files if needed)
                    for id in &deleted_ids {
                        let _ = self.delete_with_conn(conn, *id, data_dir);
                    }
                    return Ok(deleted_ids);
                }
            }
        }

        Ok(Vec::new())
    }
    pub fn toggle_pin_with_conn(
        &self,
        conn: &Connection,
        id: i64,
        is_pinned: bool,
    ) -> Result<(), String> {
        if is_pinned {
            // Set pinned_order to max + 1 so it appears at top
            conn.execute(
                "UPDATE clipboard_history 
                 SET is_pinned = 1, 
                     pinned_order = (SELECT COALESCE(MAX(pinned_order), 0) + 1 FROM clipboard_history WHERE is_pinned = 1) 
                 WHERE id = ?",
                params![id],
            ).map_err(|e| e.to_string())?;
        } else {
            conn.execute(
                "UPDATE clipboard_history SET is_pinned = 0, pinned_order = 0 WHERE id = ?",
                params![id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn update_pinned_order_with_conn(
        &self,
        conn: &Connection,
        orders: Vec<(i64, i64)>,
    ) -> Result<(), String> {
        for (id, order) in orders {
            conn.execute(
                "UPDATE clipboard_history SET pinned_order = ? WHERE id = ?",
                params![order, id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn get_entry_by_id_with_conn(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<Option<ClipboardEntry>, String> {
        let mut stmt = conn.prepare(
            "SELECT id, content_type, content, html_content, source_app, timestamp, preview, is_pinned, tags, use_count, is_external, pinned_order, source_app_path 
             FROM clipboard_history 
             WHERE id = ? 
             LIMIT 1",
        ).map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let tags_str: String = row.get(8).unwrap_or_else(|_| "[]".to_string());
            let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();

            let content_raw: String = row.get(2).map_err(|e| e.to_string())?;
            let html_raw: Option<String> = row.get(3).map_err(|e| e.to_string()).unwrap_or(None);
            let preview_raw: String = row.get(6).map_err(|e| e.to_string())?;
            let content = self.maybe_decrypt_text(&content_raw);
            let preview = self.maybe_decrypt_text(&preview_raw);
            let html_content = html_raw.map(|v| self.maybe_decrypt_text(&v));

            Ok(Some(ClipboardEntry {
                id: row.get(0).map_err(|e| e.to_string())?,
                content_type: row.get(1).map_err(|e| e.to_string())?,
                content,
                html_content,
                source_app: row.get(4).map_err(|e| e.to_string())?,
                timestamp: row.get(5).map_err(|e| e.to_string())?,
                preview,
                is_pinned: row.get::<_, i32>(7).map_err(|e| e.to_string())? == 1,
                tags,
                use_count: row.get(9).unwrap_or(0),
                is_external: row.get::<_, i32>(10).unwrap_or(0) == 1,
                pinned_order: row.get(11).unwrap_or(0),
                source_app_path: row.get(12).unwrap_or(None),
                file_preview_exists: true,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_entry_content_with_conn(
        &self,
        conn: &Connection,
        id: i64,
        content: &str,
        preview: &str,
    ) -> Result<(), String> {
        let (old_content_raw, content_type, tags_json) = conn
            .query_row(
                "SELECT content, content_type, tags FROM clipboard_history WHERE id = ?",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;

        let old_content = self.maybe_decrypt_text(&old_content_raw);
        if old_content == content && content_type != "rich_text" {
            return Ok(());
        }

        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let should_encrypt = has_sensitive_tag(&tags);

        if is_text_type(&content_type) {
            let hash = calc_text_hash(content) as i64;
            let new_type = if content_type == "rich_text" {
                "text"
            } else {
                &content_type
            };
            if should_encrypt {
                let encrypted_content = self.maybe_encrypt_text(content);
                let encrypted_preview = self.maybe_encrypt_text(preview);
                conn.execute(
                    "UPDATE clipboard_history SET content = ?, preview = ?, content_hash = ?, html_content = NULL, content_type = ? WHERE id = ?",
                    params![encrypted_content, encrypted_preview, hash, new_type, id],
                ).map_err(|e| e.to_string())?;
            } else {
                conn.execute(
                    "UPDATE clipboard_history SET content = ?, preview = ?, content_hash = ?, html_content = NULL, content_type = ? WHERE id = ?",
                    params![content, preview, hash, new_type, id],
                ).map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        if should_encrypt {
            let encrypted_content = self.maybe_encrypt_text(content);
            let encrypted_preview = self.maybe_encrypt_text(preview);
            conn.execute(
                "UPDATE clipboard_history SET content = ?, preview = ?, html_content = NULL WHERE id = ?",
                params![encrypted_content, encrypted_preview, id],
            ).map_err(|e| e.to_string())?;
        } else {
            conn.execute(
                "UPDATE clipboard_history SET content = ?, preview = ?, html_content = NULL WHERE id = ?",
                params![content, preview, id],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn get_entry_content_full_with_conn(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<Option<(String, String)>, String> {
        let mut stmt = conn
            .prepare("SELECT content, content_type FROM clipboard_history WHERE id = ?")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let content: String = row.get(0).map_err(|e| e.to_string())?;
            let content_type: String = row.get(1).map_err(|e| e.to_string())?;
            Ok(Some((self.maybe_decrypt_text(&content), content_type)))
        } else {
            Ok(None)
        }
    }

    pub fn get_entry_content_with_html_with_conn(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<Option<(String, String, Option<String>)>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT content, content_type, html_content FROM clipboard_history WHERE id = ?",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let content: String = row.get(0).map_err(|e| e.to_string())?;
            let content_type: String = row.get(1).map_err(|e| e.to_string())?;
            let html_raw: Option<String> = row.get(2).map_err(|e| e.to_string()).unwrap_or(None);
            let html_content = html_raw.map(|v| self.maybe_decrypt_text(&v));
            Ok(Some((
                self.maybe_decrypt_text(&content),
                content_type,
                html_content,
            )))
        } else {
            Ok(None)
        }
    }
}

impl ClipboardRepository for SqliteClipboardRepository {
    fn save(
        &self,
        entry: &ClipboardEntry,
        data_dir: Option<&std::path::Path>,
    ) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.save_with_conn(&conn, entry, data_dir)
    }

    fn get_history(
        &self,
        limit: i32,
        offset: i32,
        content_type: Option<&str>,
    ) -> Result<Vec<ClipboardEntry>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let map_row = |row: &rusqlite::Row| {
            let tags_str: String = row.get(8).unwrap_or_else(|_| "[]".to_string());
            let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();
            let content_type: String = row.get(1)?;
            let content_raw: String = row.get(2)?;
            let html_raw: Option<String> = row.get(3).ok();
            let preview_raw: String = row.get(6)?;
            let content = self.maybe_decrypt_text(&content_raw);
            let preview = self.maybe_decrypt_text(&preview_raw);
            let html_content = html_raw.as_ref().map(|v| self.maybe_decrypt_text(v));

            Ok((
                ClipboardEntry {
                    id: row.get(0)?,
                    content_type,
                    content,
                    html_content,
                    source_app: row.get(4)?,
                    timestamp: row.get(5)?,
                    preview,
                    is_pinned: row.get::<_, i32>(7)? == 1,
                    tags,
                    use_count: row.get(9).unwrap_or(0),
                    is_external: row.get::<_, i32>(10)? == 1,
                    pinned_order: row.get(11).unwrap_or(0),
                    source_app_path: row.get(12).unwrap_or(None),
                    file_preview_exists: {
                        let is_ext = row.get::<_, i32>(10)? == 1;
                        if is_ext {
                            let c: String = self.maybe_decrypt_text(&row.get::<_, String>(2)?);
                            std::path::Path::new(&c).exists()
                        } else {
                            true
                        }
                    },
                },
                content_raw,
                preview_raw,
                html_raw,
            ))
        };

        let mut mapped_rows = Vec::new();
        if let Some(ct) = content_type {
            let mut stmt = conn.prepare(
                "SELECT id, content_type, content, html_content, source_app, timestamp, preview, is_pinned, tags, use_count, is_external, pinned_order, source_app_path 
                 FROM clipboard_history 
                 WHERE content_type = ? 
                 ORDER BY is_pinned DESC, pinned_order DESC, timestamp DESC, id DESC 
                 LIMIT ? OFFSET ?",
            ).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![ct, limit, offset], map_row)
                .map_err(|e| e.to_string())?;
            for row in rows {
                mapped_rows.push(row.map_err(|e| e.to_string())?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, content_type, content, html_content, source_app, timestamp, preview, is_pinned, tags, use_count, is_external, pinned_order, source_app_path 
                 FROM clipboard_history 
                 ORDER BY is_pinned DESC, pinned_order DESC, timestamp DESC, id DESC 
                 LIMIT ? OFFSET ?",
            ).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([limit, offset], map_row)
                .map_err(|e| e.to_string())?;
            for row in rows {
                mapped_rows.push(row.map_err(|e| e.to_string())?);
            }
        }

        let mut history = Vec::new();
        for (entry, content_raw, preview_raw, html_raw) in mapped_rows {
            #[cfg(not(feature = "portable"))]
            {
                let is_sensitive = has_sensitive_tag(&entry.tags);
                let content_encrypted = content_raw.starts_with(ENCRYPT_PREFIX);
                let preview_encrypted = preview_raw.starts_with(ENCRYPT_PREFIX);
                let html_encrypted = html_raw
                    .as_ref()
                    .map(|h| h.starts_with(ENCRYPT_PREFIX))
                    .unwrap_or(false);
                let html_needs_encrypt = html_raw
                    .as_ref()
                    .map(|h| !h.starts_with(ENCRYPT_PREFIX))
                    .unwrap_or(false);

                if is_sensitive && (!content_encrypted || !preview_encrypted || html_needs_encrypt)
                {
                    let _ = self.encrypt_entry_with_conn(&conn, entry.id);
                } else if !is_sensitive
                    && (content_encrypted || preview_encrypted || html_encrypted)
                {
                    let _ = self.decrypt_entry_with_conn(&conn, entry.id);
                }
            }

            history.push(entry);
        }
        Ok(history)
    }

    fn search(&self, query: &str, limit: i32) -> Result<Vec<ClipboardEntry>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        let term = query.trim().to_lowercase();
        if term.is_empty() {
            return Ok(Vec::new());
        }

        // The trigram index cannot answer terms shorter than 3 characters (documented
        // FTS5 behaviour) and is empty until the one-time backfill completes; both
        // cases fall back to the linear scan.
        let indexed = crate::database::is_fts_ready(&conn)
            && crate::database::fts_term_is_usable(&term);

        #[cfg(feature = "portable")]
        {
            // Portable build: nothing is encrypted at rest, so the indexed path
            // covers every entry and no DPAPI pass is required.
            let mut results = if indexed {
                self.search_indexed(&conn, &term, limit)?
            } else {
                self.search_linear(&conn, &term, limit)?
            };
            results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
            if results.len() > limit as usize {
                results.truncate(limit as usize);
            }
            return Ok(results);
        }

        #[cfg(not(feature = "portable"))]
        {
            let mut results = if indexed {
                self.search_indexed(&conn, &term, limit)?
            } else {
                self.search_linear(&conn, &term, limit)?
            };

            let mut seen: HashSet<i64> = results.iter().map(|e| e.id).collect();

            // Encrypted/sensitive entries are not in the plaintext index, so they
            // still need a decrypt pass - but only when the plaintext path did not
            // already fill the page.
            if results.len() < limit as usize {
                self.search_sensitive_paged(&conn, &term, limit, &mut results, &mut seen)?;
            }

            results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
            if results.len() > limit as usize {
                results.truncate(limit as usize);
            }
            Ok(results)
        }
    }

    fn delete(&self, id: i64, data_dir: Option<&std::path::Path>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.delete_with_conn(&conn, id, data_dir)
    }

    fn clear(&self, data_dir: Option<&std::path::Path>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;

        // Get IDs of unpinned items without tags.
        let mut stmt = conn
            .prepare(
                "SELECT id FROM clipboard_history 
             WHERE is_pinned = 0 
               AND NOT EXISTS (SELECT 1 FROM entry_tags WHERE entry_id = clipboard_history.id)",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = rows.filter_map(Result::ok).collect();

        // Delete one-by-one so tombstones are recorded for cloud deletion sync.
        for id in &ids {
            self.delete_with_conn(&conn, *id, data_dir)?;
        }

        // VACUUM to reclaim space
        let _ = conn.execute_batch("VACUUM;");
        Ok(())
    }

    fn get_count(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM clipboard_history")
            .map_err(|e| e.to_string())?;
        let count: i64 = stmt
            .query_row([], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        Ok(count)
    }

    fn increment_use_count(&self, id: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE clipboard_history SET use_count = use_count + 1 WHERE id = ?",
            params![id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn touch_entry(&self, id: i64, timestamp: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE clipboard_history SET timestamp = ? WHERE id = ?",
            params![timestamp, id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn toggle_pin(&self, id: i64, is_pinned: bool) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.toggle_pin_with_conn(&conn, id, is_pinned)
    }

    fn update_pinned_order(&self, orders: Vec<(i64, i64)>) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        self.update_pinned_order_with_conn(&tx, orders)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn get_entry_by_id(&self, id: i64) -> Result<Option<ClipboardEntry>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.get_entry_by_id_with_conn(&conn, id)
    }

    fn get_entry_by_content(
        &self,
        content: &str,
        content_type: Option<&str>,
    ) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.find_by_content_with_conn(&conn, content, content_type)
    }

    fn update_entry_content(&self, id: i64, content: &str, preview: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.update_entry_content_with_conn(&conn, id, content, preview)
    }

    fn get_entry_content(&self, id: i64) -> Result<Option<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT content FROM clipboard_history WHERE id = ?")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let content: String = row.get(0).map_err(|e| e.to_string())?;
            Ok(Some(self.maybe_decrypt_text(&content)))
        } else {
            Ok(None)
        }
    }

    fn get_entry_content_full(&self, id: i64) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.get_entry_content_full_with_conn(&conn, id)
    }

    fn get_entry_content_with_html(
        &self,
        id: i64,
    ) -> Result<Option<(String, String, Option<String>)>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        self.get_entry_content_with_html_with_conn(&conn, id)
    }
}

/// Regression tests for the indexed search path.
///
/// The bug being locked down: search used `LIKE '%term%'`, which cannot use an index
/// and degenerated into a full table scan whose cost grew with the database size. The
/// fix routes search through an FTS5 trigram index, so these tests assert that the
/// indexed path returns *exactly* what the linear `LIKE` path returned.
#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::infrastructure::repository::migrations::{run_migrations, FtsBackfill};
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn setup() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&conn).expect("migrations");
        Arc::new(Mutex::new(conn))
    }

    fn insert(conn: &Connection, id: i64, content: &str, app: &str, ts: i64) {
        conn.execute(
            "INSERT INTO clipboard_history
                 (id, content_type, content, source_app, timestamp, preview, is_pinned, tags,
                  use_count, pinned_order, content_hash, html_content, is_external, source_app_path)
             VALUES (?1, 'text', ?2, ?3, ?4, ?2, 0, '[]', 0, 0, 0, NULL, 0, NULL)",
            params![id, content, app, ts],
        )
        .expect("insert row");
    }

    fn tag(conn: &Connection, id: i64, name: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag) VALUES (?1, ?2)",
            params![id, name],
        )
        .expect("insert tag");
    }

    /// Indexes everything and flips the ready flag, i.e. simulates a completed backfill.
    fn finish_backfill(conn: &Connection) {
        let mut bf = FtsBackfill::new();
        loop {
            let written = bf.step(conn, 2).expect("backfill step");
            if written == 0 || bf.is_done() {
                break;
            }
        }
        crate::database::set_fts_state(conn, crate::database::FTS_STATE_READY).expect("set state");
    }

    /// Reference implementation: the original `LIKE '%term%'` semantics, restricted to
    /// non-sensitive rows. Only used to assert the indexed path agrees with it.
    fn reference_ids(conn: &Connection, term: &str, limit: i32) -> Vec<i64> {
        let mut stmt = conn
            .prepare(
                "SELECT ch.id
                   FROM clipboard_history ch
                   LEFT JOIN entry_tags et ON ch.id = et.entry_id
                  WHERE NOT EXISTS (
                      SELECT 1 FROM entry_tags se
                       WHERE se.entry_id = ch.id AND se.tag COLLATE NOCASE IN ('sensitive','密码')
                  )
                    AND (ch.content LIKE '%' || ?1 || '%'
                         OR ch.source_app LIKE '%' || ?1 || '%'
                         OR et.tag LIKE '%' || ?1 || '%')
                  ORDER BY ch.timestamp DESC, ch.id DESC
                  LIMIT ?2",
            )
            .expect("prepare reference");
        let mut ids: Vec<i64> = stmt
            .query_map(params![term, limit], |row| row.get(0))
            .expect("query reference")
            .filter_map(|r| r.ok())
            .collect();
        ids.dedup();
        ids
    }

    fn search_ids(repo: &SqliteClipboardRepository, term: &str, limit: i32) -> Vec<i64> {
        repo.search(term, limit)
            .expect("search")
            .into_iter()
            .map(|e| e.id)
            .collect()
    }

    /// Faithful re-implementation of the ORIGINAL `search()` body (pre-fix), used as the
    /// benchmark baseline. It runs the old `LIKE '%term%'` query, materialises full rows,
    /// and then the old sensitive pass with its redundant `OR ... LIKE 'dpapi:%'`
    /// predicate. Without this, comparing against an id-only query would flatter the fix.
    fn legacy_search_ids(
        conn: &Connection,
        repo: &SqliteClipboardRepository,
        term: &str,
        limit: i32,
    ) -> Vec<i64> {
        let sensitive = sensitive_tags_sql();
        let mut results: Vec<ClipboardEntry> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();

        let sql = format!(
            "SELECT DISTINCT {cols}
               FROM clipboard_history ch
               LEFT JOIN entry_tags et ON ch.id = et.entry_id
              WHERE NOT EXISTS (SELECT 1 FROM entry_tags se
                                 WHERE se.entry_id = ch.id
                                   AND se.tag COLLATE NOCASE IN {sensitive})
                AND (ch.content LIKE '%' || ?1 || '%'
                     OR ch.source_app LIKE '%' || ?1 || '%'
                     OR et.tag LIKE '%' || ?1 || '%')
              ORDER BY ch.timestamp DESC, ch.id DESC
              LIMIT ?2",
            cols = ENTRY_COLUMNS,
            sensitive = sensitive
        );
        {
            let mut stmt = conn.prepare(&sql).expect("legacy prepare");
            let rows = stmt
                .query_map(params![term, limit], |row| repo.row_to_entry(row))
                .expect("legacy query");
            for row in rows.flatten() {
                if seen.insert(row.id) {
                    results.push(row);
                }
            }
        }

        if results.len() < limit as usize {
            let sql2 = format!(
                "SELECT {cols} FROM clipboard_history ch
                  WHERE ( EXISTS (SELECT 1 FROM entry_tags se
                                   WHERE se.entry_id = ch.id
                                     AND se.tag COLLATE NOCASE IN {sensitive})
                       OR ch.content LIKE 'dpapi:%'
                       OR ch.preview LIKE 'dpapi:%'
                       OR ch.html_content LIKE 'dpapi:%' )
                    AND ((ch.timestamp < ?1) OR (ch.timestamp = ?1 AND ch.id < ?2))
                  ORDER BY ch.timestamp DESC, ch.id DESC
                  LIMIT ?3",
                cols = ENTRY_COLUMNS,
                sensitive = sensitive
            );
            let mut cursor_ts = i64::MAX;
            let mut cursor_id = i64::MAX;
            loop {
                let mut batch: Vec<ClipboardEntry> = Vec::new();
                {
                    let mut stmt = conn.prepare(&sql2).expect("legacy2 prepare");
                    let rows = stmt
                        .query_map(params![cursor_ts, cursor_id, 500], |row| repo.row_to_entry(row))
                        .expect("legacy2 query");
                    for row in rows.flatten() {
                        batch.push(row);
                    }
                }
                if batch.is_empty() {
                    break;
                }
                for entry in &batch {
                    let hit = entry.content.to_lowercase().contains(term)
                        || entry.source_app.to_lowercase().contains(term)
                        || entry.tags.iter().any(|t| t.to_lowercase().contains(term));
                    if hit && seen.insert(entry.id) {
                        results.push(entry.clone());
                        if results.len() >= limit as usize {
                            break;
                        }
                    }
                }
                if results.len() >= limit as usize {
                    break;
                }
                match batch.last() {
                    Some(last) => {
                        cursor_ts = last.timestamp;
                        cursor_id = last.id;
                    }
                    None => break,
                }
            }
        }

        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.id.cmp(&a.id)));
        results.truncate(limit as usize);
        results.into_iter().map(|e| e.id).collect()
    }

    const CORPUS: &[(&str, &str, i64)] = &[
        ("hello world sqlite database", "Code", 1000),
        ("SQLite full text search with trigram", "Code", 2000),
        ("这是一个中文测试内容，包含功能性描述", "Notepad", 3000),
        ("世界和平是我们的共同愿望", "WeChat", 4000),
        ("plain text entry about nothing", "Notepad", 5000),
        ("prefix_sqlite_mid_sqlite_suffix", "Code", 6000),
        ("mixed 中文 and english content", "Notepad", 7000),
    ];

    fn populate(conn: &Connection) {
        for (i, (content, app, ts)) in CORPUS.iter().enumerate() {
            insert(conn, (i + 1) as i64, content, app, *ts);
        }
        tag(conn, 6, "work");
    }

    #[test]
    fn indexed_search_matches_like_for_terms_of_three_or_more_chars() {
        for term in [
            "sqlite",
            "SQLITE",
            "trigram",
            "functional",
            "功能性",
            "世界和平",
            "prefix_sqlite_mid",
            "中文",
        ] {
            // Each term gets a fresh database so the comparison is independent.
            let shared = setup();
            let expected = {
                let conn = shared.lock().unwrap();
                populate(&conn);
                finish_backfill(&conn);
                assert!(
                    crate::database::is_fts_ready(&conn),
                    "backfill should report ready"
                );
                reference_ids(&conn, &term.to_lowercase(), 50)
            };
            // NOTE: the guard above is dropped before search(), which takes the same
            // mutex - holding it would deadlock.
            let repo = SqliteClipboardRepository::new(shared.clone());
            let actual = search_ids(&repo, term, 50);

            assert_eq!(
                actual, expected,
                "indexed search for {:?} disagreed with LIKE semantics",
                term
            );
        }
    }

    #[test]
    fn short_terms_fall_back_and_still_match() {
        let shared = setup();
        {
            let conn = shared.lock().unwrap();
            populate(&conn);
            finish_backfill(&conn);
        }
        let repo = SqliteClipboardRepository::new(shared.clone());

        // The trigram tokenizer cannot answer <3 characters; the fallback must.
        let ids = search_ids(&repo, "世界", 50);
        assert_eq!(ids, vec![4], "2-char CJK term should match row 4");

        let ids = search_ids(&repo, "sq", 50);
        assert_eq!(ids, vec![6, 2, 1], "2-char ASCII term should match by LIKE");
    }

    #[test]
    fn terms_before_backfill_completes_use_the_fallback() {
        let shared = setup();
        {
            let conn = shared.lock().unwrap();
            populate(&conn);
            // Deliberately do NOT finish the backfill.
            assert!(!crate::database::is_fts_ready(&conn));
        }
        let repo = SqliteClipboardRepository::new(shared.clone());
        let ids = search_ids(&repo, "sqlite", 50);
        assert_eq!(
            ids,
            vec![6, 2, 1],
            "un-indexed database must still return LIKE results"
        );
    }

    #[test]
    fn fts_operator_syntax_in_user_input_is_not_interpreted() {
        let shared = setup();
        {
            let conn = shared.lock().unwrap();
            populate(&conn);
            finish_backfill(&conn);
        }
        let repo = SqliteClipboardRepository::new(shared.clone());

        // If the term were interpolated raw into MATCH these would raise an FTS5 syntax
        // error or silently change the query's meaning.
        for term in ["AND", "OR NOT", "\"quoted\"", "a*", "(paren)", "col:x", "NEAR/2"] {
            let res = repo.search(term, 50);
            assert!(res.is_ok(), "term {:?} must not error: {:?}", term, res.err());
        }

        // A term containing a double quote must be treated literally, not as syntax.
        let ids = search_ids(&repo, "\"sqlite\"", 50);
        assert!(
            ids.is_empty(),
            "no row literally contains quotes; got {:?}",
            ids
        );
    }

    #[test]
    fn sensitive_encrypted_entries_are_still_searchable() {
        let shared = setup();
        {
            let conn = shared.lock().unwrap();
            insert(&conn, 1, "public note about sqlite", "Code", 1000);
            insert(&conn, 2, "top secret api key sqlite", "Code", 2000);
            tag(&conn, 2, "sensitive");
            finish_backfill(&conn);

            // Encrypt row 2 the way the app does on capture.
            let repo = SqliteClipboardRepository::new(shared.clone());
            repo.encrypt_entry_with_conn(&conn, 2).expect("encrypt");

            let raw: String = conn
                .query_row("SELECT content FROM clipboard_history WHERE id = 2", [], |r| r.get(0))
                .unwrap();
            assert!(
                raw.starts_with(ENCRYPT_PREFIX),
                "row 2 should be encrypted at rest"
            );
        }

        let repo = SqliteClipboardRepository::new(shared.clone());
        let ids = search_ids(&repo, "secret", 50);
        assert_eq!(
            ids,
            vec![2],
            "plaintext search must still reach the encrypted entry"
        );

        // And a term only present in the public row must not pull in the secret one.
        let ids = search_ids(&repo, "public", 50);
        assert_eq!(ids, vec![1]);
    }

    /// End-to-end verification of the real upgrade path: takes a *copy* of a production
    /// `clipboard.db` (still on migration 9, no FTS objects), applies migration 10,
    /// runs the real background backfill, and then measures and validates the real
    /// search path against the original `LIKE` semantics.
    ///
    /// Opt-in because it needs a real database:
    /// `set TIEZ_REAL_DB=<copy of clipboard.db>`
    /// `cargo test --bin tiez-app -- --ignored --nocapture`
    #[test]
    #[ignore = "needs TIEZ_REAL_DB pointing at a copy of a real clipboard.db"]
    fn real_database_upgrade_path_is_correct_and_fast() {
        use std::time::Instant;

        let path = match std::env::var("TIEZ_REAL_DB") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => {
                eprintln!("TIEZ_REAL_DB not set; skipping real-database verification");
                return;
            }
        };
        assert!(
            std::path::Path::new(&path).exists(),
            "TIEZ_REAL_DB does not exist: {}",
            path
        );

        let shared = Arc::new(Mutex::new(Connection::open(&path).expect("open real db")));
        let (before_version, rows, before_fts) = {
            let conn = shared.lock().unwrap();
            conn.execute_batch("PRAGMA mmap_size=268435456; PRAGMA cache_size=-16384;")
                .expect("pragmas");
            let v: i32 = conn
                .query_row("SELECT COALESCE(MAX(version),0) FROM schema_migrations", [], |r| r.get(0))
                .unwrap_or(0);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM clipboard_history", [], |r| r.get(0))
                .expect("count");
            let fts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%fts%'",
                    [],
                    |r| r.get(0),
                )
                .expect("fts count");
            (v, n, fts)
        };
        eprintln!(
            "[REALDB] before: migration={} rows={} fts_objects={}",
            before_version, rows, before_fts
        );
        assert_eq!(before_fts, 0, "expected a pre-FTS database");

        // Apply migration 10 the way the app does at startup.
        let migrate_ms = {
            let conn = shared.lock().unwrap();
            let t = Instant::now();
            run_migrations(&conn).expect("run_migrations");
            t.elapsed().as_secs_f64() * 1000.0
        };
        {
            let conn = shared.lock().unwrap();
            let after: i32 = conn
                .query_row("SELECT COALESCE(MAX(version),0) FROM schema_migrations", [], |r| r.get(0))
                .unwrap();
            let fts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%fts%' AND type='table'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            eprintln!("[REALDB] migration applied in {:.0} ms; version now {}; fts tables {}", migrate_ms, after, fts);
            assert!(after >= 10, "migration 10 must be recorded");
            assert!(fts >= 1, "clipboard_fts must exist");
        }

        // Run the real batched backfill exactly as spawn_fts_backfill does.
        let backfill = {
            let mut bf = FtsBackfill::new();
            let t = Instant::now();
            let mut total = 0i64;
            loop {
                let n = {
                    let conn = shared.lock().unwrap();
                    bf.step(&conn, 500).expect("backfill step")
                };
                total += n;
                if n == 0 || bf.is_done() {
                    break;
                }
            }
            (t.elapsed().as_secs_f64(), total)
        };
        {
            let conn = shared.lock().unwrap();
            crate::database::set_fts_state(&conn, crate::database::FTS_STATE_READY).expect("ready");
        }
        eprintln!(
            "[REALDB] backfill: {} rows in {:.1} s",
            backfill.1, backfill.0
        );
        assert_eq!(backfill.1, rows, "every row must be indexed");

        // The backfill leaves a very large WAL; checkpoint it and warm the page cache so
        // the numbers reflect steady state rather than first-touch I/O.
        {
            let conn = shared.lock().unwrap();
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }

        // Compare the indexed path against the original LIKE semantics, and time both.
        // Median of N runs after a warm-up; single cold runs on a 650 MB database are
        // dominated by I/O noise.
        const REPS: usize = 5;
        let terms = ["TieZ", "sqlite", "http", "clip", "的一个", "zzqqxx_not_found"];
        for term in terms {
            let repo = SqliteClipboardRepository::new(shared.clone());

            // warm-up pass for both paths
            let like_ids = {
                let conn = shared.lock().unwrap();
                legacy_search_ids(&conn, &repo, term, 200)
            };
            let _ = repo.search(term, 200).expect("warm-up search");

            let mut like_ms = Vec::new();
            for _ in 0..REPS {
                let conn = shared.lock().unwrap();
                let t = Instant::now();
                let _ = legacy_search_ids(&conn, &repo, term, 200);
                like_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }

            let mut idx_ms = Vec::new();
            let mut got: Vec<i64> = Vec::new();
            for _ in 0..REPS {
                let t = Instant::now();
                got = repo
                    .search(term, 200)
                    .expect("indexed search")
                    .into_iter()
                    .map(|e| e.id)
                    .collect();
                idx_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }

            let med = |v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            };
            let like_med = med(&mut like_ms);
            let idx_med = med(&mut idx_ms);

            eprintln!(
                "[REALDB] term={:<20} ORIGINAL {:8.1} ms ({} ids) | FIXED {:8.1} ms ({} ids) | speedup {:.1}x",
                format!("{:?}", term),
                like_med,
                like_ids.len(),
                idx_med,
                got.len(),
                like_med / idx_med.max(0.001)
            );

            // Same id set (order may differ for equal timestamps), same size.
            let mut a = like_ids.clone();
            let mut b = got.clone();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                b, a,
                "indexed search disagreed with LIKE for term {:?}",
                term
            );
        }
        eprintln!("[REALDB] all terms agreed with LIKE semantics");
    }
}
