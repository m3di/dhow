//! Settings (ARCHITECTURE §3.1, §5.1): what the user can change, one
//! `settings` row per key, changed by a [`SettingsPatch`] that applies
//! whole or not at all. A setting without a row, or with one this version
//! can't read, has its default: the engine's [`DownloadConfig`]. Rows of
//! keys this version doesn't know (a newer one wrote them) are left alone.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;

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
const PROBE_RANGE_GET: &str = "probe_range_get";
const EXTRA_EXTENSIONS: &str = "extra_extensions";
const PROXY_MODE: &str = "proxy_mode";

/// Every setting, as in effect. Speed limits apply to running downloads at
/// once (in another process at its next [`Engine::refresh_limits`]); the
/// rest from the next download, probe or lease, except the connection
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
    /// Connections of a download that doesn't ask for its own number (§4.1).
    pub default_connections: u8,
    /// Probe with a one-byte `GET` when `HEAD` doesn't tell (§2).
    pub probe_range_get: bool,
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
    pub probe_range_get: Option<bool>,
    /// Applied in order.
    pub extra_extensions: Vec<CategoryExtensions>,
    pub proxy_mode: Option<ProxyMode>,
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
            probe_range_get: config.probe.range_get,
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
        // Each as validated on the way in, so a hand-edited row can't
        // stall the budget.
        if let Some(v) = read(&rows, HEADROOM_PERCENT).filter(|&v| v <= 99) {
            s.headroom_percent = v;
        }
        if let Some(v) = read(&rows, GLOBAL_CONNECTIONS).filter(|&v| v >= 1) {
            s.global_connections = v;
        }
        if let Some(v) = read(&rows, PER_HOST_CONNECTIONS).filter(|&v| v >= 1) {
            s.per_host_connections = v;
        }
        if let Some(v) = read(&rows, MIN_CONNECTIONS_FOR_NEW).filter(|&v| v <= 1) {
            s.min_connections_for_new = v;
        }
        if let Some(v) = read(&rows, DEFAULT_CONNECTIONS)
            .filter(|&v| (1..=config.ramp.max_connections).contains(&v))
        {
            s.default_connections = v;
        }
        if let Some(v) = read(&rows, PROBE_RANGE_GET) {
            s.probe_range_get = v;
        }
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
}

/// `key`'s value; `None` without a row or with one that doesn't parse.
fn read<T: DeserializeOwned>(rows: &HashMap<String, String>, key: &str) -> Option<T> {
    serde_json::from_str(rows.get(key)?)
        .inspect_err(|e| tracing::warn!(key, error = %e, "ignoring unreadable setting"))
        .ok()
}

fn put(conn: &Connection, key: &str, value: &impl serde::Serialize) -> Result<(), EngineError> {
    let json = serde_json::to_string(value).expect("settings serialize");
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, json],
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

    /// Checks every field of `patch`, then stores them in one transaction:
    /// with any field invalid, nothing changes. Emits `SettingsChanged`
    /// unless the patch is empty.
    pub fn set_settings(
        &self,
        patch: SettingsPatch,
        on_event: &(dyn Fn(EngineEvent) + Send + Sync),
    ) -> Result<Settings, EngineError> {
        if let Some(p) = patch.headroom_percent {
            invalid(p <= 99, "headroom is 0–99 %")?;
        }
        if let Some(n) = patch.global_connections {
            invalid(n >= 1, "global connections must be at least 1")?;
        }
        if let Some(n) = patch.per_host_connections {
            invalid(n >= 1, "per-host connections must be at least 1")?;
        }
        if let Some(n) = patch.min_connections_for_new {
            invalid(n <= 1, "connections for a newcomer are 0 or 1")?;
        }
        if let Some(n) = patch.default_connections {
            invalid(
                (1..=self.config.ramp.max_connections).contains(&n),
                "default connections are 1 to the per-download maximum",
            )?;
        }
        let proxy = patch.proxy_mode.as_ref().map(Stored::new).transpose()?;
        let extensions = patch
            .extra_extensions
            .iter()
            .map(|c| {
                let mut exts = c
                    .extensions
                    .iter()
                    .map(|e| normalize_extension(e))
                    .collect::<Option<Vec<_>>>()
                    .ok_or(EngineError::InvalidSetting("not a file extension"))?;
                exts.sort();
                exts.dedup();
                Ok((c.category, exts))
            })
            .collect::<Result<Vec<_>, EngineError>>()?;

        {
            let db = self.db();
            let tx = db.conn().unchecked_transaction()?;
            if let Some(limit) = patch.global_speed_limit {
                let rate = match limit {
                    LimitPatch::Unlimited => None,
                    LimitPatch::Rate(r) => Some(r),
                };
                put(&tx, GLOBAL_SPEED_LIMIT, &rate)?;
            }
            for (key, value) in [
                (HEADROOM_PERCENT, patch.headroom_percent.map(u32::from)),
                (GLOBAL_CONNECTIONS, patch.global_connections),
                (PER_HOST_CONNECTIONS, patch.per_host_connections),
                (MIN_CONNECTIONS_FOR_NEW, patch.min_connections_for_new),
                (
                    DEFAULT_CONNECTIONS,
                    patch.default_connections.map(u32::from),
                ),
            ] {
                if let Some(v) = value {
                    put(&tx, key, &v)?;
                }
            }
            if let Some(v) = patch.probe_range_get {
                put(&tx, PROBE_RANGE_GET, &v)?;
            }
            if !extensions.is_empty() {
                let mut extra = Settings::load(&tx, &self.config)?.extra_extensions;
                for (category, exts) in extensions {
                    for list in extra.values_mut() {
                        list.retain(|e| !exts.contains(e));
                    }
                    extra.insert(category, exts);
                }
                extra.retain(|_, list| !list.is_empty());
                put(&tx, EXTRA_EXTENSIONS, &extra)?;
            }
            if let Some(stored) = proxy {
                put(&tx, PROXY_MODE, &stored)?;
            }
            tx.commit()?;
        }
        if patch.global_speed_limit.is_some() || patch.headroom_percent.is_some() {
            self.refresh_limits()?;
        }
        let settings = self.settings()?;
        if patch != SettingsPatch::default() {
            on_event(EngineEvent::SettingsChanged {
                settings: settings.clone(),
            });
        }
        Ok(settings)
    }

    /// The engine's config with the settings that apply per download.
    pub(super) fn run_config(&self) -> Result<DownloadConfig, EngineError> {
        let s = self.settings()?;
        let mut config = self.config;
        config.ramp.connections = s.default_connections;
        config.probe.range_get = s.probe_range_get;
        Ok(config)
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
            probe_range_get: Some(false),
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
                s.probe_range_get,
            ),
            (None, 0, 16, 4, 1, 8, true)
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
            probe_range_get: false,
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
                default_connections: Some(17),
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
}
