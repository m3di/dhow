//! Settings (ARCHITECTURE §3.1, §5.1): what the user can change, one
//! `settings` row per key, changed by a [`SettingsPatch`] that applies
//! whole or not at all. A setting without a row, or with one this version
//! can't read or would refuse, has its default: the engine's
//! [`DownloadConfig`]. Rows of keys this version doesn't know (a newer one
//! wrote them) are left alone.

use std::collections::HashMap;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::proxy::Stored;
use super::{Engine, EngineError, EngineEvent};
use crate::download::DownloadConfig;
use crate::download::budget::BudgetConfig;
use crate::naming::{Category, ExtraExtensions, normalize_extension};
use crate::platform::ProxyMode;

const GLOBAL_SPEED_LIMIT: &str = "global_speed_limit";
const HEADROOM_PERCENT: &str = "headroom_percent";
const GLOBAL_CONNECTIONS: &str = "global_connections";
const PER_HOST_CONNECTIONS: &str = "per_host_connections";
const MIN_CONNECTIONS_FOR_NEW: &str = "min_connections_for_new";
const DEFAULT_CONNECTIONS: &str = "default_connections";
const MAX_CONNECTIONS_PER_DOWNLOAD: &str = "max_connections_per_download";
const FIRST_BYTE_TIMEOUT_MS: &str = "first_byte_timeout_ms";
const IDLE_TIMEOUT_MS: &str = "idle_timeout_ms";
const MAX_RETRIES: &str = "max_retries";
const COMMIT_INTERVAL_MS: &str = "commit_interval_ms";
const PROBE_RANGE_GET: &str = "probe_range_get";
const USE_CONTENT_DISPOSITION: &str = "use_content_disposition";
const APPEND_TYPE_EXTENSION: &str = "append_type_extension";
const AUTO_GROUPING: &str = "auto_grouping";
const EXTRA_EXTENSIONS: &str = "extra_extensions";
const PROXY_MODE: &str = "proxy_mode";

/// Every setting, as in effect. Speed limits apply to running downloads at
/// once (in another process at its next [`Engine::refresh_limits`]); the
/// rest from the next add, download run or lease, except the connection
/// budget, which an engine reads when it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Bytes/s for everything together; `None`: unlimited (ENGINE-SPEC §12).
    pub global_speed_limit: Option<u64>,
    /// Share of the link to leave free, 0–99 (§10.6).
    pub headroom_percent: u8,
    /// Connections of all downloads together, at least 1 (§10.1).
    pub global_connections: u32,
    /// Per `host:port`, at least 1 (§10.5).
    pub per_host_connections: u32,
    /// 1: a newcomer gets a connection given up for it; 0: it waits (§10.3).
    pub min_connections_for_new: u32,
    /// Connections of a download that doesn't ask for its own number, 1 to
    /// `max_connections_per_download` (§4.1).
    pub default_connections: u8,
    /// No download uses more, whatever it asks for; at least 1 (§4.1).
    pub max_connections_per_download: u8,
    /// From sending a request to its response headers, 100 ms–1 h (§9).
    pub first_byte_timeout_ms: u64,
    /// Longest silence while reading a body, 100 ms–1 h (§9).
    pub idle_timeout_ms: u64,
    /// Consecutive failed attempts per connection that give up, at least 1
    /// (§9).
    pub max_retries: u32,
    /// Longest time received bytes stay uncommitted, 100 ms–60 s (§5.3).
    pub commit_interval_ms: u64,
    /// Probe with a one-byte `GET` when `HEAD` doesn't tell (§2).
    pub probe_range_get: bool,
    /// Name files from `Content-Disposition` when the server sends one (§3).
    pub use_content_disposition: bool,
    /// Give a name without an extension the `Content-Type`'s (§3).
    pub append_type_extension: bool,
    /// `Add` puts downloads in groups (§13.2); off, groups stay as they are
    /// and `SetGroup` still works.
    pub auto_grouping: bool,
    /// Extensions the user put in categories (§13.1).
    pub extra_extensions: ExtraExtensions,
    pub proxy_mode: ProxyMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self::defaults(&DownloadConfig::default())
    }
}

/// The new global speed limit. Not `Option<Option<u64>>`: a binding has to
/// tell "leave it" from "unlimited", and Kotlin's `T??` is `T?`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitPatch {
    Unlimited,
    Rate(u64),
}

impl LimitPatch {
    pub fn from_rate(rate: Option<u64>) -> Self {
        rate.map_or(Self::Unlimited, Self::Rate)
    }
}

/// One category's extra extensions, replacing its earlier ones (empty
/// clears them). An extension belongs to one category, so it leaves any
/// other; `Other` takes built-ins back out of theirs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CategoryExtensions {
    pub category: Category,
    pub extensions: Vec<String>,
}

/// `None` (empty for `extra_extensions`) leaves a setting as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsPatch {
    pub global_speed_limit: Option<LimitPatch>,
    pub headroom_percent: Option<u8>,
    pub global_connections: Option<u32>,
    pub per_host_connections: Option<u32>,
    pub min_connections_for_new: Option<u32>,
    pub default_connections: Option<u8>,
    pub max_connections_per_download: Option<u8>,
    pub first_byte_timeout_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub commit_interval_ms: Option<u64>,
    pub probe_range_get: Option<bool>,
    pub use_content_disposition: Option<bool>,
    pub append_type_extension: Option<bool>,
    pub auto_grouping: Option<bool>,
    /// Applied in order.
    pub extra_extensions: Vec<CategoryExtensions>,
    pub proxy_mode: Option<ProxyMode>,
}

const TIMEOUT_MS: std::ops::RangeInclusive<u64> = 100..=3_600_000;
const COMMIT_MS: std::ops::RangeInclusive<u64> = 100..=60_000;

fn ms(d: Duration) -> u64 {
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

impl Settings {
    fn defaults(config: &DownloadConfig) -> Self {
        Self {
            global_speed_limit: None,
            headroom_percent: 0,
            global_connections: config.budget.global,
            per_host_connections: config.budget.per_host,
            min_connections_for_new: config.budget.min_for_new,
            default_connections: config.ramp.connections,
            max_connections_per_download: config.ramp.max_connections,
            first_byte_timeout_ms: ms(config.transfer.first_byte),
            idle_timeout_ms: ms(config.transfer.idle),
            max_retries: config.transfer.retry.max_failures,
            commit_interval_ms: ms(config.writer.commit_interval),
            probe_range_get: config.probe.range_get,
            use_content_disposition: true,
            append_type_extension: true,
            auto_grouping: true,
            extra_extensions: ExtraExtensions::new(),
            proxy_mode: ProxyMode::System,
        }
    }

    pub(super) fn load(conn: &Connection, config: &DownloadConfig) -> Result<Self, EngineError> {
        let rows: HashMap<String, String> = {
            let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let mut s = Self::defaults(config);
        if let Some(v) = read::<Option<u64>>(&rows, GLOBAL_SPEED_LIMIT) {
            s.global_speed_limit = v;
        }
        // Each only as a patch could have set it, so a hand-edited row
        // can't stall the budget.
        macro_rules! load {
            ($field:ident, $key:expr, |$v:ident| $valid:expr) => {
                if let Some($v) = read(&rows, $key) {
                    if $valid {
                        s.$field = $v;
                    } else {
                        tracing::warn!(key = $key, "ignoring out-of-range setting");
                    }
                }
            };
        }
        load!(headroom_percent, HEADROOM_PERCENT, |v| v <= 99);
        load!(global_connections, GLOBAL_CONNECTIONS, |v| v >= 1);
        load!(per_host_connections, PER_HOST_CONNECTIONS, |v| v >= 1);
        load!(min_connections_for_new, MIN_CONNECTIONS_FOR_NEW, |v| v <= 1);
        load!(
            max_connections_per_download,
            MAX_CONNECTIONS_PER_DOWNLOAD,
            |v| v >= 1
        );
        load!(first_byte_timeout_ms, FIRST_BYTE_TIMEOUT_MS, |v| TIMEOUT_MS
            .contains(&v));
        load!(idle_timeout_ms, IDLE_TIMEOUT_MS, |v| TIMEOUT_MS
            .contains(&v));
        load!(max_retries, MAX_RETRIES, |v| v >= 1);
        load!(commit_interval_ms, COMMIT_INTERVAL_MS, |v| COMMIT_MS
            .contains(&v));
        load!(probe_range_get, PROBE_RANGE_GET, |v| true);
        load!(use_content_disposition, USE_CONTENT_DISPOSITION, |v| true);
        load!(append_type_extension, APPEND_TYPE_EXTENSION, |v| true);
        load!(auto_grouping, AUTO_GROUPING, |v| true);
        let max = s.max_connections_per_download;
        load!(default_connections, DEFAULT_CONNECTIONS, |v| (1..=max)
            .contains(&v));
        // A default above a lowered maximum would be capped anyway.
        s.default_connections = s.default_connections.clamp(1, max);
        if let Some(v) = read(&rows, EXTRA_EXTENSIONS) {
            s.extra_extensions = v;
        }
        match read::<Stored>(&rows, PROXY_MODE).map(Stored::mode) {
            Some(Some(mode)) => s.proxy_mode = mode,
            Some(None) => tracing::warn!(key = PROXY_MODE, "ignoring unreadable setting"),
            None => {}
        }
        Ok(s)
    }

    /// The budget of an engine opened with these.
    pub(super) fn budget(&self) -> BudgetConfig {
        BudgetConfig {
            global: self.global_connections,
            per_host: self.per_host_connections,
            min_for_new: self.min_connections_for_new,
        }
    }

    /// `config` with the settings that apply per download run.
    pub(super) fn apply(&self, mut config: DownloadConfig) -> DownloadConfig {
        config.ramp.connections = self.default_connections;
        config.ramp.max_connections = self.max_connections_per_download;
        config.transfer.first_byte = Duration::from_millis(self.first_byte_timeout_ms);
        config.transfer.idle = Duration::from_millis(self.idle_timeout_ms);
        config.transfer.retry.max_failures = self.max_retries;
        config.writer.commit_interval = Duration::from_millis(self.commit_interval_ms);
        config.probe.range_get = self.probe_range_get;
        config
    }
}

/// `key`'s value; `None` without a row or with one that doesn't parse.
fn read<T: DeserializeOwned>(rows: &HashMap<String, String>, key: &str) -> Option<T> {
    serde_json::from_str(rows.get(key)?)
        .inspect_err(|e| tracing::warn!(key, error = %e, "ignoring unreadable setting"))
        .ok()
}

fn put(conn: &Connection, key: &str, value: &Value) -> Result<(), EngineError> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

fn invalid(ok: bool, why: &'static str) -> Result<(), EngineError> {
    if ok {
        Ok(())
    } else {
        Err(EngineError::InvalidSetting(why))
    }
}

impl Engine {
    pub fn settings(&self) -> Result<Settings, EngineError> {
        Settings::load(self.db().conn(), &self.config)
    }

    /// Checks every field of `patch` and stores them in one transaction:
    /// with any field invalid, nothing changes. Once it returns `Ok`, the
    /// patch is stored. Emits `SettingsChanged` unless the patch is empty.
    pub fn set_settings(
        &self,
        patch: SettingsPatch,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Settings, EngineError> {
        let settings = {
            let db = self.db();
            let tx = db.conn().unchecked_transaction()?;
            let mut s = Settings::load(&tx, &self.config)?;
            let mut rows: Vec<(&str, Value)> = Vec::new();
            macro_rules! set {
                ($field:ident, $key:expr, |$v:ident| $valid:expr, $why:expr) => {
                    if let Some($v) = patch.$field {
                        invalid($valid, $why)?;
                        s.$field = $v;
                        rows.push(($key, json!($v)));
                    }
                };
            }
            if let Some(limit) = patch.global_speed_limit {
                s.global_speed_limit = match limit {
                    LimitPatch::Unlimited => None,
                    LimitPatch::Rate(r) => Some(r),
                };
                rows.push((GLOBAL_SPEED_LIMIT, json!(s.global_speed_limit)));
            }
            set!(
                headroom_percent,
                HEADROOM_PERCENT,
                |v| v <= 99,
                "headroom is 0–99 %"
            );
            set!(
                global_connections,
                GLOBAL_CONNECTIONS,
                |v| v >= 1,
                "global connections must be at least 1"
            );
            set!(
                per_host_connections,
                PER_HOST_CONNECTIONS,
                |v| v >= 1,
                "per-host connections must be at least 1"
            );
            set!(
                min_connections_for_new,
                MIN_CONNECTIONS_FOR_NEW,
                |v| v <= 1,
                "connections for a newcomer are 0 or 1"
            );
            set!(
                max_connections_per_download,
                MAX_CONNECTIONS_PER_DOWNLOAD,
                |v| v >= 1,
                "connections per download must be at least 1"
            );
            set!(
                default_connections,
                DEFAULT_CONNECTIONS,
                |v| v >= 1,
                "default connections must be at least 1"
            );
            set!(
                first_byte_timeout_ms,
                FIRST_BYTE_TIMEOUT_MS,
                |v| TIMEOUT_MS.contains(&v),
                "timeouts are 100 ms to 1 h"
            );
            set!(
                idle_timeout_ms,
                IDLE_TIMEOUT_MS,
                |v| TIMEOUT_MS.contains(&v),
                "timeouts are 100 ms to 1 h"
            );
            set!(
                max_retries,
                MAX_RETRIES,
                |v| v >= 1,
                "retries must be at least 1"
            );
            set!(
                commit_interval_ms,
                COMMIT_INTERVAL_MS,
                |v| COMMIT_MS.contains(&v),
                "the commit interval is 100 ms to 60 s"
            );
            set!(probe_range_get, PROBE_RANGE_GET, |v| true, "");
            set!(
                use_content_disposition,
                USE_CONTENT_DISPOSITION,
                |v| true,
                ""
            );
            set!(append_type_extension, APPEND_TYPE_EXTENSION, |v| true, "");
            set!(auto_grouping, AUTO_GROUPING, |v| true, "");
            invalid(
                s.default_connections <= s.max_connections_per_download,
                "default connections above the per-download maximum",
            )?;
            if !patch.extra_extensions.is_empty() {
                for c in &patch.extra_extensions {
                    let mut exts = c
                        .extensions
                        .iter()
                        .map(|e| normalize_extension(e))
                        .collect::<Option<Vec<_>>>()
                        .ok_or(EngineError::InvalidSetting("not a file extension"))?;
                    exts.sort();
                    exts.dedup();
                    for list in s.extra_extensions.values_mut() {
                        list.retain(|e| !exts.contains(e));
                    }
                    s.extra_extensions.insert(c.category, exts);
                }
                s.extra_extensions.retain(|_, list| !list.is_empty());
                rows.push((EXTRA_EXTENSIONS, json!(s.extra_extensions)));
            }
            if let Some(mode) = &patch.proxy_mode {
                let stored = serde_json::to_value(Stored::new(mode)?).expect("settings serialize");
                s.proxy_mode = mode.clone();
                rows.push((PROXY_MODE, stored));
            }
            for (key, value) in &rows {
                put(&tx, key, value)?;
            }
            tx.commit()?;
            s
        };
        // Stored; what runs here catches up, or at its next refresh.
        if (patch.global_speed_limit.is_some() || patch.headroom_percent.is_some())
            && let Err(e) = self.refresh_limits()
        {
            tracing::warn!(error = %e, "settings stored, limits not refreshed");
        }
        if patch != SettingsPatch::default() {
            on_event(EngineEvent::SettingsChanged {
                settings: settings.clone(),
            });
        }
        Ok(settings)
    }

    /// The engine's config with the settings that apply per download.
    pub(super) fn run_config(&self) -> Result<DownloadConfig, EngineError> {
        Ok(self.settings()?.apply(self.config))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use dhow_testserver::TestServer;
    use reqwest::Url;
    use tokio::sync::watch;

    use super::*;
    use crate::engine::{AddRequest, State};
    use crate::naming::TargetFs;
    use crate::probe::Resumable;

    fn open(dir: &Path) -> Engine {
        Engine::open(&dir.join("data"), DownloadConfig::default()).unwrap()
    }

    fn set(engine: &Engine, patch: SettingsPatch) -> Result<Settings, EngineError> {
        engine.set_settings(patch, &|_| {})
    }

    fn request(server: &TestServer, dir: &Path, path: &str) -> AddRequest {
        AddRequest {
            url: Url::parse(&server.url(path)).unwrap(),
            headers: vec![],
            directory: dir.to_owned(),
            filename: None,
            paused: false,
            connections: None,
            queue: None,
            speed_limit: None,
            target_fs: TargetFs::Posix,
        }
    }

    fn everything() -> SettingsPatch {
        SettingsPatch {
            global_speed_limit: Some(LimitPatch::Rate(300_000)),
            headroom_percent: Some(15),
            global_connections: Some(6),
            per_host_connections: Some(2),
            min_connections_for_new: Some(0),
            default_connections: Some(3),
            max_connections_per_download: Some(12),
            first_byte_timeout_ms: Some(5_000),
            idle_timeout_ms: Some(6_000),
            max_retries: Some(4),
            commit_interval_ms: Some(500),
            probe_range_get: Some(false),
            use_content_disposition: Some(false),
            append_type_extension: Some(false),
            auto_grouping: Some(false),
            extra_extensions: vec![CategoryExtensions {
                category: Category::Data,
                extensions: vec![".QQQ".into(), "mkv".into()],
            }],
            proxy_mode: Some(ProxyMode::None),
        }
    }

    #[test]
    fn defaults_are_the_specs() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(dir.path()).settings().unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(
            (
                s.global_speed_limit,
                s.headroom_percent,
                s.global_connections,
                s.per_host_connections,
                s.min_connections_for_new,
                s.default_connections,
                s.max_connections_per_download,
            ),
            (None, 0, 16, 4, 1, 8, 16)
        );
        assert_eq!(
            (
                s.first_byte_timeout_ms,
                s.idle_timeout_ms,
                s.max_retries,
                s.commit_interval_ms,
            ),
            (30_000, 30_000, 10, 2_000)
        );
        assert!(
            s.probe_range_get
                && s.use_content_disposition
                && s.append_type_extension
                && s.auto_grouping
        );
        assert!(s.extra_extensions.is_empty());
        assert_eq!(s.proxy_mode, ProxyMode::System);
    }

    #[test]
    fn a_patch_round_trips_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let events = Mutex::new(Vec::new());
        let s = engine
            .set_settings(everything(), &|e| events.lock().unwrap().push(e))
            .unwrap();
        let expected = Settings {
            global_speed_limit: Some(300_000),
            headroom_percent: 15,
            global_connections: 6,
            per_host_connections: 2,
            min_connections_for_new: 0,
            default_connections: 3,
            max_connections_per_download: 12,
            first_byte_timeout_ms: 5_000,
            idle_timeout_ms: 6_000,
            max_retries: 4,
            commit_interval_ms: 500,
            probe_range_get: false,
            use_content_disposition: false,
            append_type_extension: false,
            auto_grouping: false,
            extra_extensions: [(Category::Data, vec!["mkv".into(), "qqq".into()])].into(),
            proxy_mode: ProxyMode::None,
        };
        assert_eq!(s, expected);
        assert_eq!(engine.settings().unwrap(), expected);
        assert_eq!(
            *events.lock().unwrap(),
            [EngineEvent::SettingsChanged {
                settings: expected.clone()
            }]
        );
        // An empty patch changes nothing and says nothing.
        let quiet = engine
            .set_settings(SettingsPatch::default(), &|_| panic!("event"))
            .unwrap();
        assert_eq!(quiet, expected);
        drop(engine);
        assert_eq!(open(dir.path()).settings().unwrap(), expected);
    }

    #[test]
    fn one_bad_field_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let bad_proxy = ProxyMode::Custom(Box::default());
        for bad in [
            SettingsPatch {
                headroom_percent: Some(100),
                ..everything()
            },
            SettingsPatch {
                global_connections: Some(0),
                ..everything()
            },
            SettingsPatch {
                per_host_connections: Some(0),
                ..everything()
            },
            SettingsPatch {
                min_connections_for_new: Some(2),
                ..everything()
            },
            SettingsPatch {
                default_connections: Some(0),
                ..everything()
            },
            // Above the maximum it sets, or the stored one.
            SettingsPatch {
                default_connections: Some(13),
                ..everything()
            },
            SettingsPatch {
                default_connections: Some(17),
                ..SettingsPatch::default()
            },
            SettingsPatch {
                max_connections_per_download: Some(0),
                ..everything()
            },
            SettingsPatch {
                max_connections_per_download: Some(2),
                ..SettingsPatch::default()
            },
            SettingsPatch {
                first_byte_timeout_ms: Some(50),
                ..everything()
            },
            SettingsPatch {
                idle_timeout_ms: Some(3_600_001),
                ..everything()
            },
            SettingsPatch {
                max_retries: Some(0),
                ..everything()
            },
            SettingsPatch {
                commit_interval_ms: Some(60_001),
                ..everything()
            },
            SettingsPatch {
                proxy_mode: Some(bad_proxy.clone()),
                ..everything()
            },
            SettingsPatch {
                extra_extensions: vec![CategoryExtensions {
                    category: Category::Video,
                    extensions: vec!["a/b".into()],
                }],
                ..everything()
            },
        ] {
            let r = engine.set_settings(bad.clone(), &|_| panic!("event"));
            assert!(matches!(r, Err(EngineError::InvalidSetting(_))), "{bad:?}");
            assert_eq!(engine.settings().unwrap(), Settings::default(), "{bad:?}");
        }
        assert_eq!(engine.shared.global.rate(), None);
    }

    #[test]
    fn leave_is_not_clear() {
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let limit = |l| SettingsPatch {
            global_speed_limit: l,
            ..SettingsPatch::default()
        };
        set(&engine, limit(Some(LimitPatch::Rate(1000)))).unwrap();
        let s = set(
            &engine,
            SettingsPatch {
                headroom_percent: Some(5),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        assert_eq!(s.global_speed_limit, Some(1000), "left alone");
        let s = set(&engine, limit(Some(LimitPatch::Unlimited))).unwrap();
        assert_eq!((s.global_speed_limit, s.headroom_percent), (None, 5));
        assert_eq!(engine.shared.global.rate(), None);
    }

    /// Rows as M2.5–M3.4 wrote them still read; rows of unknown keys and
    /// unreadable values don't stop the rest.
    #[test]
    fn stored_rows_old_unknown_and_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        for (key, value) in [
            ("global_speed_limit", "123456"),
            ("headroom_percent", "12"),
            ("extra_extensions", r#"{"data":["qqq"]}"#),
            (
                "proxy_mode",
                r#"{"custom":{"http":"http://p.test:3128/","https":null,"all":null,"no_proxy":["lan"]}}"#,
            ),
            ("from_the_future", r#"{"x":1}"#),
            ("global_connections", "\"many\""),
            ("per_host_connections", "0"),
        ] {
            engine
                .db()
                .conn()
                .execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                    params![key, value],
                )
                .unwrap();
        }
        let s = engine.settings().unwrap();
        assert_eq!(s.global_speed_limit, Some(123_456));
        assert_eq!(s.headroom_percent, 12);
        assert_eq!(s.extra_extensions[&Category::Data], ["qqq"]);
        let ProxyMode::Custom(c) = &s.proxy_mode else {
            panic!("{:?}", s.proxy_mode)
        };
        assert_eq!(c.http.as_ref().unwrap().as_str(), "http://p.test:3128/");
        assert_eq!(c.no_proxy, ["lan"]);
        assert_eq!((s.global_connections, s.per_host_connections), (16, 4));
        // A patch keeps the rows it doesn't touch, unknown ones included.
        set(
            &engine,
            SettingsPatch {
                global_connections: Some(8),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let future: String = engine
            .db()
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = 'from_the_future'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(future, r#"{"x":1}"#);
        assert_eq!(engine.settings().unwrap().global_connections, 8);
    }

    /// Lifting the global limit applies to a download already running.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_limit_change_reaches_a_running_download() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let limit = |l| SettingsPatch {
            global_speed_limit: Some(l),
            ..SettingsPatch::default()
        };
        set(&engine, limit(LimitPatch::Rate(50_000))).unwrap();
        let added = engine
            .add(request(&server, dir.path(), "/blob/2000000"), &|_| {})
            .await
            .unwrap();
        let (flows, mut flowing) = watch::channel(false);
        let on_event = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
                flows.send_replace(true);
            }
        };
        let (_keep, stop) = watch::channel(false);
        let started = Instant::now();
        let lift = async {
            flowing.wait_for(|f| *f).await.unwrap();
            set(&engine, limit(LimitPatch::Unlimited)).unwrap();
        };
        let (snap, ()) = tokio::join!(engine.run(&added.id, &on_event, &stop), lift);
        assert_eq!(snap.unwrap().state, State::Completed);
        // 40 s at the old limit.
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    /// The budget is read when an engine opens: a reopened one has one
    /// slot per host, so the second download waits for the first.
    #[tokio::test(flavor = "multi_thread")]
    async fn budget_settings_apply_to_the_next_engine() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        set(
            &open(dir.path()),
            SettingsPatch {
                per_host_connections: Some(1),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let engine = open(dir.path());
        // One connection: under the default cap of 4, b would not wait.
        let mut req = request(&server, dir.path(), "/blob/400000?seed=1&throttle=400000");
        req.connections = Some(1);
        let a = engine.add(req, &|_| {}).await.unwrap();
        let b = engine
            .add(request(&server, dir.path(), "/blob/400000?seed=2"), &|_| {})
            .await
            .unwrap();
        let (a_flows, mut flowing) = watch::channel(false);
        let on_a = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { downloaded, .. } if downloaded > 0) {
                a_flows.send_replace(true);
            }
        };
        let waited = Arc::new(Mutex::new(false));
        let (_keep, stop) = watch::channel(false);
        let second = async {
            flowing.wait_for(|f| *f).await.unwrap();
            let (flag, b_id) = (waited.clone(), b.id.clone());
            let on_b = move |e: EngineEvent| {
                if matches!(&e, EngineEvent::StateChanged { id, to: State::Waiting, .. } if *id == b_id)
                {
                    *flag.lock().unwrap() = true;
                }
            };
            engine.run(&b.id, &on_b, &stop).await
        };
        let (ra, rb) = tokio::join!(engine.run(&a.id, &on_a, &stop), second);
        assert_eq!(ra.unwrap().state, State::Completed);
        assert_eq!(rb.unwrap().state, State::Completed);
        assert!(*waited.lock().unwrap(), "b waited for the host's one slot");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_range_get_applies_to_the_next_add() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let path = "/blob/5000?ranges=hidden";
        let on = engine
            .add(request(&server, dir.path(), path), &|_| {})
            .await
            .unwrap();
        assert_eq!(on.resumable, Resumable::Yes);
        set(
            &engine,
            SettingsPatch {
                probe_range_get: Some(false),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let before = server.requests().len();
        let off = engine
            .add(request(&server, dir.path(), path), &|_| {})
            .await
            .unwrap();
        assert_eq!(off.resumable, Resumable::Unknown);
        let methods: Vec<_> = server.requests()[before..]
            .iter()
            .map(|r| r.method.clone())
            .collect();
        assert_eq!(methods, ["HEAD"]);
    }

    /// Ramp-up that never saturates and steps every ~130 ms, so a download
    /// reaches whatever cap applies within a second.
    fn ramping() -> DownloadConfig {
        let mut c = DownloadConfig::default();
        c.ramp.window = Duration::from_millis(100);
        c.ramp.min_gain = -1.0;
        c.ramp.min_chunk = 128 * 1024;
        c.ramp.min_steal_gain = Duration::from_millis(200);
        c.transfer.retry.base = Duration::from_millis(1);
        c.transfer.retry.cap = Duration::from_millis(5);
        c
    }

    fn open_with(dir: &Path, config: DownloadConfig) -> Engine {
        Engine::open(&dir.join("data"), config).unwrap()
    }

    /// Runs `req` to the end; the most connections it had at once.
    async fn peak_connections(engine: &Engine, req: AddRequest) -> u32 {
        let id = engine.add(req, &|_| {}).await.unwrap().id;
        let peak = Arc::new(Mutex::new(0));
        let p = peak.clone();
        let on_event = move |e: EngineEvent| {
            if let EngineEvent::Progress { connections, .. } = e {
                let mut p = p.lock().unwrap();
                *p = (*p).max(connections);
            }
        };
        let (_keep, stop) = watch::channel(false);
        let snap = engine.run(&id, &on_event, &stop).await.unwrap();
        assert_eq!(snap.state, State::Completed);
        *peak.lock().unwrap()
    }

    #[test]
    fn run_config_carries_every_per_download_setting() {
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        set(&engine, everything()).unwrap();
        let c = engine.run_config().unwrap();
        assert_eq!((c.ramp.connections, c.ramp.max_connections), (3, 12));
        assert_eq!(
            (c.transfer.first_byte, c.transfer.idle),
            (Duration::from_secs(5), Duration::from_secs(6))
        );
        assert_eq!(c.transfer.retry.max_failures, 4);
        assert_eq!(c.writer.commit_interval, Duration::from_millis(500));
        assert!(!c.probe.range_get);
    }

    /// A download that doesn't ask uses `default_connections`; none uses
    /// more than `max_connections_per_download`, asked or not.
    #[tokio::test(flavor = "multi_thread")]
    async fn connection_settings_cap_downloads() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open_with(dir.path(), ramping());
        let path = |seed| format!("/blob/4500000?seed={seed}&throttle=500000");
        // Unset, it ramps to the per-host cap.
        let free = peak_connections(&engine, request(&server, dir.path(), &path(1))).await;
        assert_eq!(free, 4);
        set(
            &engine,
            SettingsPatch {
                default_connections: Some(2),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let default = peak_connections(&engine, request(&server, dir.path(), &path(2))).await;
        assert_eq!(default, 2);
        set(
            &engine,
            SettingsPatch {
                default_connections: Some(1),
                max_connections_per_download: Some(1),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let mut asked = request(&server, dir.path(), "/blob/500000?seed=3&throttle=500000");
        asked.connections = Some(4);
        assert_eq!(peak_connections(&engine, asked).await, 1);
    }

    /// `a` runs with `a_conns`; once it has them, `b` starts. When `b`
    /// waits for the budget, `a` keeps running for a while, then is
    /// stopped. Whether `b` got a byte before that.
    async fn second_download(engine: &Engine, a: AddRequest, b: AddRequest, a_conns: u32) -> bool {
        let a = engine.add(a, &|_| {}).await.unwrap();
        let b = engine.add(b, &|_| {}).await.unwrap();
        let (a_ready, mut ready) = watch::channel(false);
        let on_a = move |e: EngineEvent| {
            if matches!(e, EngineEvent::Progress { connections, .. } if connections >= a_conns) {
                a_ready.send_replace(true);
            }
        };
        let (b_waits, mut waiting) = watch::channel(false);
        let a_stopped = Arc::new(Mutex::new(false));
        let early = Arc::new(Mutex::new(false));
        let (stop_a, a_stop) = watch::channel(false);
        let (_keep, stop) = watch::channel(false);
        let second = async {
            ready.wait_for(|r| *r).await.unwrap();
            let (stopped, early) = (a_stopped.clone(), early.clone());
            let on_b = move |e: EngineEvent| match e {
                EngineEvent::StateChanged {
                    to: State::Waiting, ..
                } => {
                    b_waits.send_replace(true);
                }
                EngineEvent::Progress { downloaded, .. }
                    if downloaded > 0 && !*stopped.lock().unwrap() =>
                {
                    *early.lock().unwrap() = true;
                }
                _ => {}
            };
            engine.run(&b.id, &on_b, &stop).await
        };
        let stopper = async {
            waiting.wait_for(|w| *w).await.unwrap();
            // Long enough for a slot to be given up, were it allowed.
            tokio::time::sleep(Duration::from_millis(500)).await;
            *a_stopped.lock().unwrap() = true;
            stop_a.send_replace(true);
        };
        let (ra, rb, ()) = tokio::join!(engine.run(&a.id, &on_a, &a_stop), second, stopper);
        assert_eq!(ra.unwrap().state, State::Paused);
        assert_eq!(rb.unwrap().state, State::Completed);
        *early.lock().unwrap()
    }

    /// Read when an engine opens: one connection in all, so `b`, on
    /// another server, waits for `a`'s (with the default 16 it wouldn't).
    #[tokio::test(flavor = "multi_thread")]
    async fn global_connections_apply_to_the_next_engine() {
        let (one, two) = (TestServer::start().unwrap(), TestServer::start().unwrap());
        let dir = tempfile::tempdir().unwrap();
        set(
            &open(dir.path()),
            SettingsPatch {
                global_connections: Some(1),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let engine = open(dir.path());
        let mut a = request(&one, dir.path(), "/blob/20000000?seed=1&throttle=300000");
        a.connections = Some(1);
        let b = request(&two, dir.path(), "/blob/40000?seed=2");
        assert!(!second_download(&engine, a, b, 1).await, "b waited for a");
    }

    /// With `min_connections_for_new` 0 a newcomer is not given one of
    /// `a`'s two connections (with 1 it would be): it starts once `a`
    /// stops.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_newcomer_minimum_applies_to_the_next_engine() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        set(
            &open(dir.path()),
            SettingsPatch {
                global_connections: Some(2),
                min_connections_for_new: Some(0),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let engine = open_with(dir.path(), ramping());
        let mut a = request(&server, dir.path(), "/blob/20000000?seed=1&throttle=300000");
        a.connections = Some(2);
        let b = request(&server, dir.path(), "/blob/40000?seed=2");
        assert!(
            !second_download(&engine, a, b, 2).await,
            "b got none of a's"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_grouping_off_leaves_downloads_ungrouped() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        set(
            &engine,
            SettingsPatch {
                auto_grouping: Some(false),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        // T-13's first two.
        for name in ["Movie.part1.rar", "Movie.part2.rar"] {
            let mut req = request(&server, dir.path(), "/blob/10");
            req.filename = Some(name.into());
            assert_eq!(engine.add(req, &|_| {}).await.unwrap().group, None);
        }
        assert!(engine.list_groups().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn naming_toggles_hide_the_servers_hints() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open(dir.path());
        let named = "/blob/10?header=Content-Disposition%3A%20attachment%3B%20filename%3Dx.qqq";
        let typed = "/blob/20?header=Content-Type%3A%20audio%2Fmpeg";
        let name = |s: crate::engine::Snapshot| {
            s.final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let add = |path| engine.add(request(&server, dir.path(), path), &|_| {});
        assert_eq!(name(add(named).await.unwrap()), "x.qqq");
        assert_eq!(name(add(typed).await.unwrap()), "20.mp3");
        set(
            &engine,
            SettingsPatch {
                use_content_disposition: Some(false),
                append_type_extension: Some(false),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        assert!(!name(add(named).await.unwrap()).starts_with('x'));
        assert_eq!(name(add(typed).await.unwrap()), "20");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retries_and_timeouts_apply_to_the_next_run() {
        let server = TestServer::start().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = open_with(dir.path(), ramping());
        set(
            &engine,
            SettingsPatch {
                max_retries: Some(2),
                idle_timeout_ms: Some(200),
                ..SettingsPatch::default()
            },
        )
        .unwrap();
        let (_keep, stop) = watch::channel(false);
        let mut failing = request(&server, dir.path(), "/blob/1000?fail=100");
        failing.connections = Some(1);
        let id = engine.add(failing, &|_| {}).await.unwrap().id;
        let snap = engine.run(&id, &|_| {}, &stop).await.unwrap();
        assert_eq!(snap.state, State::Failed);
        let gets = server
            .requests()
            .iter()
            .filter(|r| r.method == "GET")
            .count();
        assert_eq!(gets, 2, "the 2nd failure gives up");

        // Silent after the headers: each attempt ends at the idle timeout,
        // not the default 30 s.
        let mut silent = request(&server, dir.path(), "/blob/1000?stall_after=0");
        silent.connections = Some(1);
        let id = engine.add(silent, &|_| {}).await.unwrap().id;
        let started = Instant::now();
        let snap = engine.run(&id, &|_| {}, &stop).await.unwrap();
        assert_eq!(snap.state, State::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}
