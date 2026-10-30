//! The clipboard: one for the whole session, as on any desktop. A window
//! copies with its own Ctrl+C and another pastes with its own Ctrl+V,
//! whichever app, sandbox or virtual machine each window comes from. A
//! machine's windows are clients like any other (through waypipe, scrcpy,
//! FreeRDP or the macOS window client), so no copy step stands between
//! them, unlike Qubes' Ctrl+Shift+C and Ctrl+Shift+V.
//!
//! What a client cannot do is read the clipboard without the keyboard:
//! `wl_data_device` offers the selection only to the focused client, and
//! the data-control globals that read it from anywhere are hidden from
//! sandboxed clients (see `automation` and `security`).
//!
//! Three things make it behave as people expect:
//!
//! - **What was copied outlives the window it came from.** A Wayland
//!   selection is served by the client that set it, so closing the window,
//!   or shutting down the machine it ran in, used to empty the clipboard.
//!   The host copies the common types of each new selection ([`KEPT`]) as
//!   soon as it is set, and offers that copy itself once its owner is gone.
//!   Only an explicit clear (a client setting no selection) empties it.
//! - **The in-process apps and the chrome take part.** They are egui, which
//!   learns of copying and pasting from `Copy`, `Cut` and `Paste` events,
//!   not from keys: Ctrl+C, Ctrl+X and Ctrl+V (and Ctrl+Insert, Shift+Delete
//!   and Shift+Insert) become those events, and a paste reads whoever holds
//!   the selection, a machine's window included, without blocking the
//!   session.
//! - **Nothing a client does can stall the session.** Every read and write
//!   runs on a thread of its own with a deadline, at most
//!   [`MAX_SELECTION_TRANSFERS`] at a time, and what is kept is capped
//!   ([`MAX_KEPT_BYTES`]).

use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcsapi::WindowId;
use smithay::{
    input::Seat,
    reexports::{
        calloop::{
            LoopHandle,
            channel::{self, Channel, Sender},
        },
        wayland_server::{Client, DisplayHandle, Resource},
    },
    wayland::selection::{
        SelectionHandler, SelectionSource, SelectionTarget,
        data_device::{
            DataDeviceHandler, SelectionRequestError, current_data_device_selection_userdata,
            request_data_device_client_selection, set_data_device_selection,
        },
    },
};
use tracing::{debug, warn};

use super::Host;
use crate::{Shell, egui};

pub(super) const MAX_SELECTION_TRANSFERS: usize = 4;
const SELECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client gets to hand over the selection: a machine's window
/// answers over vsock, which is quick, but a guest under load is not.
const SELECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The most a kept copy holds, all its types together. A selection bigger
/// than this (a large picture) is still pasted from its owner while it
/// lives, just not kept after it.
const MAX_KEPT_BYTES: usize = 64 << 20;

/// The types kept of a selection, in the order they are read. Text in the
/// names Wayland and X11 apps ask for, HTML for formatted text, and the
/// lists of files that file managers copy. An app's own formats, which it
/// may render on demand at some cost, are left to the app while it runs.
const KEPT: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "text/html",
    "text/uri-list",
    "x-special/gnome-copied-files",
];

/// The text types, best first, that a paste into an in-process app reads.
const TEXT: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

static ACTIVE_SELECTION_TRANSFERS: AtomicUsize = AtomicUsize::new(0);

pub(super) struct SelectionTransferGuard;

impl SelectionTransferGuard {
    pub(super) fn acquire() -> Option<Self> {
        let mut active = ACTIVE_SELECTION_TRANSFERS.load(Ordering::Relaxed);
        loop {
            if active >= MAX_SELECTION_TRANSFERS {
                return None;
            }
            match ACTIVE_SELECTION_TRANSFERS.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self),
                Err(current) => active = current,
            }
        }
    }
}

impl Drop for SelectionTransferGuard {
    fn drop(&mut self) {
        ACTIVE_SELECTION_TRANSFERS.fetch_sub(1, Ordering::Release);
    }
}

/// A selection the host serves itself: what an in-process app or the chrome
/// copied, or the kept copy of a client's.
#[derive(Debug, Default)]
pub struct Clip {
    types: Vec<(String, Arc<[u8]>)>,
}

impl Clip {
    /// Text, under the names Wayland and X11 apps ask for it by.
    pub(super) fn text(text: String) -> Self {
        let bytes: Arc<[u8]> = text.into_bytes().into();
        Self {
            types: ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
                .into_iter()
                .map(|mime| (mime.to_owned(), Arc::clone(&bytes)))
                .collect(),
        }
    }

    fn mime_types(&self) -> Vec<String> {
        self.types.iter().map(|(mime, _)| mime.clone()).collect()
    }

    fn get(&self, mime: &str) -> Option<&Arc<[u8]>> {
        self.types.iter().find(|(m, _)| m == mime).map(|(_, b)| b)
    }

    /// The best text it has, for a paste into an in-process app.
    fn as_text(&self) -> Option<String> {
        TEXT.iter()
            .find_map(|mime| self.get(mime))
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// Where a paste into the host's own egui goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PasteTarget {
    /// The shell's chrome (the palette, the overview's fields).
    Chrome,
    /// An in-process app's window.
    Window(WindowId),
}

/// What a transfer thread hands back to the session.
enum Done {
    /// The copy of the selection set in `generation`.
    Kept { generation: u64, clip: Clip },
    /// Text for a paste into the host's own egui.
    Paste { target: PasteTarget, text: String },
}

pub(super) struct Clipboard<S: Shell + 'static> {
    handle: LoopHandle<'static, Host<S>>,
    done: Sender<Done>,
    /// Goes up with every change of the selection, so a copy or a paste
    /// that finishes after the selection moved on is dropped.
    generation: u64,
    /// The types the client holding the selection offers; empty while the
    /// host holds it, or nobody does.
    offered: Vec<String>,
    /// The client that set the selection: the one with the keyboard, which
    /// `wl_data_device` requires. (A data-control client, such as wl-copy,
    /// sets it without the keyboard; then this is whoever had it, and the
    /// kept copy stands in for the same text a little early.)
    owner: Option<Client>,
    /// The copy of the client's selection, once read.
    kept: Option<Arc<Clip>>,
}

impl<S: Shell + 'static> Clipboard<S> {
    pub(super) fn new(handle: LoopHandle<'static, Host<S>>) -> Self {
        let (done, results): (Sender<Done>, Channel<Done>) = channel::channel();
        let inserted = handle.insert_source(results, |event, _, host| {
            if let channel::Event::Msg(done) = event {
                host.clipboard_done(done);
            }
        });
        if let Err(e) = inserted {
            warn!(error = %e, "cannot watch clipboard transfers");
        }
        Self {
            handle,
            done,
            generation: 0,
            offered: Vec::new(),
            owner: None,
            kept: None,
        }
    }
}

impl<S: Shell + 'static> Clipboard<S> {
    /// Text the host serves itself, for a caller holding other parts of
    /// the host (an in-process app's window) borrowed.
    pub(super) fn serve_text(&mut self, dh: &DisplayHandle, seat: &Seat<Host<S>>, text: String) {
        self.serve(dh, seat, Arc::new(Clip::text(text)));
    }

    fn serve(&mut self, dh: &DisplayHandle, seat: &Seat<Host<S>>, clip: Arc<Clip>) {
        self.generation += 1;
        self.offered.clear();
        self.owner = None;
        self.kept = None;
        set_data_device_selection(dh, seat, clip.mime_types(), clip);
    }
}

impl<S: Shell + 'static> Host<S> {
    /// The host's own selection, which it serves itself: what an in-process
    /// app or the chrome copied.
    pub(super) fn set_clipboard_text(&mut self, text: String) {
        self.clipboard.serve_text(&self.display, &self.seat, text);
    }

    /// A client set the clipboard, or cleared it. Smithay calls this before
    /// it takes the new selection in, so the copy is read once this dispatch
    /// is over.
    fn clipboard_changed(&mut self, source: Option<SelectionSource>, seat: &Seat<Self>) {
        let clipboard = &mut self.clipboard;
        clipboard.generation += 1;
        clipboard.kept = None;
        let Some(source) = source else {
            clipboard.offered.clear();
            clipboard.owner = None;
            return;
        };
        clipboard.offered = source.mime_types();
        clipboard.owner = seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .and_then(|surface| surface.client());
        let generation = clipboard.generation;
        clipboard
            .handle
            .insert_idle(move |host| host.keep(generation));
    }

    /// Reads the kept types of the client's selection on a thread of its
    /// own, unless the selection has moved on already.
    fn keep(&mut self, generation: u64) {
        let clipboard = &self.clipboard;
        if clipboard.generation != generation {
            return;
        }
        let mut wanted: Vec<&str> = KEPT
            .iter()
            .copied()
            .filter(|mime| clipboard.offered.iter().any(|m| m == mime))
            .collect();
        // One picture, PNG when there is one: apps offer a picture in every
        // format they can convert it to, and convert each one asked for.
        let picture = clipboard
            .offered
            .iter()
            .find(|m| *m == "image/png")
            .or_else(|| clipboard.offered.iter().find(|m| m.starts_with("image/")));
        wanted.extend(picture.map(String::as_str));
        if wanted.is_empty() {
            return;
        }
        let Some(guard) = SelectionTransferGuard::acquire() else {
            debug!("clipboard busy; not keeping this selection");
            return;
        };
        let mut reads = Vec::new();
        for mime in wanted {
            let Some((read, write)) = pipe() else {
                return;
            };
            match request_data_device_client_selection(&self.seat, mime.to_owned(), write) {
                Ok(()) => reads.push((mime.to_owned(), read)),
                // Gone or replaced since: nothing to keep.
                Err(_) => return,
            }
        }
        let done = self.clipboard.done.clone();
        let spawned = std::thread::Builder::new()
            .name("mcsapi-selection-keep".into())
            .spawn(move || {
                let _guard = guard;
                let types = read_all(reads, SELECTION_READ_TIMEOUT, MAX_KEPT_BYTES);
                let clip = Clip {
                    types: types
                        .into_iter()
                        .map(|(mime, bytes)| (mime, bytes.into()))
                        .collect(),
                };
                let _ = done.send(Done::Kept { generation, clip });
            });
        if let Err(error) = spawned {
            warn!(%error, "cannot start clipboard transfer");
        }
    }

    fn clipboard_done(&mut self, done: Done) {
        match done {
            Done::Kept { generation, clip } => {
                if generation == self.clipboard.generation && !clip.is_empty() {
                    self.clipboard.kept = Some(Arc::new(clip));
                    // The owner may have gone while its selection was read.
                    self.clipboard_owner_check();
                }
            }
            Done::Paste { target, text } => self.paste_text(target, text),
        }
    }

    /// Takes over the selection with its kept copy once the client that set
    /// it has gone: its last window closed, or its machine shut down. Called
    /// after every dispatch, so cheap when nothing changed.
    pub(super) fn clipboard_owner_check(&mut self) {
        let clipboard = &self.clipboard;
        let Some(owner) = &clipboard.owner else {
            return;
        };
        if clipboard.kept.is_none() || owner.get_credentials(&self.display).is_ok() {
            return;
        }
        self.restore_kept();
    }

    /// After the keyboard moved: Smithay drops a selection whose source has
    /// gone when it next offers it, which covers sources that went without
    /// their client (a data-control client's, whose owner is unknown).
    pub(super) fn clipboard_focus_changed(&mut self) {
        if self.clipboard.kept.is_none() || !self.clipboard_empty() {
            return;
        }
        self.restore_kept();
    }

    fn restore_kept(&mut self) {
        if let Some(kept) = self.clipboard.kept.take() {
            debug!("the clipboard's owner went; serving its kept copy");
            self.clipboard.serve(&self.display, &self.seat, kept);
        }
    }

    /// Whether nobody holds the selection. Smithay has no question for
    /// that, but asking a selection for a type no selection has tells:
    /// only an empty one answers that there is none.
    fn clipboard_empty(&self) -> bool {
        let Some((_, write)) = pipe() else {
            return false;
        };
        matches!(
            request_data_device_client_selection(&self.seat, String::new(), write),
            Err(SelectionRequestError::NoSelection)
        )
    }

    /// A paste into the host's own egui: the host's selection at once,
    /// a client's once it has written it.
    pub(super) fn paste_into(&mut self, target: PasteTarget) {
        let own = current_data_device_selection_userdata(&self.seat)
            .map(|clip| clip.as_text())
            .or_else(|| self.clipboard.kept.as_ref().map(|clip| clip.as_text()));
        if let Some(own) = own {
            if let Some(text) = own {
                self.paste_text(target, text);
            }
            return;
        }
        let Some(mime) = TEXT
            .iter()
            .find(|mime| self.clipboard.offered.iter().any(|m| m == *mime))
        else {
            return;
        };
        let Some(guard) = SelectionTransferGuard::acquire() else {
            return;
        };
        let Some((read, write)) = pipe() else {
            return;
        };
        if request_data_device_client_selection(&self.seat, (*mime).to_owned(), write).is_err() {
            return;
        }
        let done = self.clipboard.done.clone();
        let mime = (*mime).to_owned();
        let spawned = std::thread::Builder::new()
            .name("mcsapi-selection-paste".into())
            .spawn(move || {
                let _guard = guard;
                if let Some((_, bytes)) =
                    read_all(vec![(mime, read)], SELECTION_READ_TIMEOUT, MAX_KEPT_BYTES).pop()
                {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = done.send(Done::Paste { target, text });
                }
            });
        if let Err(error) = spawned {
            warn!(%error, "cannot start clipboard transfer");
        }
    }

    fn paste_text(&mut self, target: PasteTarget, text: String) {
        if text.is_empty() {
            return;
        }
        let events = match target {
            PasteTarget::Chrome => Some(&mut self.chrome_events),
            PasteTarget::Window(window) => self.internal_events(Some(window)),
        };
        if let Some(events) = events {
            events.push(egui::Event::Paste(text));
            self.dirty = true;
        }
    }
}

/// What a clipboard key means to egui, which takes copying and pasting as
/// events of their own rather than keys: Ctrl+C and Ctrl+Insert copy,
/// Ctrl+X and Shift+Delete cut, Ctrl+V and Shift+Insert paste, as
/// egui-winit has them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClipboardKey {
    Copy,
    Cut,
    Paste,
}

impl ClipboardKey {
    pub(super) fn of(key: egui::Key, mods: egui::Modifiers) -> Option<Self> {
        use egui::Key;
        let ctrl = mods.ctrl && !mods.alt;
        let shift = mods.shift && !mods.ctrl && !mods.alt;
        match key {
            Key::C if ctrl && !mods.shift => Some(Self::Copy),
            Key::X if ctrl && !mods.shift => Some(Self::Cut),
            Key::V if ctrl && !mods.shift => Some(Self::Paste),
            Key::Insert if ctrl && !mods.shift => Some(Self::Copy),
            Key::Delete if shift => Some(Self::Cut),
            Key::Insert if shift => Some(Self::Paste),
            Key::Copy => Some(Self::Copy),
            Key::Cut => Some(Self::Cut),
            Key::Paste => Some(Self::Paste),
            _ => None,
        }
    }
}

impl<S: Shell + 'static> SelectionHandler for Host<S> {
    type SelectionUserData = Arc<Clip>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        if ty == SelectionTarget::Clipboard {
            self.clipboard_changed(source, &seat);
        }
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let Some(bytes) = user_data.get(&mime_type).cloned() else {
            return;
        };
        let Some(guard) = SelectionTransferGuard::acquire() else {
            return;
        };
        if let Err(error) = std::thread::Builder::new()
            .name("mcsapi-selection-send".into())
            .spawn(move || {
                let _guard = guard;
                let _ = write_selection(fd, &bytes, SELECTION_WRITE_TIMEOUT);
            })
        {
            warn!(%error, "cannot start clipboard transfer");
        }
    }
}

impl<S: Shell + 'static> DataDeviceHandler for Host<S> {
    fn data_device_state(&self) -> &smithay::wayland::selection::data_device::DataDeviceState {
        &self.data_device_state
    }
}

/// A pipe for a transfer, its read end first.
fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    match io::pipe() {
        Ok((read, write)) => Some((read.into(), write.into())),
        Err(error) => {
            warn!(%error, "cannot open a pipe for the clipboard");
            None
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    // SAFETY: fcntl is called with a valid owned file descriptor.
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl is called with a valid owned file descriptor and its existing flags.
    if unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads every pipe to its end, all at once, since a client may write them
/// in any order and a full pipe would hold up the others. A type that is
/// not finished by the deadline is left out, and nothing is returned once
/// the total passes `limit`.
fn read_all(
    reads: Vec<(String, OwnedFd)>,
    timeout: Duration,
    limit: usize,
) -> Vec<(String, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let order: Vec<String> = reads.iter().map(|(mime, _)| mime.clone()).collect();
    let mut open: Vec<(String, File, Vec<u8>)> = reads
        .into_iter()
        .filter(|(_, fd)| set_nonblocking(fd).is_ok())
        .map(|(mime, fd)| (mime, File::from(fd), Vec::new()))
        .collect();
    let mut finished = Vec::new();
    let mut total = 0;
    let mut chunk = vec![0; 64 << 10];
    while !open.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut descriptors: Vec<libc::pollfd> = open
            .iter()
            .map(|(_, file, _)| libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: descriptors holds initialized pollfds for files that stay open.
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if ready == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        // Back to front, so removing one leaves the rest where they were.
        for i in (0..open.len()).rev() {
            if descriptors[i].revents == 0 {
                continue;
            }
            let (_, file, bytes) = &mut open[i];
            let ended = loop {
                match file.read(&mut chunk) {
                    Ok(0) => break true,
                    Ok(n) => {
                        total += n;
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break false,
                    Err(_) => break true,
                }
                if total > limit {
                    return Vec::new();
                }
            };
            if ended {
                let (mime, _, bytes) = open.remove(i);
                finished.push((mime, bytes));
            }
        }
    }
    // In the order they were asked for, which is the order of preference.
    finished.sort_by_key(|(mime, _)| order.iter().position(|m| m == mime));
    finished
}

pub(super) fn write_selection(fd: OwnedFd, text: &[u8], timeout: Duration) -> io::Result<()> {
    set_nonblocking(&fd)?;
    let raw_fd = fd.as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut remaining = text;
    while !remaining.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        // SAFETY: the byte slice is valid for the duration of this write call.
        let written = unsafe { libc::write(raw_fd, remaining.as_ptr().cast(), remaining.len()) };
        if written > 0 {
            remaining = &remaining[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }

        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let timeout_ms = remaining_time.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: raw_fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and the fd remains owned.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn clipboard_transfers_are_bounded() {
        let guards: Vec<_> = (0..MAX_SELECTION_TRANSFERS)
            .map(|_| SelectionTransferGuard::acquire().unwrap())
            .collect();
        assert!(SelectionTransferGuard::acquire().is_none());
        drop(guards);
        assert!(SelectionTransferGuard::acquire().is_some());
    }

    #[test]
    fn stalled_clipboard_write_times_out() {
        let (_read, write) = pipe().unwrap();
        let error =
            write_selection(write, &vec![0; 1024 * 1024], Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn reads_every_type_whatever_order_they_come_in() {
        let (read_a, write_a) = pipe().unwrap();
        let (read_b, write_b) = pipe().unwrap();
        // The second type is written first, and more than a pipe holds, so
        // reading the first to its end before starting the second would
        // never finish.
        let writer = std::thread::spawn(move || {
            File::from(write_b)
                .write_all(&vec![b'b'; 256 << 10])
                .unwrap();
            File::from(write_a).write_all(b"a").unwrap();
        });
        let read = read_all(
            vec![("a".into(), read_a), ("b".into(), read_b)],
            Duration::from_secs(5),
            MAX_KEPT_BYTES,
        );
        writer.join().unwrap();
        let mut names: Vec<_> = read.iter().map(|(m, b)| (m.as_str(), b.len())).collect();
        names.sort();
        assert_eq!(names, [("a", 1), ("b", 256 << 10)]);
    }

    #[test]
    fn a_silent_client_is_left_out_by_the_deadline() {
        let (read_a, write_a) = pipe().unwrap();
        let (read_b, _write_b) = pipe().unwrap();
        File::from(write_a).write_all(b"text").unwrap();
        let read = read_all(
            vec![("a".into(), read_a), ("b".into(), read_b)],
            Duration::from_millis(50),
            MAX_KEPT_BYTES,
        );
        assert_eq!(read, [("a".to_owned(), b"text".to_vec())]);
    }

    #[test]
    fn a_selection_over_the_limit_is_not_kept() {
        let (read, write) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = File::from(write).write_all(&vec![0; 1 << 20]);
        });
        assert!(read_all(vec![("a".into(), read)], Duration::from_secs(5), 1000).is_empty());
        writer.join().unwrap();
    }

    #[test]
    fn copied_text_is_offered_under_every_text_name() {
        let clip = Clip::text("héllo".into());
        assert!(clip.mime_types().contains(&"UTF8_STRING".to_owned()));
        assert_eq!(clip.as_text().as_deref(), Some("héllo"));
    }

    #[test]
    fn clipboard_keys_are_egui_winits() {
        let ctrl = egui::Modifiers::CTRL;
        let shift = egui::Modifiers::SHIFT;
        assert_eq!(
            ClipboardKey::of(egui::Key::C, ctrl),
            Some(ClipboardKey::Copy)
        );
        assert_eq!(
            ClipboardKey::of(egui::Key::V, ctrl),
            Some(ClipboardKey::Paste)
        );
        assert_eq!(
            ClipboardKey::of(egui::Key::Insert, shift),
            Some(ClipboardKey::Paste)
        );
        assert_eq!(
            ClipboardKey::of(egui::Key::Delete, shift),
            Some(ClipboardKey::Cut)
        );
        // Ctrl+Shift+C stays a key: a terminal's copy, which egui leaves to
        // the app.
        assert_eq!(ClipboardKey::of(egui::Key::C, ctrl | shift), None);
        assert_eq!(ClipboardKey::of(egui::Key::C, egui::Modifiers::NONE), None);
    }
}
