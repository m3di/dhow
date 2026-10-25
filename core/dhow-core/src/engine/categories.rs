//! The storage breakdown of completed downloads by category (ENGINE-SPEC
//! §13.1). The user's extra extensions are a setting ([`super::settings`]).

use super::{DownloadId, Engine, EngineError, EngineEvent};
use crate::naming::Category;

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
    /// `total_size` of `Completed` downloads by category, in
    /// [`Category::ALL`] order; categories without any are left out. Moved
    /// files are found as in [`Engine::get`].
    pub fn storage_breakdown(
        &self,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Vec<CategoryUsage>, EngineError> {
        let ids: Vec<DownloadId> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT id FROM downloads WHERE state = 'completed' ORDER BY completed_at, id",
            )?;
            stmt.query_map([], |r| r.get(0))?
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
        for id in ids {
            let snap = self.get(&id, on_event)?;
            let u = &mut usage[snap.category as usize];
            match std::fs::metadata(&snap.final_path) {
                Ok(meta) if meta.is_file() => {
                    u.bytes += snap.total_size.unwrap_or(meta.len());
                    u.files += 1;
                }
                _ => u.missing.push(id),
            }
        }
        usage.retain(|u| u.files > 0 || !u.missing.is_empty());
        Ok(usage)
    }
}
