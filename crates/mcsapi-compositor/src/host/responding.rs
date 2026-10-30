//! Noticing a window that stopped answering, the way GNOME and KDE do: each
//! click or key into a Wayland window sends its client an `xdg_wm_base.ping`,
//! and a client that has not sent the pong after [`UNRESPONSIVE_AFTER`] is
//! stuck in something other than its event loop. The shell hears it through
//! [`Shell::window_responding`] for each of that client's windows, and again
//! when the pong arrives.
//!
//! Pinging on input, not on a timer, keeps an idle session from waking
//! every client for nothing; a window nobody is using is not one anyone is
//! waiting on. `xdg_wm_base` allows one ping in flight per client, so a
//! client with a ping already pending is not pinged again.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use mcsapi::WindowId;
use smithay::{utils::SERIAL_COUNTER, wayland::shell::xdg::ShellClient};

use super::{Content, Host};
use crate::Shell;

/// How long a pong may take. Mutter waits five seconds too; a busy app that
/// is only slow to draw answers well within it, because the pong comes
/// from its event loop and not from its renderer.
pub(super) const UNRESPONSIVE_AFTER: Duration = Duration::from_secs(5);

/// A ping waiting for its pong.
struct Ping {
    client: ShellClient,
    sent: Instant,
    /// Whether the shell was told this client's windows stopped responding.
    reported: bool,
}

/// The pings in flight.
#[derive(Default)]
pub(super) struct Responding {
    pings: Vec<Ping>,
}

/// The Wayland windows `client` has open.
fn windows_of(windows: &HashMap<WindowId, Content>, client: &ShellClient) -> Vec<WindowId> {
    windows
        .iter()
        .filter_map(|(id, content)| match content {
            Content::Wayland(window)
                if window.toplevel().is_some_and(|t| t.client() == *client) =>
            {
                Some(*id)
            }
            _ => None,
        })
        .collect()
}

impl<S: Shell> Host<S> {
    /// Pings the client behind `window`, when it is a Wayland window and its
    /// client has no ping pending.
    pub(super) fn ping(&mut self, window: Option<WindowId>) {
        let Some(Content::Wayland(window)) = window.and_then(|id| self.windows.get(&id)) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let client = toplevel.client();
        if self.responding.pings.iter().any(|p| p.client == client) {
            return;
        }
        // Fails only for a client that is gone or already has a ping out,
        // which Smithay tracks itself; either way nothing is waited on.
        if client.send_ping(SERIAL_COUNTER.next_serial()).is_ok() {
            self.responding.pings.push(Ping {
                client,
                sent: Instant::now(),
                reported: false,
            });
        }
    }

    /// Tells the shell about clients whose pong is late. Run from the
    /// one-second timer, so a hung window is noticed within six seconds.
    pub(super) fn check_responding(&mut self) {
        let now = Instant::now();
        let mut late = Vec::new();
        let windows = &self.windows;
        self.responding.pings.retain_mut(|ping| {
            // A client that disconnected took its windows with it, and the
            // shell already unmapped them.
            if !ping.client.alive() {
                return false;
            }
            if !ping.reported && now.duration_since(ping.sent) >= UNRESPONSIVE_AFTER {
                ping.reported = true;
                late.extend(windows_of(windows, &ping.client));
            }
            true
        });
        for id in late {
            self.shell.window_responding(id, false);
            self.dirty = true;
        }
    }

    /// A pong: the client is back in its event loop.
    pub(super) fn ponged(&mut self, client: &ShellClient) {
        let Some(i) = self
            .responding
            .pings
            .iter()
            .position(|p| p.client == *client)
        else {
            return;
        };
        let ping = self.responding.pings.remove(i);
        if ping.reported {
            for id in windows_of(&self.windows, client) {
                self.shell.window_responding(id, true);
            }
            self.dirty = true;
        }
    }
}
