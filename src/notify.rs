//! Desktop notifications for attention items (yggdrasil-135).
//!
//! Engagement-UX research called out OSC 9 / OSC 777 as "use only for
//! human-required events"; we hold to that here. Three trigger
//! classes get a one-shot notification:
//!
//!   - run requires approval (yggdrasil-130 attention items)
//!   - agent hung > 10 min
//!   - high-priority task closed (P0 / P1)
//!
//! Each trigger is keyed (run_id / agent_id / task_ref) so the same
//! item never re-pings — a once-per-life dedupe lives in the caller's
//! `seen` set.
//!
//! On macOS we shell out to `osascript -e 'display notification ...'`
//! (works headlessly on every laptop, no extra deps). On Linux,
//! `notify-send` is the convention. Unknown OS → no-op silently. The
//! `YGG_NOTIFY=off` env var is the universal kill switch so users
//! never get surprised by a beep.

use std::process::{Command, Stdio};

/// Severity drives the notification subtitle and (where the OS
/// honours it) the ringer sound. Lean on three bands so users can
/// pattern-match on category without reading the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifySeverity {
    Info,
    Warn,
    Critical,
}

impl NotifySeverity {
    pub fn as_subtitle(self) -> &'static str {
        match self {
            Self::Info => "Yggdrasil",
            Self::Warn => "Yggdrasil · attention",
            Self::Critical => "Yggdrasil · critical",
        }
    }
}

/// `YGG_NOTIFY=off` (or the usual truthy variants of "off") suppresses
/// every notification. Anything else — including unset — leaves the
/// default behaviour: notify when a trigger fires.
pub fn notifications_disabled() -> bool {
    matches!(
        std::env::var("YGG_NOTIFY").ok().as_deref(),
        Some("off") | Some("0") | Some("false") | Some("no")
    )
}

/// Fire a notification on the current platform. Returns `Ok(true)`
/// when the OS-level notify command was invoked, `Ok(false)` when
/// suppressed by env var, and `Err(_)` only when the spawn itself
/// failed (which we render as a Toast rather than panicking).
pub fn notify(title: &str, body: &str, severity: NotifySeverity) -> Result<bool, std::io::Error> {
    if notifications_disabled() {
        return Ok(false);
    }
    if cfg!(target_os = "macos") {
        notify_mac(title, body, severity)?;
        Ok(true)
    } else if cfg!(target_os = "linux") {
        notify_linux(title, body, severity)?;
        Ok(true)
    } else {
        // Other platforms — silently do nothing. Logs are not a
        // notification channel; the events table already carries the
        // signal for anyone watching `ygg logs`.
        Ok(false)
    }
}

fn notify_mac(title: &str, body: &str, severity: NotifySeverity) -> std::io::Result<()> {
    // osascript escapes any embedded quotes if we use the AS string
    // type. Strip newlines from the body since AS doesn't render them
    // well — single-line bodies match the OS notification card shape.
    let safe_title = escape_as(title);
    let safe_body = escape_as(&body.replace('\n', " · "));
    let safe_subtitle = escape_as(severity.as_subtitle());
    let script = format!(
        "display notification \"{safe_body}\" with title \"{safe_title}\" subtitle \"{safe_subtitle}\""
    );
    let status = Command::new("osascript")
        .args(["-e", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "osascript exited {status:?}"
        )));
    }
    Ok(())
}

fn notify_linux(title: &str, body: &str, severity: NotifySeverity) -> std::io::Result<()> {
    let urgency = match severity {
        NotifySeverity::Info => "low",
        NotifySeverity::Warn => "normal",
        NotifySeverity::Critical => "critical",
    };
    Command::new("notify-send")
        .args(["-u", urgency, "-a", "yggdrasil", title, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

fn escape_as(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
