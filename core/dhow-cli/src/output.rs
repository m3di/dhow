//! Everything the CLI reports is one JSON object per line on stdout, so
//! scripts and spec-tests read the same thing a person does.

use dhow_core::download::ResetReason;
use dhow_core::engine::limits::LimitSettings;
use dhow_core::engine::queues::QueueSnapshot;
use dhow_core::engine::speed::SpeedSample;
use dhow_core::engine::{EngineEvent, Snapshot, WarningKind};
use dhow_core::probe::Resumable;
use serde_json::{Value, json};

pub fn emit(v: Value) {
    println!("{v}");
}

pub fn event(e: &EngineEvent) -> Value {
    match e {
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
    })
}

pub fn sample(id: Option<&str>, s: &SpeedSample) -> Value {
    json!({
        "event": "sample", "id": id, "at": s.at, "period_secs": s.period_secs,
        "bytes_per_sec": s.bytes_per_sec,
    })
}

pub fn settings(s: &LimitSettings) -> Value {
    json!({
        "event": "settings",
        "global_speed_limit": s.global,
        "headroom_percent": s.headroom_percent,
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
        "schedule": q.schedule,
        "run_state": q.run_state.as_str(),
        "run_state_until": q.run_state_until,
        "active": q.active_at(&jiff::Zoned::now()),
    })
}
