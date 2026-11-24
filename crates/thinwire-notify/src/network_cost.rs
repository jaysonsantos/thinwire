//! The cost of the network, for the automatic download of photos and
//! videos.
//!
//! - Linux: the `Metered` value of NetworkManager on the system bus. One
//!   thread reads it every 30 s and sends a change. With no
//!   NetworkManager or no system bus, the cost is unknown.
//! - macOS: a path monitor of Network.framework (module `network_macos`).
//!   It sends each new path at once: a phone network, a hotspot, or Low
//!   Data Mode is metered.
//! - Windows: not read yet. The cost stays unknown, and
//!   [`NetworkWatch::SUPPORTED`] is `false`: the Windows call is not checked
//!   on Windows.
//!
//! An unknown cost counts as metered (`thinwire_core::media_auto`).

use std::sync::mpsc::{self, Receiver};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

use thinwire_core::NetworkCost;

/// How often the thread reads the cost. macOS sends each change instead.
#[cfg(not(target_os = "macos"))]
pub const POLL: Duration = Duration::from_secs(30);

/// The thread that reads the cost. It ends when the watch drops.
pub struct NetworkWatch {
    changes: Option<Receiver<NetworkCost>>,
}

impl NetworkWatch {
    /// This OS tells the cost of the network. A frontend offers the choice
    /// "On unmetered networks" only when it does.
    pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

    /// Start the thread. `wake` runs after each change, so the frontend
    /// reads it soon.
    #[must_use]
    pub fn start(wake: impl Fn() + Send + 'static) -> Self {
        if !Self::SUPPORTED {
            let _ = wake;
            return Self { changes: None };
        }
        let (send, changes) = mpsc::channel();
        Self::begin(send, changes, wake)
    }

    /// The path monitor of Network.framework sends each new cost.
    #[cfg(target_os = "macos")]
    fn begin(
        send: mpsc::Sender<NetworkCost>,
        changes: Receiver<NetworkCost>,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        if crate::network_macos::start(send, wake) {
            Self {
                changes: Some(changes),
            }
        } else {
            tracing::warn!("network path monitor did not start");
            Self { changes: None }
        }
    }

    /// The thread that reads the cost every `POLL` and sends a change.
    #[cfg(not(target_os = "macos"))]
    fn begin(
        send: mpsc::Sender<NetworkCost>,
        changes: Receiver<NetworkCost>,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let spawned = std::thread::Builder::new()
            .name("thinwire-network".into())
            .spawn(move || {
                let mut reader = Reader::default();
                let mut last = None;
                loop {
                    let cost = reader.read();
                    if last != Some(cost) {
                        last = Some(cost);
                        if send.send(cost).is_err() {
                            return;
                        }
                        wake();
                    }
                    std::thread::sleep(POLL);
                }
            });
        match spawned {
            Ok(_) => Self {
                changes: Some(changes),
            },
            Err(error) => {
                tracing::warn!(%error, "network cost thread did not start");
                Self { changes: None }
            }
        }
    }

    /// The newest cost, when it changed since the last call.
    pub fn changed(&mut self) -> Option<NetworkCost> {
        let changes = self.changes.as_ref()?;
        let mut newest = None;
        while let Ok(cost) = changes.try_recv() {
            newest = Some(cost);
        }
        newest
    }
}

/// NetworkManager `NMMetered`: 1 yes, 2 no, 3 guess yes, 4 guess no, 0
/// unknown.
#[must_use]
pub fn cost_of_nm_metered(value: u32) -> NetworkCost {
    match value {
        1 | 3 => NetworkCost::Metered,
        2 | 4 => NetworkCost::Unmetered,
        _ => NetworkCost::Unknown,
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct Reader {
    bus: Option<zbus::blocking::Connection>,
}

#[cfg(target_os = "linux")]
impl Reader {
    const SERVICE: &'static str = "org.freedesktop.NetworkManager";
    const PATH: &'static str = "/org/freedesktop/NetworkManager";

    fn read(&mut self) -> NetworkCost {
        if self.bus.is_none() {
            self.bus = zbus::blocking::Connection::system().ok();
        }
        let Some(bus) = &self.bus else {
            return NetworkCost::Unknown;
        };
        match Self::metered(bus) {
            Some(value) => cost_of_nm_metered(value),
            None => {
                // A new connection next time: the bus may have restarted.
                self.bus = None;
                NetworkCost::Unknown
            }
        }
    }

    fn metered(bus: &zbus::blocking::Connection) -> Option<u32> {
        let reply = bus
            .call_method(
                Some(Self::SERVICE),
                Self::PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(Self::SERVICE, "Metered"),
            )
            .ok()?;
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
        match &*value {
            zbus::zvariant::Value::U32(value) => Some(*value),
            zbus::zvariant::Value::Value(inner) => match &**inner {
                zbus::zvariant::Value::U32(value) => Some(*value),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Default)]
struct Reader;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Reader {
    fn read(&mut self) -> NetworkCost {
        NetworkCost::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guess_counts_as_the_value_and_zero_is_unknown() {
        assert_eq!(cost_of_nm_metered(1), NetworkCost::Metered);
        assert_eq!(cost_of_nm_metered(3), NetworkCost::Metered);
        assert_eq!(cost_of_nm_metered(2), NetworkCost::Unmetered);
        assert_eq!(cost_of_nm_metered(4), NetworkCost::Unmetered);
        assert_eq!(cost_of_nm_metered(0), NetworkCost::Unknown);
        assert_eq!(cost_of_nm_metered(9), NetworkCost::Unknown);
    }

    #[test]
    fn linux_and_macos_read_the_cost() {
        assert_eq!(
            NetworkWatch::SUPPORTED,
            cfg!(any(target_os = "linux", target_os = "macos"))
        );
    }
}
