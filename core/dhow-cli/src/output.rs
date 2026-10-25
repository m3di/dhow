//! Everything the CLI reports is one JSON object per line on stdout, so
//! scripts and spec-tests read the same thing a person does.

use dhow_core::download::ResetReason;
use dhow_core::engine::categories::CategoryUsage;
use dhow_core::engine::groups::GroupSnapshot;
use dhow_core::engine::queues::QueueSnapshot;
use dhow_core::engine::settings::Settings;
use dhow_core::engine::speed::SpeedSample;
use dhow_core::engine::{EngineEvent, Snapshot, WarningKind};
use dhow_core::platform::{InterfaceKind, NetworkInfo, ProxyConfig, ProxyMode, redact_proxy_url};
use dhow_core::probe::Resumable;
use serde_json::{Value, json};

pub fn emit(v: Value) {
    println!("{v}");
}

pub fn event(e: &EngineEvent) -> Value {
    match e {
        EngineEvent::DownloadAdded { snapshot: s } => snapshot("added", s),
        EngineEvent::Removed { id, queue_id } => json!({
            "event": "removed", "id": id, "queue": queue_id,
        }),
        EngineEvent::RangeMap { id, map } => json!({
            "event": "range_map", "id": id, "total": map.total,
            "done": map.done.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>(),
            "connections": map.connections.iter().map(|c| [c.start, c.at, c.end]).collect::<Vec<_>>(),
            "pending": map.pending.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>(),
        }),
        EngineEvent::StateChanged {
            id,
            from,
            to,
            reason,
        } => json!({
            "event": "state", "id": id, "from": from.as_str(), "to": to.as_str(),
            "reason": reason.as_ref().map(|r| r.as_db()),
        }),
        EngineEvent::Progress {
            id,
            downloaded,
            total,
            connections,
            speed_bps,
            eta_secs,
        } => json!({
            "event": "progress", "id": id, "downloaded": downloaded, "total": total,
            "connections": connections, "speed_bps": speed_bps, "eta_secs": eta_secs,
        }),
        EngineEvent::Reset { id, reason } => json!({
            "event": "reset", "id": id, "reason": match reason {
                ResetReason::ServerFileChanged => "server_file_changed",
                ResetReason::Unaccountable => "unaccountable",
                ResetReason::CannotResume => "cannot_resume",
            },
        }),
        EngineEvent::Retrying {
            id,
            failures,
            delay,
        } => json!({
            "event": "retrying", "id": id, "failures": failures, "delay_ms": delay.as_millis() as u64,
        }),
        EngineEvent::Renamed { id, path } => json!({
            "event": "renamed", "id": id, "path": path.to_string_lossy(),
        }),
        EngineEvent::Warning { id, kind } => json!({
            "event": "warning", "id": id, "kind": match kind {
                WarningKind::NotResumable => "not_resumable",
                WarningKind::SizeUnverified => "size_unverified",
                WarningKind::IdentityWeak => "identity_weak",
                WarningKind::ServerIgnoredRange => "server_ignored_range",
            },
        }),
        EngineEvent::GroupChanged { id, group } => json!({
            "event": "group_changed", "id": id, "group": group.as_ref().map(group_fields),
        }),
        EngineEvent::SettingsChanged { settings: s } => settings("settings_changed", s),
    }
}

pub fn snapshot(event: &str, s: &Snapshot) -> Value {
    json!({
        "event": event,
        "id": s.id,
        "state": s.state.as_str(),
        "reason": s.fail_reason.as_ref().map(|r| r.as_db()),
        "path": s.final_path.to_string_lossy(),
        "url": s.url,
        "total": s.total_size,
        "downloaded": s.downloaded,
        "resumable": match s.resumable {
            Resumable::Yes => "yes",
            Resumable::No => "no",
            Resumable::Unknown => "unknown",
        },
        "size_unverified": s.size_unverified,
        "identity_weak": s.identity_weak,
        "queue": s.queue_id,
        "position": s.position,
        "speed_limit": s.speed_limit,
        "category": s.category.as_str(),
        "group": s.group,
    })
}

pub fn group(g: &GroupSnapshot) -> Value {
    let mut v = group_fields(g);
    v["event"] = "group".into();
    v
}

fn group_fields(g: &GroupSnapshot) -> Value {
    json!({
        "id": g.id, "name": g.name, "auto": g.auto, "members": g.members,
        "downloaded": g.downloaded, "total": g.total, "speed_bps": g.speed_bps,
        "eta_secs": g.eta_secs,
    })
}

pub fn storage(u: &CategoryUsage) -> Value {
    json!({
        "event": "storage", "category": u.category.as_str(), "bytes": u.bytes,
        "files": u.files, "missing": u.missing,
    })
}

pub fn sample(id: Option<&str>, s: &SpeedSample) -> Value {
    json!({
        "event": "sample", "id": id, "at": s.at, "period_secs": s.period_secs,
        "bytes_per_sec": s.bytes_per_sec,
    })
}

pub fn settings(event: &str, s: &Settings) -> Value {
    json!({
        "event": event,
        "global_speed_limit": s.global_speed_limit,
        "headroom_percent": s.headroom_percent,
        "global_connections": s.global_connections,
        "per_host_connections": s.per_host_connections,
        "min_connections_for_new": s.min_connections_for_new,
        "default_connections": s.default_connections,
        "max_connections_per_download": s.max_connections_per_download,
        "first_byte_timeout_ms": s.first_byte_timeout_ms,
        "idle_timeout_ms": s.idle_timeout_ms,
        "max_retries": s.max_retries,
        "commit_interval_ms": s.commit_interval_ms,
        "probe_range_get": s.probe_range_get,
        "use_content_disposition": s.use_content_disposition,
        "append_type_extension": s.append_type_extension,
        "auto_grouping": s.auto_grouping,
        "extra_extensions": s.extra_extensions,
        "proxy": proxy_mode(&s.proxy_mode),
    })
}

pub fn queue(q: &QueueSnapshot) -> Value {
    json!({
        "event": "queue",
        "id": q.id,
        "name": q.name,
        "default": q.is_default,
        "max_concurrent": q.max_concurrent,
        "speed_limit": q.speed_limit,
        "on_complete": q.on_complete.as_str(),
        "unfinished": q.unfinished,
        "queued": q.queued,
        "running": q.running,
        "schedule": q.schedule,
        "run_state": q.run_state.as_str(),
        "run_state_until": q.run_state_until,
        "active": q.active_at(&jiff::Zoned::now()),
    })
}

pub fn network(n: &NetworkInfo) -> Value {
    json!({
        "event": "network",
        "interface": match n.interface_kind {
            InterfaceKind::Wifi => "wifi",
            InterfaceKind::Ethernet => "ethernet",
            InterfaceKind::Cellular => "cellular",
            InterfaceKind::Vpn => "vpn",
            InterfaceKind::Unknown => "unknown",
        },
        "vpn_active": n.vpn_active,
        "system_proxy": n.system_proxy.as_ref().map(proxy),
        "proxy_in_use": proxy_mode(&n.proxy_in_use),
    })
}

fn proxy_mode(m: &ProxyMode) -> Value {
    match m {
        ProxyMode::System => json!("system"),
        ProxyMode::None => json!("none"),
        ProxyMode::Custom(c) => proxy(c),
    }
}

/// Credentials and query strings in proxy URLs are not printed.
fn proxy(c: &ProxyConfig) -> Value {
    let url = |u: &Option<dhow_core::http::Url>| u.as_ref().map(redact_proxy_url);
    json!({
        "http": url(&c.http), "https": url(&c.https), "all": url(&c.all),
        "no_proxy": c.no_proxy, "pac": url(&c.pac),
    })
}
