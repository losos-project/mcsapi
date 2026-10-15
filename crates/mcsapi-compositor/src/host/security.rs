//! `wp_security_context_manager_v1`: a sandbox (Flatpak) opens a listening
//! socket for the app it starts and attaches its metadata, and clients that
//! connect through that socket are marked sandboxed. Globals that reach past
//! a client's own windows (placing surfaces over every window, reading the
//! clipboard without focus, typing or clicking into other clients) are
//! created with [`unsandboxed`] as their filter, so a sandboxed client never
//! sees them; nor may it open a further context.
//!
//! The shell can open such a socket too ([`crate::Command::Sandbox`]), for a
//! sandbox it runs itself and so has no Wayland connection to ask through.

use std::{
    collections::HashMap,
    io,
    os::unix::{fs::FileTypeExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::Arc,
};

use smithay::{
    delegate_security_context,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken, generic::Generic},
        wayland_server::{Client, DisplayHandle},
    },
    wayland::security_context::{
        SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
        SecurityContextState,
    },
};
use tracing::{info, warn};

use super::{ClientState, Host};
use crate::{Sandbox, Shell};

pub(super) struct Security<S: Shell + 'static> {
    _state: SecurityContextState,
    handle: LoopHandle<'static, Host<S>>,
    /// The shell's own sandbox sockets, by path.
    sockets: HashMap<PathBuf, RegistrationToken>,
}

impl<S: Shell + 'static> Security<S> {
    pub(super) fn new(dh: &DisplayHandle, handle: LoopHandle<'static, Host<S>>) -> Self {
        Self {
            _state: SecurityContextState::new::<Host<S>, _>(dh, unsandboxed),
            handle,
            sockets: HashMap::new(),
        }
    }

    /// Listens at `socket` for clients of `sandbox` ([`crate::Command::Sandbox`]).
    pub(super) fn listen(&mut self, socket: PathBuf, sandbox: Sandbox) {
        self.close(&socket);
        let listener = match bind(&socket) {
            Ok(listener) => listener,
            Err(e) => {
                warn!(socket = %socket.display(), error = %e, "cannot open a sandbox socket");
                return;
            }
        };
        info!(
            socket = %socket.display(),
            engine = sandbox.engine.as_deref().unwrap_or("?"),
            app = sandbox.app_id.as_deref().unwrap_or("?"),
            "shell opened a sandbox socket"
        );
        let source = Generic::new(listener, Interest::READ, Mode::Level);
        let inserted = self.handle.insert_source(source, move |_, listener, host| {
            // Level-triggered: whatever is left is accepted on the next
            // dispatch, so one pass takes what is waiting now and stops.
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = ClientState {
                            sandbox: Some(sandbox.clone()),
                            ..ClientState::default()
                        };
                        if let Err(e) = host.display.insert_client(stream, Arc::new(state)) {
                            warn!(error = %e, "rejected sandboxed client");
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        warn!(error = %e, "cannot accept a sandboxed client");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        });
        match inserted {
            Ok(token) => {
                self.sockets.insert(socket, token);
            }
            Err(e) => warn!(error = %e, "cannot listen for a sandbox"),
        }
    }

    /// Stops listening at `socket` and removes it, if the shell opened it.
    pub(super) fn close(&mut self, socket: &Path) {
        if let Some(token) = self.sockets.remove(socket) {
            self.handle.remove(token);
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// A non-blocking listener at `path`, replacing a socket left there (by a
/// session that ended without closing it) but nothing else.
fn bind(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "something other than a socket is there",
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Whether `client` connected outside any sandbox's security context, so it
/// may bind the privileged globals.
pub(super) fn unsandboxed(client: &Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_some_and(|state| state.sandbox.is_none())
}

impl<S: Shell + 'static> SecurityContextHandler for Host<S> {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        info!(
            engine = context.sandbox_engine.as_deref().unwrap_or("?"),
            app = context.app_id.as_deref().unwrap_or("?"),
            "sandbox opened a socket"
        );
        let sandbox = Sandbox {
            engine: context.sandbox_engine,
            app_id: context.app_id,
            instance_id: context.instance_id,
        };
        // The source removes itself when the sandbox closes its end.
        let inserted = self
            .security
            .handle
            .insert_source(source, move |stream, _, host| {
                let state = ClientState {
                    sandbox: Some(sandbox.clone()),
                    ..ClientState::default()
                };
                if let Err(e) = host.display.insert_client(stream, Arc::new(state)) {
                    warn!(error = %e, "rejected sandboxed client");
                }
            });
        if let Err(e) = inserted {
            warn!(error = %e, "cannot listen for a sandbox");
        }
    }
}

delegate_security_context!(@<S: Shell + 'static> Host<S>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_replaces_a_stale_socket_only() {
        let dir = std::env::temp_dir().join(format!("mcsapi-sandbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("work.wayland");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(bind(&socket).is_ok(), "a stale socket is replaced");

        let file = dir.join("file");
        std::fs::write(&file, "keep").unwrap();
        assert!(bind(&file).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
