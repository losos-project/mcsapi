//! Running on the bare seat: DRM/KMS output, libinput input, and seat access
//! through libseat (logind, or seatd).
//!
//! The nested backend draws into a window of another session. This one is
//! for when there is no other session: a display manager's greeter, or a
//! desktop started from a login. It drives every connected display at its
//! preferred mode through GBM buffers, and gives up the GPU and input
//! devices whenever the seat moves to another VT.
//!
//! The first display connected is the session's output: its mode is the
//! size clients and the shell lay out for, and its vblanks pace and time
//! the frames. Every other display shows the same frame, scaled to fit with
//! its aspect kept and the rest black: the displays mirror one another,
//! they do not extend the desktop. A display plugged in later lights up at
//! the next frame, one unplugged goes, and when the output's own display
//! goes, the next one becomes the output and the session takes its size
//! (`rescan`, on udev's change events for the GPU).
//!
//! Frames are triple buffered: one buffer is on screen, one may wait for
//! its page flip, and one more may be queued behind it, which Smithay's
//! surface flips at the vblank that shows the one before. Drawing is paced
//! by a timer at the refresh rate, not by the vblank, so with only one
//! frame allowed past the screen a tick that lands just before the vblank
//! finds the flip still pending and the frame is dropped; on a GPU that
//! takes most of a refresh to draw, every other one is. The cost is a
//! frame of latency when the GPU is ahead, and one more scanout buffer.
//!
//! The frame is drawn exactly as for the nested window, into an offscreen
//! texture, and then copied onto each display's scanout buffer. The chrome and the blur
//! are painted with raw GL, which puts row 0 at the bottom as a window
//! surface does, while a scanout buffer shows row 0 at the top; drawing them
//! straight into it would turn them upside down under the client windows.
//! One extra full-screen copy keeps every layer of the frame identical
//! between the two backends.
//!
//! Clients that draw on the GPU hand over their buffers as dma-bufs
//! (`zwp_linux_dmabuf_v1`), which are only offered here: the nested window
//! has no DRM device to name in the feedback. Where the device can wait on
//! a syncobj through an eventfd, clients may also pass explicit fences
//! (`wp_linux_drm_syncobj_v1`). NVIDIA's EGL Wayland platform needs the
//! first to present at all, and uses the second where it can.
//!
//! The vblank that puts a frame on screen also carries the kernel's
//! timestamp and counter, which `wp_presentation` reports to the clients
//! whose commits the frame showed (see `host::timing`).

use std::{collections::VecDeque, io, path::PathBuf};

use smithay::{
    backend::{
        allocator::{
            Format, Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEventMetadata, DrmEventTime, DrmNode,
            GbmBufferedSurface, NodeType,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind, Color32F, Frame, ImportDma, Renderer, Texture,
            element::surface::WaylandSurfaceRenderElement,
            gles::{GlesRenderer, GlesTexture},
        },
        session::{Session, libseat::LibSeatSession, libseat::LibSeatSessionNotifier},
        udev::{self, UdevBackend},
    },
    desktop::utils::OutputPresentationFeedback,
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind,
    reexports::{
        drm::control::{Device as _, Mode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Monotonic, Physical, Rectangle, Size, Time, Transform},
    wayland::{
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
        drm_syncobj::supports_syncobj_eventfd,
    },
};
use tracing::{error, info, warn};

/// Frames past the one on screen: one waiting for its flip and one queued
/// behind it, which is triple buffering. Smithay's surface holds no more.
const QUEUED_FRAMES: usize = 2;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The GPU, displays and seat of a session on the bare seat.
pub(crate) struct Kms {
    pub(crate) session: LibSeatSession,
    pub(crate) drm: DrmDevice,
    pub(crate) renderer: GlesRenderer,
    pub(crate) libinput: Libinput,
    /// The node clients should allocate their buffers on: the GPU's render
    /// node, else (a display-only device) the node this session opened.
    node: DrmNode,
    /// What a display plugged in later gets its scanout buffers from.
    allocator: GbmAllocator<DrmDeviceFd>,
    formats: FormatSet,
    /// The displays driven, the output's first (see the module's note).
    displays: Vec<Display>,
    /// The output's size and refresh rate, kept while no display is
    /// connected, which draws nothing until one is.
    size: Size<i32, Physical>,
    refresh_mhz: u32,
    /// Client surfaces drawn into the frame being drawn. Each holds its
    /// client's buffer, and Smithay releases a buffer (`wl_buffer.release`,
    /// and the syncobj release point) as soon as nothing holds it, which is
    /// when the CPU is done with it, not the GPU. They move to the output's
    /// `queued` with their frame, and are let go at the vblank that puts it
    /// on screen: its scanout buffer waited on a fence put after the
    /// sampling on the same context, so the GPU is done with them then. The
    /// other displays copy the finished frame, not the clients' buffers.
    /// Without this a client could draw into a buffer the GPU still reads:
    /// harmless where the kernel orders the two through the dma-buf's
    /// implicit fences, torn frames where nothing does.
    pub(crate) in_flight: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    /// Whether this session holds the seat (it is on the active VT).
    pub(crate) active: bool,
    /// Whether the displays are on: `zwlr_output_power_v1` turns them off.
    powered: bool,
}

/// One display: its connector, the CRTC scanning out to it at its mode, and
/// the surface its frames go through.
struct Display {
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    /// The frames handed to the surface whose vblank has not come yet,
    /// oldest first. The first waits for its page flip, a second for the
    /// vblank that shows the first.
    queued: VecDeque<Queued>,
    /// The gamma ramp a `zwlr_gamma_control_v1` client replaced, to put
    /// back when it lets go: red, then green, then blue.
    replaced_gamma: Option<Vec<u16>>,
}

/// A frame handed to a display's surface: the client surfaces it drew, and
/// their presentation feedback, on the output's display only. Dropping it
/// discards the feedback.
struct Queued {
    elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    feedback: Option<OutputPresentationFeedback>,
}

/// Event sources the host puts on its loop.
pub(crate) struct Sources {
    pub(crate) session: LibSeatSessionNotifier,
    pub(crate) drm: DrmDeviceNotifier,
    pub(crate) input: LibinputInputBackend,
    /// Displays plugged in and out: the GPU's change events.
    pub(crate) udev: UdevBackend,
}

impl Kms {
    /// Takes the seat, the primary GPU and its connected displays.
    pub(crate) fn open() -> Result<(Self, Sources)> {
        let (mut session, session_notifier) = LibSeatSession::new()
            .map_err(|e| format!("cannot take the seat (is this a logind session?): {e}"))?;
        let seat = session.seat();

        let path = gpu(&seat)?;
        let fd = session
            .open(
                &path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let node = DrmNode::from_file(&fd)?;
        let node = node
            .node_with_type(NodeType::Render)
            .and_then(std::result::Result::ok)
            .unwrap_or(node);
        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;
        // SAFETY: the GBM device lives as long as the display, which keeps a
        // clone of it.
        let egl = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&egl)?;
        let formats = context.dmabuf_render_formats().clone();
        // SAFETY: the context is new and used by nothing else.
        let renderer = unsafe { GlesRenderer::new(context)? };
        let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);

        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat)
            .map_err(|()| format!("libinput cannot use seat {seat}"))?;
        let input = LibinputInputBackend::new(libinput.clone());
        let udev = UdevBackend::new(&seat)?;

        let active = session.is_active();
        let mut kms = Self {
            session,
            drm,
            renderer,
            libinput,
            node,
            allocator,
            formats,
            displays: Vec::new(),
            size: (0, 0).into(),
            refresh_mhz: 60_000,
            in_flight: Vec::new(),
            active,
            powered: true,
        };
        kms.rescan();
        if kms.displays.is_empty() {
            return Err("no connected display".into());
        }
        Ok((
            kms,
            Sources {
                session: session_notifier,
                drm: drm_notifier,
                input,
                udev,
            },
        ))
    }

    /// Follows the displays connected now: drives each new one, lets go of
    /// each one gone, and makes the first one left the output if the
    /// output's own display went. Only while the seat is ours; `resume`
    /// looks again.
    pub(crate) fn rescan(&mut self) {
        if !self.active {
            return;
        }
        let connected = match connected(&self.drm) {
            Ok(connected) => connected,
            Err(e) => {
                warn!(error = %e, "cannot list the displays");
                return;
            }
        };
        let output = self.displays.first().map(|d| d.connector);
        self.displays.retain(|d| {
            let kept = connected.iter().any(|c| c.connector == d.connector);
            if !kept {
                info!(connector = ?d.connector, "display unplugged");
            }
            kept
        });
        for found in connected {
            if self.displays.iter().any(|d| d.connector == found.connector) {
                continue;
            }
            let Some(&crtc) = found
                .crtcs
                .iter()
                .find(|crtc| self.displays.iter().all(|d| d.crtc != **crtc))
            else {
                warn!(connector = ?found.connector, "no CRTC left to drive this display");
                continue;
            };
            match self.surface(crtc, found.mode, found.connector) {
                Ok(surface) => {
                    let (w, h) = found.mode.size();
                    info!(connector = ?found.connector, w, h, "display connected");
                    self.displays.push(Display {
                        connector: found.connector,
                        crtc,
                        mode: found.mode,
                        surface,
                        queued: VecDeque::new(),
                        replaced_gamma: None,
                    });
                }
                Err(e) => {
                    warn!(connector = ?found.connector, error = %e, "cannot drive this display")
                }
            }
        }
        if let Some(first) = self.displays.first() {
            if output != Some(first.connector) {
                info!(connector = ?first.connector, "the output's display");
            }
            let (w, h) = first.mode.size();
            self.size = (i32::from(w), i32::from(h)).into();
            self.refresh_mhz = refresh_mhz(&first.mode);
        }
    }

    /// A surface scanning out through `crtc` to `connector` at `mode`.
    ///
    /// The scanout buffers are allocated with a modifier both the plane and
    /// the renderer list, and Smithay test-commits one before taking it. A
    /// driver can still list a modifier its display engine refuses for this
    /// mode, so, as Smithay's own `DrmOutputManager` does, try again with the
    /// implicit modifier only, leaving the layout to the driver's GBM, which
    /// knows the buffer is for scanout.
    fn surface(
        &mut self,
        crtc: crtc::Handle,
        mode: Mode,
        connector: connector::Handle,
    ) -> Result<GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>> {
        match GbmBufferedSurface::new(
            self.drm.create_surface(crtc, mode, &[connector])?,
            self.allocator.clone(),
            COLOR_FORMATS,
            self.formats.iter().copied(),
        ) {
            Ok(surface) => Ok(surface),
            Err(e) => {
                warn!(error = %e, "no scanout buffer with explicit modifiers, trying implicit");
                Ok(GbmBufferedSurface::new(
                    self.drm.create_surface(crtc, mode, &[connector])?,
                    self.allocator.clone(),
                    COLOR_FORMATS,
                    implicit(self.formats.iter().copied()),
                )?)
            }
        }
    }

    /// The output's size in pixels.
    pub(crate) fn size(&self) -> Size<i32, Physical> {
        self.size
    }

    /// The output's refresh rate in millihertz.
    pub(crate) fn refresh_mhz(&self) -> u32 {
        self.refresh_mhz
    }

    /// Whether a frame may be drawn now: the seat is ours and the output's
    /// surface can take another frame (see the module's note on triple
    /// buffering). The other displays take it if they can.
    pub(crate) fn can_draw(&self) -> bool {
        self.active
            && self.powered
            && self
                .displays
                .first()
                .is_some_and(|d| d.queued.len() < QUEUED_FRAMES)
    }

    /// Copies the frame drawn offscreen, `scene`, to every display, turned
    /// by `transform` (see `host::OFFSCREEN_COPY`), with the feedback for
    /// when the output's display shows it. A display that cannot take
    /// another frame yet skips this one, and one that fails to is let be;
    /// only the output's failing is an error.
    pub(crate) fn present(
        &mut self,
        scene: &GlesTexture,
        transform: Transform,
        feedback: Option<OutputPresentationFeedback>,
    ) -> Result {
        let mut feedback = feedback;
        for (i, display) in self.displays.iter_mut().enumerate() {
            let output = i == 0;
            if !output && display.queued.len() >= QUEUED_FRAMES {
                continue;
            }
            match copy(&mut self.renderer, display, scene, transform) {
                Ok(()) => display.queued.push_back(if output {
                    Queued {
                        elements: std::mem::take(&mut self.in_flight),
                        feedback: feedback.take(),
                    }
                } else {
                    Queued {
                        elements: Vec::new(),
                        feedback: None,
                    }
                }),
                Err(e) if output => return Err(e),
                Err(e) => {
                    let connector = display.connector;
                    warn!(?connector, error = %e, "cannot show the frame");
                }
            }
        }
        Ok(())
    }

    /// The seat moved to another VT: let go of the GPU and input devices.
    pub(crate) fn pause(&mut self) {
        self.active = false;
        self.libinput.suspend();
        self.drm.pause();
    }

    /// The seat came back: take the devices again, follow any display
    /// plugged in or out while away, and redraw everything.
    pub(crate) fn resume(&mut self) {
        self.active = true;
        if let Err(e) = self.drm.activate(false) {
            error!(error = %e, "cannot take the display back");
        }
        if self.libinput.resume().is_err() {
            error!("cannot take the input devices back");
        }
        for display in &mut self.displays {
            // Buffers queued before the switch never flipped; start clean.
            display.surface.reset_buffers();
            // Their vblank will not come either. The GPU finished those
            // frames while the seat was away, so their clients' buffers can
            // go back.
            display.queued.clear();
        }
        self.in_flight.clear();
        self.rescan();
    }

    /// A vblank on `crtc`: its queued buffer is on screen. `now` stands in
    /// for a timestamp the kernel took on another clock.
    pub(crate) fn vblank(
        &mut self,
        crtc: crtc::Handle,
        metadata: Option<DrmEventMetadata>,
        now: Time<Monotonic>,
    ) {
        let Some(i) = self.displays.iter().position(|d| d.crtc == crtc) else {
            return;
        };
        let refresh_mhz = self.refresh_mhz;
        let display = &mut self.displays[i];
        // The frame waiting for this flip is on screen, so the GPU has read
        // every client buffer it drew (see `in_flight`). Smithay then flips
        // the frame queued behind it, if there is one.
        match display.surface.frame_submitted() {
            Ok(Some(())) => {
                let Some(mut feedback) = display.queued.pop_front().and_then(|q| q.feedback) else {
                    return;
                };
                let (time, seq, clock) = match metadata {
                    Some(DrmEventMetadata {
                        time: DrmEventTime::Monotonic(time),
                        sequence,
                    }) => (time.into(), sequence, Kind::HwClock),
                    Some(DrmEventMetadata { sequence, .. }) => (now, sequence, Kind::empty()),
                    None => (now, 0, Kind::empty()),
                };
                feedback.presented(
                    time,
                    crate::host::refresh(refresh_mhz, false),
                    u64::from(seq),
                    Kind::Vsync | Kind::HwCompletion | clock,
                );
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "page flip failed");
                display.queued.pop_front();
                // The queued frame's flip is what failed, and the surface
                // dropped it. The GPU may still be drawing it, so its
                // client buffers ride with the next frame instead.
                if let Some(lost) = display.queued.pop_front() {
                    self.in_flight.extend(lost.elements);
                }
            }
        }
    }

    /// Turns the displays off, their planes disabled and DPMS off, or back
    /// on, which happens with the next frame's commit.
    pub(crate) fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        if !on {
            for display in &mut self.displays {
                if let Err(e) = display.surface.surface().clear() {
                    warn!(error = %e, "cannot turn the display off");
                }
                // No vblank comes for frames queued before, as on a VT
                // switch.
                display.surface.reset_buffers();
                display.queued.clear();
            }
        }
    }

    /// Entries per channel in the output's gamma ramp; none where its CRTC
    /// has no ramp.
    pub(crate) fn gamma_size(&self) -> Option<u32> {
        self.displays
            .first()
            .and_then(|d| crtc_gamma_size(&self.drm, d.crtc))
    }

    /// Sets the gamma ramp: `gamma_size` entries of red, then green, then
    /// blue, on every display, each with as many entries as its own ramp
    /// has. `None` puts back the ramps the first one replaced.
    pub(crate) fn set_gamma(&mut self, ramp: Option<&[u16]>) -> io::Result<()> {
        let Some(n) = self.gamma_size().map(|n| n as usize) else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        if ramp.is_some_and(|ramp| ramp.len() != 3 * n) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        for (i, display) in self.displays.iter_mut().enumerate() {
            let result = set_crtc_gamma(&self.drm, display, ramp);
            match result {
                Err(e) if i == 0 => return Err(e),
                Err(e) => {
                    let connector = display.connector;
                    warn!(?connector, error = %e, "cannot set the gamma ramp");
                }
                Ok(()) => {}
            }
        }
        Ok(())
    }

    /// The `zwp_linux_dmabuf_v1` feedback for every surface: the buffers
    /// this renderer can sample from, allocated on the GPU's render node.
    /// Every client surface is composited into the offscreen frame, none
    /// is scanned out directly, so there is no scanout tranche to offer.
    pub(crate) fn dmabuf_feedback(&self) -> io::Result<DmabufFeedback> {
        DmabufFeedbackBuilder::new(self.node.dev_id(), self.renderer.dmabuf_formats()).build()
    }

    /// The device to import clients' syncobj timelines on, when it can
    /// signal a timeline point through an eventfd: without that a commit
    /// cannot wait for its acquire point, so explicit sync is not offered.
    /// It needs kernel 6.6 or newer and a driver with timeline syncobjs;
    /// virtio-gpu and the software drivers may lack them.
    pub(crate) fn syncobj_device(&self) -> Option<DrmDeviceFd> {
        let fd = self.drm.device_fd();
        supports_syncobj_eventfd(fd).then(|| fd.clone())
    }

    /// Ctrl+Alt+F1–F12.
    pub(crate) fn change_vt(&mut self, vt: i32) {
        if let Err(e) = self.session.change_vt(vt) {
            warn!(vt, error = %e, "cannot switch VT");
        }
    }
}

/// Copies `scene` onto the next scanout buffer of `display`, scaled to fit
/// with its aspect kept, and queues the buffer for the next vblank.
fn copy(
    renderer: &mut GlesRenderer,
    display: &mut Display,
    scene: &GlesTexture,
    transform: Transform,
) -> Result {
    let (w, h) = display.mode.size();
    let size = Size::<i32, Physical>::from((i32::from(w), i32::from(h)));
    let full = Rectangle::from_size(size);
    let scene_size = scene.size();
    let dst = fit((scene_size.w, scene_size.h), (size.w, size.h));
    let (mut dmabuf, _age) = display.surface.next_buffer()?;
    let mut framebuffer = renderer.bind(&mut dmabuf)?;
    let mut frame = renderer.render(&mut framebuffer, size, Transform::Normal)?;
    // The bars beside or under a frame of another shape.
    if dst != full {
        frame.clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[full])?;
    }
    Frame::render_texture_from_to(
        &mut frame,
        scene,
        Rectangle::from_size(scene_size).to_f64(),
        dst,
        // Relative to `dst`.
        &[Rectangle::from_size(dst.size)],
        &[],
        transform,
        1.0,
    )?;
    let sync = frame.finish()?;
    drop(framebuffer);
    // The fence goes with the buffer: the flip waits on it in the kernel
    // (`IN_FENCE_FD`), or Smithay waits on it here where the plane takes no
    // fence. Nothing relies on the driver ordering the flip after the
    // drawing by itself.
    display
        .surface
        .queue_buffer(Some(sync), Some(vec![full]), ())?;
    Ok(())
}

/// Where a frame of size `frame` goes on a display of size `screen`: as
/// large as fits with its aspect kept, centred.
fn fit(frame: (i32, i32), screen: (i32, i32)) -> Rectangle<i32, Physical> {
    let (fw, fh) = (i64::from(frame.0.max(1)), i64::from(frame.1.max(1)));
    let (sw, sh) = (i64::from(screen.0), i64::from(screen.1));
    // Full width if the frame is the wider of the two, else full height.
    let (w, h) = if sw * fh <= sh * fw {
        (sw, sw * fh / fw)
    } else {
        (sh * fw / fh, sh)
    };
    Rectangle::new(
        (((sw - w) / 2) as i32, ((sh - h) / 2) as i32).into(),
        (w as i32, h as i32).into(),
    )
}

fn crtc_gamma_size(drm: &DrmDevice, crtc: crtc::Handle) -> Option<u32> {
    drm.get_crtc(crtc)
        .ok()
        .map(|c| c.gamma_length())
        .filter(|&n| n > 0)
}

/// Sets `display`'s gamma ramp from `ramp`, resampled to its own length,
/// or puts back the one it replaced.
fn set_crtc_gamma(drm: &DrmDevice, display: &mut Display, ramp: Option<&[u16]>) -> io::Result<()> {
    let Some(n) = crtc_gamma_size(drm, display.crtc).map(|n| n as usize) else {
        return Err(io::ErrorKind::Unsupported.into());
    };
    let ramp = match ramp {
        Some(ramp) => {
            if display.replaced_gamma.is_none() {
                let mut old = vec![0; 3 * n];
                let (red, rest) = old.split_at_mut(n);
                let (green, blue) = rest.split_at_mut(n);
                drm.get_gamma(display.crtc, red, green, blue)?;
                display.replaced_gamma = Some(old);
            }
            resample(ramp, n)
        }
        None => match display.replaced_gamma.take() {
            Some(old) => old,
            None => return Ok(()),
        },
    };
    let (red, rest) = ramp.split_at(n);
    let (green, blue) = rest.split_at(n);
    drm.set_gamma(display.crtc, red, green, blue)
}

/// A ramp of three channels, each `ramp.len() / 3` entries, as three of `n`
/// entries each, by the nearest entry.
fn resample(ramp: &[u16], n: usize) -> Vec<u16> {
    let from = ramp.len() / 3;
    if from == n || from == 0 {
        return ramp.to_vec();
    }
    (0..3)
        .flat_map(|channel| {
            let channel = &ramp[channel * from..(channel + 1) * from];
            (0..n).map(move |i| channel[(i * (from - 1) + (n - 1) / 2) / (n - 1).max(1)])
        })
        .collect()
}

/// The scanout formats to try, in order.
const COLOR_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];

/// The formats among `formats` that leave the layout to the driver.
fn implicit(formats: impl IntoIterator<Item = Format>) -> impl Iterator<Item = Format> {
    formats
        .into_iter()
        .filter(|f| f.modifier == Modifier::Invalid)
}

/// The seat's primary GPU, else its first.
fn gpu(seat: &str) -> Result<PathBuf> {
    if let Some(path) = udev::primary_gpu(seat)? {
        return Ok(path);
    }
    udev::all_gpus(seat)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no GPU on seat {seat}").into())
}

fn refresh_mhz(mode: &Mode) -> u32 {
    refresh_from_timings(
        mode.clock(),
        mode.hsync().2,
        mode.vsync().2,
        mode.vrefresh(),
    )
}

/// The refresh rate in millihertz from a mode's pixel clock (kHz) and
/// totals; `vrefresh` is rounded to whole hertz, the timings are exact.
fn refresh_from_timings(clock_khz: u32, htotal: u16, vtotal: u16, vrefresh: u32) -> u32 {
    let total = u64::from(htotal) * u64::from(vtotal);
    if total == 0 {
        return vrefresh.max(1) * 1000;
    }
    (u64::from(clock_khz) * 1_000_000 / total) as u32
}

/// A connected display: its connector, its preferred mode (else its first),
/// and the CRTCs that can drive it.
struct Connected {
    connector: connector::Handle,
    mode: Mode,
    crtcs: Vec<crtc::Handle>,
}

/// Every connected display, in the order the device lists the connectors.
fn connected(drm: &DrmDevice) -> Result<Vec<Connected>> {
    let resources = drm.resource_handles()?;
    let mut found = Vec::new();
    for &handle in resources.connectors() {
        // Probed, not cached: a display just plugged in has no modes yet in
        // the kernel's last answer.
        let Ok(info) = drm.get_connector(handle, true) else {
            continue;
        };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        let mode = *info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(&info.modes()[0]);
        let mut crtcs = Vec::new();
        for encoder in info.encoders() {
            let Ok(encoder) = drm.get_encoder(*encoder) else {
                continue;
            };
            for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
        }
        if !crtcs.is_empty() {
            found.push(Connected {
                connector: handle,
                mode,
                crtcs,
            });
        }
    }
    Ok(found)
}

/// Whether to run on the bare seat: `MCSAPI_BACKEND=kms` or `=winit` decides;
/// otherwise when there is no Wayland or X11 session to nest in.
pub(crate) fn wanted() -> bool {
    match std::env::var("MCSAPI_BACKEND").as_deref() {
        Ok("kms" | "drm") => true,
        Ok(_) => false,
        Err(_) => {
            std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_from_the_mode_timings() {
        // 1920x1080@60 (CEA): 148.5 MHz over 2200 x 1125.
        assert_eq!(refresh_from_timings(148_500, 2200, 1125, 60), 60_000);
        // 59.94 Hz shows up as 59 or 60 in vrefresh, exactly here.
        assert_eq!(refresh_from_timings(148_352, 2200, 1125, 60), 59_940);
        assert_eq!(refresh_from_timings(0, 0, 0, 75), 75_000);
    }

    #[test]
    fn a_frame_fits_a_display_of_another_shape() {
        // The same shape fills it.
        assert_eq!(
            fit((1920, 1080), (1280, 720)),
            Rectangle::new((0, 0).into(), (1280, 720).into())
        );
        // 16:9 on 4:3: bars above and below.
        assert_eq!(
            fit((1920, 1080), (1024, 768)),
            Rectangle::new((0, 96).into(), (1024, 576).into())
        );
        // 4:3 on 16:9: bars at the sides.
        assert_eq!(
            fit((1024, 768), (1920, 1080)),
            Rectangle::new((240, 0).into(), (1440, 1080).into())
        );
    }

    #[test]
    fn a_gamma_ramp_is_resampled_to_each_display() {
        let ramp: Vec<u16> = [0, 100, 200, 300].repeat(3);
        assert_eq!(resample(&ramp, 4), ramp);
        assert_eq!(resample(&ramp, 2), [0, 300].repeat(3));
        let up = resample(&ramp, 7);
        assert_eq!(up.len(), 21);
        assert_eq!((up[0], up[6]), (0, 300));
    }
}
