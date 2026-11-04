// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! systemd readiness and watchdog notifications.
//!
//! With `Type=notify` the unit is started once [`ready`] is sent, and with
//! `WatchdogSec=` it is restarted when [`watchdog`] stops arriving. Without
//! `NOTIFY_SOCKET` in the environment every call does nothing.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

/// The notification socket and the watchdog interval systemd asked for.
struct Notifier {
    #[cfg(target_os = "linux")]
    socket: std::os::unix::net::UnixDatagram,
    #[cfg(target_os = "linux")]
    addr: std::os::unix::net::SocketAddr,
    /// Half the watchdog timeout, so one late ping is not a restart.
    ping_every: Option<Duration>,
    last_ping: Mutex<Option<Instant>>,
}

static NOTIFIER: OnceLock<Option<Notifier>> = OnceLock::new();
static READY: AtomicBool = AtomicBool::new(false);

/// Tells systemd the service is up. Only the first call is sent.
pub(crate) fn ready() {
    if !READY.swap(true, Ordering::SeqCst) {
        send("READY=1");
    }
}

/// Tells systemd the service is alive, at most every half watchdog
/// interval. Does nothing when no watchdog is set.
pub(crate) fn watchdog() {
    let Some(n) = notifier() else {
        return;
    };
    let Some(every) = n.ping_every else {
        return;
    };
    let now = Instant::now();
    {
        let mut last = n.last_ping.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now.duration_since(t) < every) {
            return;
        }
        *last = Some(now);
    }
    send("WATCHDOG=1");
}

/// A one-line status `systemctl status` shows.
pub(crate) fn status(text: &str) {
    send(&format!("STATUS={text}"));
}

fn notifier() -> Option<&'static Notifier> {
    NOTIFIER.get_or_init(open).as_ref()
}

fn send(message: &str) {
    if let Some(n) = notifier() {
        n.send(message);
    }
}

/// The watchdog interval for this process from `WATCHDOG_USEC` and
/// `WATCHDOG_PID`, halved.
fn ping_interval(usec: Option<&str>, pid: Option<&str>) -> Option<Duration> {
    if pid.is_some_and(|p| p.parse::<u32>().ok() != Some(std::process::id())) {
        return None;
    }
    let usec: u64 = usec?.parse().ok().filter(|&u| u > 0)?;
    Some(Duration::from_micros(usec / 2))
}

#[cfg(target_os = "linux")]
fn open() -> Option<Notifier> {
    use std::os::linux::net::SocketAddrExt as _;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let path = std::env::var_os("NOTIFY_SOCKET")?;
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let addr = match bytes.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(&path),
    };
    let addr = addr
        .map_err(|e| tracing::warn!("NOTIFY_SOCKET {path:?}: {e}"))
        .ok()?;
    let socket = UnixDatagram::unbound()
        .map_err(|e| tracing::warn!("cannot open the systemd notification socket: {e}"))
        .ok()?;
    Some(Notifier {
        socket,
        addr,
        ping_every: ping_interval(
            std::env::var("WATCHDOG_USEC").ok().as_deref(),
            std::env::var("WATCHDOG_PID").ok().as_deref(),
        ),
        last_ping: Mutex::new(None),
    })
}

#[cfg(not(target_os = "linux"))]
fn open() -> Option<Notifier> {
    None
}

impl Notifier {
    #[cfg(target_os = "linux")]
    fn send(&self, message: &str) {
        if let Err(e) = self.socket.send_to_addr(message.as_bytes(), &self.addr) {
            tracing::debug!("systemd notification {message:?}: {e}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send(&self, _message: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watchdog_pings_at_half_its_interval() {
        assert_eq!(
            ping_interval(Some("30000000"), None),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn a_watchdog_for_another_process_is_ignored() {
        let mine = std::process::id().to_string();
        assert!(ping_interval(Some("30000000"), Some(&mine)).is_some());
        assert_eq!(ping_interval(Some("30000000"), Some("1")), None);
    }

    #[test]
    fn no_or_zero_watchdog_is_none() {
        assert_eq!(ping_interval(None, None), None);
        assert_eq!(ping_interval(Some("0"), None), None);
        assert_eq!(ping_interval(Some("soon"), None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn notifications_reach_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::{SocketAddr, UnixDatagram};

        let name = format!("edgefirst-camera-notify-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let server = UnixDatagram::bind_addr(&addr).unwrap();
        let n = Notifier {
            socket: UnixDatagram::unbound().unwrap(),
            addr,
            ping_every: None,
            last_ping: Mutex::new(None),
        };
        n.send("READY=1");
        let mut buf = [0u8; 64];
        let len = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"READY=1");
    }
}
