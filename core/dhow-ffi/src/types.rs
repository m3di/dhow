//! The core's types as plain FFI records and enums (ARCHITECTURE §3, §9):
//! ids, paths and URLs are strings, durations milliseconds, times Unix ms.
//! Names that SwiftUI also uses (`State`, `Settings`, `Window`) are
//! prefixed.

use std::path::PathBuf;
use std::time::Duration;

use dhow_core::download::{DownloadConfig, ResetReason as CoreReset};
use dhow_core::engine::categories::CategoryUsage as CoreUsage;
use dhow_core::engine::groups::GroupSnapshot as CoreGroup;
use dhow_core::engine::queues::{
    OnComplete as CoreOnComplete, QueueSnapshot as CoreQueue, QueueSpec as CoreSpec,
    RunState as CoreRunState,
};
use dhow_core::engine::settings::{
    CategoryExtensions as CoreExtensions, LimitPatch as CoreLimit, Settings as CoreSettings,
    SettingsPatch as CorePatch,
};
use dhow_core::engine::speed::SpeedSample as CoreSample;
use dhow_core::engine::{
    self as core, EngineEvent, FailReason as CoreFail, Snapshot, State as CoreState,
    WarningKind as CoreWarning,
};
use dhow_core::naming::{Category as CoreCategory, TargetFs as CoreFs};
use dhow_core::platform::{
    BackgroundCapability as CoreBackground, InterfaceKind as CoreInterface,
    NetworkInfo as CoreNetwork, ProxyConfig as CoreProxy, ProxyMode as CoreMode,
};
use dhow_core::probe::{ProbeError, Resumable as CoreResumable};
use dhow_core::schedule::{
    Action as CoreAction, Clock, Schedule as CoreSchedule, Weekdays, Window as CoreWindow,
};
use reqwest::Url;

use crate::error::DhowError;

/// Engine configuration (ARCHITECTURE §3.1): the ENGINE-SPEC knobs that
/// are not user settings. `None` keeps the spec's default.
#[derive(Clone, Debug, Default, uniffi::Record)]
pub struct EngineConfig {
    /// `settings.rampup_window` (§4.4).
    #[uniffi(default)]
    pub rampup_window_ms: Option<u64>,
    /// `settings.rampup_min_gain`, percent (§4.4).
    #[uniffi(default)]
    pub rampup_min_gain_percent: Option<u32>,
    /// `settings.min_steal_gain` (§4.3).
    #[uniffi(default)]
    pub min_steal_gain_ms: Option<u64>,
    /// `settings.saturation_ttl` (§4.4).
    #[uniffi(default)]
    pub saturation_ttl_ms: Option<u64>,
    /// `settings.min_chunk` (§4.1).
    #[uniffi(default)]
    pub min_chunk_bytes: Option<u64>,
}

impl EngineConfig {
    pub(crate) fn download_config(&self) -> DownloadConfig {
        let mut c = DownloadConfig::default();
        if let Some(ms) = self.rampup_window_ms {
            c.ramp.window = Duration::from_millis(ms.max(1));
        }
        if let Some(p) = self.rampup_min_gain_percent {
            c.ramp.min_gain = f64::from(p) / 100.0;
        }
        if let Some(ms) = self.min_steal_gain_ms {
            c.ramp.min_steal_gain = Duration::from_millis(ms);
        }
        if let Some(ms) = self.saturation_ttl_ms {
            c.ramp.saturation_ttl = Duration::from_millis(ms);
        }
        if let Some(b) = self.min_chunk_bytes {
            c.ramp.min_chunk = b.max(1);
        }
        c
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// How an added download starts (ARCHITECTURE §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum StartMode {
    /// By hand: now, whatever its queue's schedule and concurrency (§11).
    Now,
    /// When its queue lets it.
    Queued,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TargetFs {
    Posix,
    /// Also FAT/exFAT/SMB volumes on any OS.
    Windows,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct AddRequest {
    pub url: String,
    /// Cookies, auth, referer: opaque to the core, never logged.
    #[uniffi(default)]
    pub headers: Vec<Header>,
    /// Must exist; the shell resolves platform defaults.
    pub directory: String,
    /// User override; else the probe's suggestion (§3).
    #[uniffi(default)]
    pub filename: Option<String>,
    pub start: StartMode,
    /// `None`: the default (§4.1).
    #[uniffi(default)]
    pub connections: Option<u8>,
    /// `None`: the default queue.
    #[uniffi(default)]
    pub queue: Option<String>,
    /// Bytes/s; `None`: only the queue's and global limits apply (§12).
    #[uniffi(default)]
    pub speed_limit: Option<u64>,
    /// Naming rules of the target volume; `None`: the host's.
    #[uniffi(default)]
    pub target_fs: Option<TargetFs>,
}

impl AddRequest {
    pub(crate) fn into_core(self) -> Result<core::AddRequest, DhowError> {
        Ok(core::AddRequest {
            url: parse_url(&self.url)?,
            headers: self
                .headers
                .into_iter()
                .map(|h| dhow_core::http::Header {
                    name: h.name,
                    value: h.value,
                })
                .collect(),
            directory: PathBuf::from(self.directory),
            filename: self.filename,
            paused: self.start == StartMode::Paused,
            connections: self.connections,
            queue: self.queue,
            speed_limit: self.speed_limit,
            target_fs: match self.target_fs {
                Some(TargetFs::Posix) => CoreFs::Posix,
                Some(TargetFs::Windows) => CoreFs::Windows,
                None => CoreFs::host(),
            },
        })
    }
}

pub(crate) fn parse_url(s: &str) -> Result<Url, DhowError> {
    Url::parse(s).map_err(|e| DhowError::InvalidArgument {
        message: format!("bad URL: {e}"),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DownloadState {
    Queued,
    /// Waiting for the connection budget (§10.3).
    Waiting,
    Active,
    Paused,
    Completing,
    Completed,
    Failed,
    Cancelled,
}

impl From<CoreState> for DownloadState {
    fn from(s: CoreState) -> Self {
        match s {
            CoreState::Queued => Self::Queued,
            CoreState::Waiting => Self::Waiting,
            CoreState::Active => Self::Active,
            CoreState::Paused => Self::Paused,
            CoreState::Completing => Self::Completing,
            CoreState::Completed => Self::Completed,
            CoreState::Failed => Self::Failed,
            CoreState::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FailReason {
    /// Network trouble outlasted the retries (§9); resuming continues.
    Transient,
    Http {
        status: u16,
    },
    TooManyRedirects,
    InvalidRequest,
    NoSpace,
    DigestMismatch,
    ServerSentExtraData,
    ServerKeptChanging,
    Io,
}

impl From<CoreFail> for FailReason {
    fn from(r: CoreFail) -> Self {
        match r {
            CoreFail::Transient => Self::Transient,
            CoreFail::Http(status) => Self::Http { status },
            CoreFail::TooManyRedirects => Self::TooManyRedirects,
            CoreFail::InvalidRequest => Self::InvalidRequest,
            CoreFail::NoSpace => Self::NoSpace,
            CoreFail::DigestMismatch => Self::DigestMismatch,
            CoreFail::ServerSentExtraData => Self::ServerSentExtraData,
            CoreFail::ServerKeptChanging => Self::ServerKeptChanging,
            CoreFail::Io => Self::Io,
        }
    }
}

/// Why `add` could not probe (§2).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProbeFailure {
    UnsupportedScheme,
    InvalidHeader,
    Status { status: u16 },
    TooManyRedirects,
    Timeout,
    Connect,
    Network,
}

impl From<ProbeError> for ProbeFailure {
    fn from(e: ProbeError) -> Self {
        match e {
            ProbeError::UnsupportedScheme(_) => Self::UnsupportedScheme,
            ProbeError::InvalidHeader(_) => Self::InvalidHeader,
            ProbeError::Status(status) => Self::Status { status },
            ProbeError::TooManyRedirects => Self::TooManyRedirects,
            ProbeError::Timeout => Self::Timeout,
            ProbeError::Connect => Self::Connect,
            ProbeError::Network => Self::Network,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum WarningKind {
    NotResumable,
    SizeUnverified,
    IdentityWeak,
    ServerIgnoredRange,
}

impl From<CoreWarning> for WarningKind {
    fn from(w: CoreWarning) -> Self {
        match w {
            CoreWarning::NotResumable => Self::NotResumable,
            CoreWarning::SizeUnverified => Self::SizeUnverified,
            CoreWarning::IdentityWeak => Self::IdentityWeak,
            CoreWarning::ServerIgnoredRange => Self::ServerIgnoredRange,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ResetReason {
    /// I4/I5: the server has a different file now.
    ServerFileChanged,
    /// A part file without a usable sidecar, or the reverse (§5.4).
    Unaccountable,
    /// Bytes are on disk but the server can't continue from them.
    CannotResume,
}

impl From<CoreReset> for ResetReason {
    fn from(r: CoreReset) -> Self {
        match r {
            CoreReset::ServerFileChanged => Self::ServerFileChanged,
            CoreReset::Unaccountable => Self::Unaccountable,
            CoreReset::CannotResume => Self::CannotResume,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Resumable {
    Yes,
    No,
    Unknown,
}

impl From<CoreResumable> for Resumable {
    fn from(r: CoreResumable) -> Self {
        match r {
            CoreResumable::Yes => Self::Yes,
            CoreResumable::No => Self::No,
            CoreResumable::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Category {
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Software,
    Code,
    Data,
    Other,
}

impl From<CoreCategory> for Category {
    fn from(c: CoreCategory) -> Self {
        match c {
            CoreCategory::Video => Self::Video,
            CoreCategory::Audio => Self::Audio,
            CoreCategory::Image => Self::Image,
            CoreCategory::Document => Self::Document,
            CoreCategory::Archive => Self::Archive,
            CoreCategory::Software => Self::Software,
            CoreCategory::Code => Self::Code,
            CoreCategory::Data => Self::Data,
            CoreCategory::Other => Self::Other,
        }
    }
}

impl From<Category> for CoreCategory {
    fn from(c: Category) -> Self {
        match c {
            Category::Video => Self::Video,
            Category::Audio => Self::Audio,
            Category::Image => Self::Image,
            Category::Document => Self::Document,
            Category::Archive => Self::Archive,
            Category::Software => Self::Software,
            Category::Code => Self::Code,
            Category::Data => Self::Data,
            Category::Other => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DownloadSnapshot {
    pub id: String,
    pub url: String,
    pub final_path: String,
    pub part_path: String,
    pub state: DownloadState,
    pub fail_reason: Option<FailReason>,
    pub total_size: Option<u64>,
    /// Committed bytes (the whole file once completed).
    pub downloaded: u64,
    pub resumable: Resumable,
    pub size_unverified: bool,
    pub identity_weak: bool,
    /// Unix ms.
    pub created_at: i64,
    pub queue_id: String,
    /// Order within its queue.
    pub position: i64,
    /// Bytes/s; `Some(0)`: paused by limit (§12).
    pub speed_limit: Option<u64>,
    pub category: Category,
    pub group: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Asked for at `add`; `None`: the default.
    pub connections: Option<u8>,
}

impl From<Snapshot> for DownloadSnapshot {
    fn from(s: Snapshot) -> Self {
        Self {
            id: s.id,
            url: s.url,
            final_path: path(s.final_path),
            part_path: path(s.part_path),
            state: s.state.into(),
            fail_reason: s.fail_reason.map(Into::into),
            total_size: s.total_size,
            downloaded: s.downloaded,
            resumable: s.resumable.into(),
            size_unverified: s.size_unverified,
            identity_weak: s.identity_weak,
            created_at: s.created_at,
            queue_id: s.queue_id,
            position: s.position,
            speed_limit: s.speed_limit,
            category: s.category.into(),
            group: s.group,
            etag: s.etag,
            last_modified: s.last_modified,
            connections: s.connections,
        }
    }
}

fn path(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct GroupSnapshot {
    pub id: String,
    pub name: String,
    /// `false` once the user changed it.
    pub auto: bool,
    /// Oldest first.
    pub members: Vec<String>,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
}

impl From<CoreGroup> for GroupSnapshot {
    fn from(g: CoreGroup) -> Self {
        Self {
            id: g.id,
            name: g.name,
            auto: g.auto,
            members: g.members,
            downloaded: g.downloaded,
            total: g.total,
            speed_bps: g.speed_bps,
            eta_secs: g.eta_secs,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

const WEEK: [(Weekday, jiff::civil::Weekday); 7] = [
    (Weekday::Monday, jiff::civil::Weekday::Monday),
    (Weekday::Tuesday, jiff::civil::Weekday::Tuesday),
    (Weekday::Wednesday, jiff::civil::Weekday::Wednesday),
    (Weekday::Thursday, jiff::civil::Weekday::Thursday),
    (Weekday::Friday, jiff::civil::Weekday::Friday),
    (Weekday::Saturday, jiff::civil::Weekday::Saturday),
    (Weekday::Sunday, jiff::civil::Weekday::Sunday),
];

/// Local wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
    #[uniffi(default = 0)]
    pub second: u8,
}

/// A weekly window (§11); ends at or before its start run past midnight.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ScheduleWindow {
    /// The days it starts on.
    pub days: Vec<Weekday>,
    pub start: TimeOfDay,
    pub end: TimeOfDay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ScheduleAction {
    /// Start only inside a window; running downloads continue past its end.
    Start,
    /// Also stop running downloads at the end (back to `Queued`).
    StartAndStopOutside,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Schedule {
    pub windows: Vec<ScheduleWindow>,
    pub action: ScheduleAction,
}

impl From<CoreSchedule> for Schedule {
    fn from(s: CoreSchedule) -> Self {
        let time = |c: Clock| {
            let t = c.time();
            TimeOfDay {
                hour: t.hour() as u8,
                minute: t.minute() as u8,
                second: t.second() as u8,
            }
        };
        Self {
            windows: s
                .windows
                .into_iter()
                .map(|w| ScheduleWindow {
                    days: w
                        .days
                        .days()
                        .into_iter()
                        .filter_map(|d| WEEK.iter().find(|(_, c)| *c == d).map(|(f, _)| *f))
                        .collect(),
                    start: time(w.start),
                    end: time(w.end),
                })
                .collect(),
            action: match s.action {
                CoreAction::Start => ScheduleAction::Start,
                CoreAction::StartAndStopOutside => ScheduleAction::StartAndStopOutside,
            },
        }
    }
}

impl TryFrom<Schedule> for CoreSchedule {
    type Error = DhowError;

    fn try_from(s: Schedule) -> Result<Self, DhowError> {
        let clock = |t: TimeOfDay| {
            let part = |v: u8| i8::try_from(v).unwrap_or(i8::MAX);
            Clock::new(part(t.hour), part(t.minute), part(t.second)).ok_or_else(|| {
                DhowError::InvalidQueue {
                    message: "window times are HH:MM:SS".into(),
                }
            })
        };
        let windows = s
            .windows
            .into_iter()
            .map(|w| {
                let days = Weekdays::from_days(
                    w.days
                        .iter()
                        .filter_map(|d| WEEK.iter().find(|(f, _)| f == d).map(|(_, c)| *c)),
                );
                if days.is_empty() {
                    return Err(DhowError::InvalidQueue {
                        message: "a window needs at least one day".into(),
                    });
                }
                Ok(CoreWindow {
                    days,
                    start: clock(w.start)?,
                    end: clock(w.end)?,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(CoreSchedule {
            windows,
            action: match s.action {
                ScheduleAction::Start => CoreAction::Start,
                ScheduleAction::StartAndStopOutside => CoreAction::StartAndStopOutside,
            },
        })
    }
}

/// What to do when a queue runs dry; the shell decides what's possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum OnComplete {
    Nothing,
    Sleep,
    Shutdown,
    Hibernate,
}

impl From<CoreOnComplete> for OnComplete {
    fn from(o: CoreOnComplete) -> Self {
        match o {
            CoreOnComplete::Nothing => Self::Nothing,
            CoreOnComplete::Sleep => Self::Sleep,
            CoreOnComplete::Shutdown => Self::Shutdown,
            CoreOnComplete::Hibernate => Self::Hibernate,
        }
    }
}

impl From<OnComplete> for CoreOnComplete {
    fn from(o: OnComplete) -> Self {
        match o {
            OnComplete::Nothing => Self::Nothing,
            OnComplete::Sleep => Self::Sleep,
            OnComplete::Shutdown => Self::Shutdown,
            OnComplete::Hibernate => Self::Hibernate,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct QueueSpec {
    pub name: String,
    #[uniffi(default = 3)]
    pub max_concurrent: u32,
    /// Bytes/s for the whole queue (§12).
    #[uniffi(default)]
    pub speed_limit: Option<u64>,
    #[uniffi(default)]
    pub schedule: Option<Schedule>,
    pub on_complete: OnComplete,
}

impl TryFrom<QueueSpec> for CoreSpec {
    type Error = DhowError;

    fn try_from(s: QueueSpec) -> Result<Self, DhowError> {
        Ok(CoreSpec {
            name: s.name,
            max_concurrent: s.max_concurrent,
            speed_limit: s.speed_limit,
            schedule: s.schedule.map(TryInto::try_into).transpose()?,
            on_complete: s.on_complete.into(),
        })
    }
}

/// A `start_queue`/`stop_queue` override of the schedule (§11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RunState {
    /// Follow the schedule (always on without one).
    Auto,
    Started,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct QueueSnapshot {
    pub id: String,
    /// `None` for the default queue until renamed: the shell names it.
    pub name: Option<String>,
    pub max_concurrent: u32,
    pub speed_limit: Option<u64>,
    pub schedule: Option<Schedule>,
    pub on_complete: OnComplete,
    pub is_default: bool,
    pub run_state: RunState,
    /// Unix ms the override lasts until; `None`: until changed.
    pub run_state_until: Option<i64>,
    /// Downloads in it that aren't completed or cancelled.
    pub unfinished: u32,
    /// May its downloads start now (schedule and override, §11)? As of
    /// the read.
    pub active_now: bool,
    /// Unix ms its state can next change (a window edge or the override's
    /// end), in local time with DST; `None`: not by itself.
    pub next_edge: Option<i64>,
}

impl From<CoreQueue> for QueueSnapshot {
    fn from(q: CoreQueue) -> Self {
        let now = jiff::Zoned::now();
        Self {
            active_now: q.active_at(&now),
            next_edge: q.next_edge_at(&now).map(|t| t.as_millisecond()),
            id: q.id,
            name: q.name,
            max_concurrent: q.max_concurrent,
            speed_limit: q.speed_limit,
            schedule: q.schedule.map(Into::into),
            on_complete: q.on_complete.into(),
            is_default: q.is_default,
            run_state: match q.run_state {
                CoreRunState::Auto => RunState::Auto,
                CoreRunState::Started => RunState::Started,
                CoreRunState::Stopped => RunState::Stopped,
            },
            run_state_until: q.run_state_until,
            unfinished: q.unfinished,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CategoryUsage {
    pub category: Category,
    /// Size of those whose file still exists.
    pub bytes: u64,
    pub files: u64,
    /// Downloads whose file is gone; not in `bytes`.
    pub missing: Vec<String>,
}

impl From<CoreUsage> for CategoryUsage {
    fn from(u: CoreUsage) -> Self {
        Self {
            category: u.category.into(),
            bytes: u.bytes,
            files: u.files,
            missing: u.missing,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SpeedSample {
    /// Unix ms the period began.
    pub at: i64,
    /// 1, or 3600 for an hourly mean.
    pub period_secs: u32,
    pub bytes_per_sec: u64,
}

impl From<CoreSample> for SpeedSample {
    fn from(s: CoreSample) -> Self {
        Self {
            at: s.at,
            period_secs: s.period_secs,
            bytes_per_sec: s.bytes_per_sec,
        }
    }
}

/// Proxies by request scheme (ARCHITECTURE §8); all `None` is direct.
/// URLs may carry credentials in their userinfo.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct ProxyConfig {
    #[uniffi(default)]
    pub http: Option<String>,
    #[uniffi(default)]
    pub https: Option<String>,
    /// For any scheme without its own (SOCKS).
    #[uniffi(default)]
    pub all: Option<String>,
    /// Hosts, `.domain`s, CIDRs or `*` that go direct.
    #[uniffi(default)]
    pub no_proxy: Vec<String>,
    /// Reported, not evaluated.
    #[uniffi(default)]
    pub pac: Option<String>,
}

impl From<CoreProxy> for ProxyConfig {
    fn from(p: CoreProxy) -> Self {
        let s = |u: Option<Url>| u.map(String::from);
        Self {
            http: s(p.http),
            https: s(p.https),
            all: s(p.all),
            no_proxy: p.no_proxy,
            pac: s(p.pac),
        }
    }
}

impl TryFrom<ProxyConfig> for CoreProxy {
    type Error = DhowError;

    fn try_from(p: ProxyConfig) -> Result<Self, DhowError> {
        let u = |s: Option<String>| s.as_deref().map(parse_url).transpose();
        Ok(CoreProxy {
            http: u(p.http)?,
            https: u(p.https)?,
            all: u(p.all)?,
            no_proxy: p.no_proxy,
            pac: u(p.pac)?,
        })
    }
}

/// The `proxy_mode` setting (ARCHITECTURE §8).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProxyMode {
    System,
    /// No proxy (the core's `None`, renamed: `.none` is `Optional`'s in
    /// Swift).
    Direct,
    Custom {
        config: ProxyConfig,
    },
}

impl From<CoreMode> for ProxyMode {
    fn from(m: CoreMode) -> Self {
        match m {
            CoreMode::System => Self::System,
            CoreMode::None => Self::Direct,
            CoreMode::Custom(c) => Self::Custom {
                config: (*c).into(),
            },
        }
    }
}

impl TryFrom<ProxyMode> for CoreMode {
    type Error = DhowError;

    fn try_from(m: ProxyMode) -> Result<Self, DhowError> {
        Ok(match m {
            ProxyMode::System => CoreMode::System,
            ProxyMode::Direct => CoreMode::None,
            ProxyMode::Custom { config } => CoreMode::Custom(Box::new(config.try_into()?)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum InterfaceKind {
    Wifi,
    Ethernet,
    Cellular,
    Vpn,
    Unknown,
}

impl From<CoreInterface> for InterfaceKind {
    fn from(k: CoreInterface) -> Self {
        match k {
            CoreInterface::Wifi => Self::Wifi,
            CoreInterface::Ethernet => Self::Ethernet,
            CoreInterface::Cellular => Self::Cellular,
            CoreInterface::Vpn => Self::Vpn,
            CoreInterface::Unknown => Self::Unknown,
        }
    }
}

impl From<InterfaceKind> for CoreInterface {
    fn from(k: InterfaceKind) -> Self {
        match k {
            InterfaceKind::Wifi => Self::Wifi,
            InterfaceKind::Ethernet => Self::Ethernet,
            InterfaceKind::Cellular => Self::Cellular,
            InterfaceKind::Vpn => Self::Vpn,
            InterfaceKind::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NetworkInfo {
    /// The interface of the default route.
    pub interface_kind: InterfaceKind,
    pub vpn_active: bool,
    /// `None`: no system proxy configured.
    pub system_proxy: Option<ProxyConfig>,
    /// What the HTTP client uses (filled in by the engine).
    pub proxy_in_use: ProxyMode,
}

impl From<CoreNetwork> for NetworkInfo {
    fn from(n: CoreNetwork) -> Self {
        Self {
            interface_kind: n.interface_kind.into(),
            vpn_active: n.vpn_active,
            system_proxy: n.system_proxy.map(Into::into),
            proxy_in_use: n.proxy_in_use.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum BackgroundCapability {
    Unrestricted,
    TimeLimited,
    /// Only through system background sessions (iOS); the core's `None`.
    SystemOnly,
}

impl From<BackgroundCapability> for CoreBackground {
    fn from(b: BackgroundCapability) -> Self {
        match b {
            BackgroundCapability::Unrestricted => Self::Unrestricted,
            BackgroundCapability::TimeLimited => Self::TimeLimited,
            BackgroundCapability::SystemOnly => Self::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CategoryExtensions {
    pub category: Category,
    pub extensions: Vec<String>,
}

/// Every user setting as in effect (ARCHITECTURE §3.1, §5.1).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DhowSettings {
    /// Bytes/s; `None`: unlimited (§12).
    pub global_speed_limit: Option<u64>,
    pub headroom_percent: u8,
    pub global_connections: u32,
    pub per_host_connections: u32,
    pub min_connections_for_new: u32,
    pub default_connections: u8,
    pub max_connections_per_download: u8,
    pub first_byte_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_retries: u32,
    pub commit_interval_ms: u64,
    pub probe_range_get: bool,
    pub use_content_disposition: bool,
    pub append_type_extension: bool,
    pub auto_grouping: bool,
    /// Categories with extra extensions, in category order.
    pub extra_extensions: Vec<CategoryExtensions>,
    pub proxy_mode: ProxyMode,
}

impl From<CoreSettings> for DhowSettings {
    fn from(s: CoreSettings) -> Self {
        Self {
            global_speed_limit: s.global_speed_limit,
            headroom_percent: s.headroom_percent,
            global_connections: s.global_connections,
            per_host_connections: s.per_host_connections,
            min_connections_for_new: s.min_connections_for_new,
            default_connections: s.default_connections,
            max_connections_per_download: s.max_connections_per_download,
            first_byte_timeout_ms: s.first_byte_timeout_ms,
            idle_timeout_ms: s.idle_timeout_ms,
            max_retries: s.max_retries,
            commit_interval_ms: s.commit_interval_ms,
            probe_range_get: s.probe_range_get,
            use_content_disposition: s.use_content_disposition,
            append_type_extension: s.append_type_extension,
            auto_grouping: s.auto_grouping,
            extra_extensions: s
                .extra_extensions
                .into_iter()
                .map(|(category, extensions)| CategoryExtensions {
                    category: category.into(),
                    extensions,
                })
                .collect(),
            proxy_mode: s.proxy_mode.into(),
        }
    }
}

/// The new global speed limit: "unlimited" is a value, unlike `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum LimitPatch {
    Unlimited,
    Rate { bytes_per_sec: u64 },
}

/// `None` (empty for `extra_extensions`) leaves a setting as it is. The
/// whole patch applies or none of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SettingsPatch {
    #[uniffi(default)]
    pub global_speed_limit: Option<LimitPatch>,
    #[uniffi(default)]
    pub headroom_percent: Option<u8>,
    #[uniffi(default)]
    pub global_connections: Option<u32>,
    #[uniffi(default)]
    pub per_host_connections: Option<u32>,
    #[uniffi(default)]
    pub min_connections_for_new: Option<u32>,
    #[uniffi(default)]
    pub default_connections: Option<u8>,
    #[uniffi(default)]
    pub max_connections_per_download: Option<u8>,
    #[uniffi(default)]
    pub first_byte_timeout_ms: Option<u64>,
    #[uniffi(default)]
    pub idle_timeout_ms: Option<u64>,
    #[uniffi(default)]
    pub max_retries: Option<u32>,
    #[uniffi(default)]
    pub commit_interval_ms: Option<u64>,
    #[uniffi(default)]
    pub probe_range_get: Option<bool>,
    #[uniffi(default)]
    pub use_content_disposition: Option<bool>,
    #[uniffi(default)]
    pub append_type_extension: Option<bool>,
    #[uniffi(default)]
    pub auto_grouping: Option<bool>,
    /// Each replaces that category's extras and takes them out of others;
    /// applied in order.
    #[uniffi(default)]
    pub extra_extensions: Vec<CategoryExtensions>,
    #[uniffi(default)]
    pub proxy_mode: Option<ProxyMode>,
}

impl TryFrom<SettingsPatch> for CorePatch {
    type Error = DhowError;

    fn try_from(p: SettingsPatch) -> Result<Self, DhowError> {
        Ok(CorePatch {
            global_speed_limit: p.global_speed_limit.map(|l| match l {
                LimitPatch::Unlimited => CoreLimit::Unlimited,
                LimitPatch::Rate { bytes_per_sec } => CoreLimit::Rate(bytes_per_sec),
            }),
            headroom_percent: p.headroom_percent,
            global_connections: p.global_connections,
            per_host_connections: p.per_host_connections,
            min_connections_for_new: p.min_connections_for_new,
            default_connections: p.default_connections,
            max_connections_per_download: p.max_connections_per_download,
            first_byte_timeout_ms: p.first_byte_timeout_ms,
            idle_timeout_ms: p.idle_timeout_ms,
            max_retries: p.max_retries,
            commit_interval_ms: p.commit_interval_ms,
            probe_range_get: p.probe_range_get,
            use_content_disposition: p.use_content_disposition,
            append_type_extension: p.append_type_extension,
            auto_grouping: p.auto_grouping,
            extra_extensions: p
                .extra_extensions
                .into_iter()
                .map(|e| CoreExtensions {
                    category: e.category.into(),
                    extensions: e.extensions,
                })
                .collect(),
            proxy_mode: p.proxy_mode.map(TryInto::try_into).transpose()?,
        })
    }
}

/// Core → shell, in order, on one listener (ARCHITECTURE §3.2). `Progress`
/// still queued for the listener is replaced by a newer one (§3.3).
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum Event {
    StateChanged {
        id: String,
        from: DownloadState,
        to: DownloadState,
        reason: Option<FailReason>,
    },
    Progress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
        connections: u32,
        /// EMA of the 1 s samples (§14).
        speed_bps: u64,
        /// `None`: unknown size, under 3 samples or under 1 KiB/s.
        eta_secs: Option<u64>,
    },
    /// Started over from zero.
    Reset {
        id: String,
        reason: ResetReason,
    },
    /// Reconnecting after `failures` consecutive failures.
    Retrying {
        id: String,
        failures: u32,
        delay_ms: u64,
    },
    /// The file is at `path` now: completed (§8), or found moved (§6).
    Renamed {
        id: String,
        path: String,
    },
    /// `id`: `None` for one about no download (ARCHITECTURE §3.2).
    Warning {
        id: Option<String>,
        kind: WarningKind,
    },
    GroupChanged {
        id: String,
        group: Option<GroupSnapshot>,
    },
    SettingsChanged {
        settings: DhowSettings,
    },
}

impl Event {
    /// The download it is about.
    pub(crate) fn download(&self) -> Option<&str> {
        match self {
            Self::StateChanged { id, .. }
            | Self::Progress { id, .. }
            | Self::Reset { id, .. }
            | Self::Retrying { id, .. }
            | Self::Renamed { id, .. }
            | Self::GroupChanged { id, .. } => Some(id),
            Self::Warning { id, .. } => id.as_deref(),
            Self::SettingsChanged { .. } => None,
        }
    }
}

impl From<EngineEvent> for Event {
    fn from(e: EngineEvent) -> Self {
        match e {
            EngineEvent::StateChanged {
                id,
                from,
                to,
                reason,
            } => Self::StateChanged {
                id,
                from: from.into(),
                to: to.into(),
                reason: reason.map(Into::into),
            },
            EngineEvent::Progress {
                id,
                downloaded,
                total,
                connections,
                speed_bps,
                eta_secs,
            } => Self::Progress {
                id,
                downloaded,
                total,
                connections,
                speed_bps,
                eta_secs,
            },
            EngineEvent::Reset { id, reason } => Self::Reset {
                id,
                reason: reason.into(),
            },
            EngineEvent::Retrying {
                id,
                failures,
                delay,
            } => Self::Retrying {
                id,
                failures,
                delay_ms: delay.as_millis().try_into().unwrap_or(u64::MAX),
            },
            EngineEvent::Renamed { id, path: p } => Self::Renamed { id, path: path(p) },
            EngineEvent::Warning { id, kind } => Self::Warning {
                id: Some(id),
                kind: kind.into(),
            },
            EngineEvent::GroupChanged { id, group } => Self::GroupChanged {
                id,
                group: group.map(Into::into),
            },
            EngineEvent::SettingsChanged { settings } => Self::SettingsChanged {
                settings: settings.into(),
            },
        }
    }
}
