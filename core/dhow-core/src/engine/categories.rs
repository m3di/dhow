//! Categories (ENGINE-SPEC §13.1): the user's extra extensions and the
//! storage breakdown of completed downloads.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use super::{DownloadId, Engine, EngineError};
use crate::naming::{Category, ExtraExtensions, normalize_extension};

const EXTRA_KEY: &str = "extra_extensions";

/// Completed downloads of one category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CategoryUsage {
    pub category: Category,
    /// Size of those whose file still exists.
    pub bytes: u64,
    pub files: u64,
    /// Those whose file is gone (moved or deleted); not in `bytes`.
    pub missing: Vec<DownloadId>,
}

impl Engine {
    pub fn extra_extensions(&self) -> Result<ExtraExtensions, EngineError> {
        Ok(self.setting(EXTRA_KEY)?.unwrap_or_default())
    }

    /// Replaces `category`'s extra extensions (empty clears them). An
    /// extension belongs to one category, so it leaves any other; `Other`
    /// takes built-ins back out of theirs.
    pub fn set_extra_extensions(
        &self,
        category: Category,
        extensions: &[String],
    ) -> Result<ExtraExtensions, EngineError> {
        let mut exts = extensions
            .iter()
            .map(|e| normalize_extension(e))
            .collect::<Option<Vec<_>>>()
            .ok_or(EngineError::InvalidSetting("not a file extension"))?;
        exts.sort();
        exts.dedup();
        let db = self.db();
        let tx = db.conn().unchecked_transaction()?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [EXTRA_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let mut extra: ExtraExtensions = stored
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        for list in extra.values_mut() {
            list.retain(|e| !exts.contains(e));
        }
        extra.insert(category, exts);
        extra.retain(|_, list| !list.is_empty());
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![
                EXTRA_KEY,
                serde_json::to_string(&extra).expect("settings serialize")
            ],
        )?;
        tx.commit()?;
        Ok(extra)
    }

    /// `total_size` of `Completed` downloads by category, in
    /// [`Category::ALL`] order; categories without any are left out.
    pub fn storage_breakdown(&self) -> Result<Vec<CategoryUsage>, EngineError> {
        let rows: Vec<(DownloadId, String, Option<i64>, String)> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT id, final_path, total_size, category FROM downloads
                 WHERE state = 'completed' ORDER BY completed_at, id",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<_, _>>()?
        };
        let mut usage: Vec<CategoryUsage> = Category::ALL
            .into_iter()
            .map(|category| CategoryUsage {
                category,
                bytes: 0,
                files: 0,
                missing: Vec::new(),
            })
            .collect();
        for (id, final_path, total_size, category) in rows {
            let category = Category::parse(&category).unwrap_or(Category::Other);
            let u = &mut usage[category as usize];
            match locate(Path::new(&final_path)).and_then(|p| std::fs::metadata(p).ok()) {
                Some(meta) => {
                    u.bytes += total_size.map_or(meta.len(), |t| t as u64);
                    u.files += 1;
                }
                None => u.missing.push(id),
            }
        }
        usage.retain(|u| u.files > 0 || !u.missing.is_empty());
        Ok(usage)
    }
}

/// Where a completed file is now. M3.5: resolve the download's
/// `file_token` through the platform adapter first, so a moved file is
/// still found.
fn locate(final_path: &Path) -> Option<PathBuf> {
    final_path.is_file().then(|| final_path.to_owned())
}
