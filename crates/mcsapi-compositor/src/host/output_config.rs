//! The display, for tools from other projects:
//!
//! - `zwlr_output_manager_v1` (wlr-randr, kanshi, nwg-displays) lists the
//!   one output with its current mode, position, scale and adaptive sync.
//!   A configuration that keeps all of that succeeds; one that changes it
//!   fails, since the mode is the display's preferred one on the bare seat
//!   and the window's size nested, and there is no other output to place.
//! - `zwlr_output_power_manager_v1` (wlopm, swayidle) turns the display
//!   off and on: on the bare seat its planes are disabled and DPMS goes
//!   off; nested, the window keeps the last frame. No frame is drawn while
//!   it is off, so clients get no frame callbacks.
//! - `zwlr_gamma_control_manager_v1` (gammastep, wlsunset) sets the CRTC's
//!   gamma ramp on the bare seat, one client at a time, and puts the old
//!   ramp back when that client lets go. Nested there is no ramp to set,
//!   and the control fails at once.
//!
//! Sandboxed clients see none of them.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::Mutex,
};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::{
            gamma_control::v1::server::{
                zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
                zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
            },
            output_management::v1::server::{
                zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
                zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
                zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
                zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
                zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
            },
            output_power_management::v1::server::{
                zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
                zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
            },
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
            backend::ClientId, protocol::wl_output::Transform,
        },
    },
};
use tracing::warn;

#[cfg(feature = "kms")]
use super::Backend;
use super::{Host, security::unsandboxed};
use crate::Shell;

/// The mode an output-management client is told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    width: i32,
    height: i32,
    /// Millihertz.
    refresh: i32,
}

/// One output-management client's view of the output.
struct Head {
    manager: ZwlrOutputManagerV1,
    head: ZwlrOutputHeadV1,
    mode: ZwlrOutputModeV1,
}

pub(super) struct OutputConfig {
    heads: Vec<Head>,
    /// What clients were last told; a configuration for an older serial is
    /// cancelled.
    mode: Option<Mode>,
    vrr: bool,
    serial: u32,
    powers: Vec<ZwlrOutputPowerV1>,
    /// Whether the display is on.
    pub(super) powered: bool,
    gamma: Option<ZwlrGammaControlV1>,
}

impl OutputConfig {
    pub(super) fn new<S: Shell + 'static>(dh: &DisplayHandle) -> Self {
        dh.create_global::<Host<S>, ZwlrOutputManagerV1, ()>(4, ());
        dh.create_global::<Host<S>, ZwlrOutputPowerManagerV1, ()>(1, ());
        dh.create_global::<Host<S>, ZwlrGammaControlManagerV1, ()>(1, ());
        Self {
            heads: Vec::new(),
            mode: None,
            vrr: false,
            serial: 1,
            powers: Vec::new(),
            powered: true,
            gamma: None,
        }
    }
}

fn current_mode(output: &Output) -> Mode {
    let mode = output.current_mode();
    Mode {
        width: mode.map_or(0, |m| m.size.w),
        height: mode.map_or(0, |m| m.size.h),
        refresh: mode.map_or(0, |m| m.refresh),
    }
}

/// A mode object for `head`, sent its size and refresh.
fn announce_mode<S: Shell + 'static>(
    dh: &DisplayHandle,
    client: &Client,
    head: &ZwlrOutputHeadV1,
    mode: Mode,
) -> Option<ZwlrOutputModeV1> {
    let object = client
        .create_resource::<ZwlrOutputModeV1, _, Host<S>>(dh, head.version(), mode)
        .ok()?;
    head.mode(&object);
    object.size(mode.width, mode.height);
    if mode.refresh > 0 {
        object.refresh(mode.refresh);
    }
    object.preferred();
    head.current_mode(&object);
    Some(object)
}

fn adaptive_sync(vrr: bool) -> AdaptiveSyncState {
    if vrr {
        AdaptiveSyncState::Enabled
    } else {
        AdaptiveSyncState::Disabled
    }
}

impl<S: Shell + 'static> Host<S> {
    /// Tells output-management clients when the mode or adaptive sync
    /// changed (the nested window was resized, or moved to another
    /// monitor).
    pub(super) fn update_output_heads(&mut self) {
        let mode = current_mode(&self.output);
        let config = &mut self.output_config;
        if config.mode == Some(mode) && config.vrr == self.vrr {
            return;
        }
        let mode_changed = config.mode.is_some_and(|m| m != mode);
        config.mode = Some(mode);
        config.vrr = self.vrr;
        config.serial = config.serial.wrapping_add(1);
        let dh = self.display.clone();
        for head in &mut config.heads {
            let Some(client) = head.manager.client() else {
                continue;
            };
            if mode_changed && let Some(new) = announce_mode::<S>(&dh, &client, &head.head, mode) {
                std::mem::replace(&mut head.mode, new).finished();
            }
            if head.head.version() >= 4 {
                head.head.adaptive_sync(adaptive_sync(self.vrr));
            }
            head.manager.done(config.serial);
        }
    }

    /// Turns the display off or on.
    fn set_power(&mut self, on: bool) {
        if self.output_config.powered == on {
            return;
        }
        self.output_config.powered = on;
        // What the screen showed while off is not trusted to be there.
        self.dirty = true;
        #[cfg(feature = "kms")]
        if let Backend::Kms(k) = &mut self.backend {
            k.set_power(on);
        }
        let mode = if on {
            zwlr_output_power_v1::Mode::On
        } else {
            zwlr_output_power_v1::Mode::Off
        };
        for power in &self.output_config.powers {
            power.mode(mode);
        }
    }

    /// The display's gamma ramp size, where it has one.
    fn gamma_size(&self) -> Option<u32> {
        match &self.backend {
            #[cfg(feature = "kms")]
            Backend::Kms(k) => k.gamma_size(),
            _ => None,
        }
    }

    /// Sets the gamma ramp, or puts the original back with `None`.
    fn set_gamma(&mut self, ramp: Option<&[u16]>) -> std::io::Result<()> {
        match &mut self.backend {
            #[cfg(feature = "kms")]
            Backend::Kms(k) => k.set_gamma(ramp),
            _ => ramp.map_or(Ok(()), |_| Err(std::io::ErrorKind::Unsupported.into())),
        }
    }
}

impl<S: Shell + 'static> GlobalDispatch<ZwlrOutputManagerV1, ()> for Host<S> {
    fn bind(
        host: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let Ok(head) =
            client.create_resource::<ZwlrOutputHeadV1, _, Self>(dh, manager.version(), ())
        else {
            return;
        };
        manager.head(&head);
        let output = &host.output;
        head.name(output.name());
        head.description(output.description());
        let physical = output.physical_properties();
        if physical.size.w > 0 && physical.size.h > 0 {
            head.physical_size(physical.size.w, physical.size.h);
        }
        let mode = current_mode(output);
        let Some(mode_object) = announce_mode::<S>(dh, client, &head, mode) else {
            return;
        };
        head.enabled(1);
        head.position(0, 0);
        head.transform(Transform::Normal);
        head.scale(1.0);
        if head.version() >= 2 {
            head.make(physical.make);
            head.model(physical.model);
        }
        if head.version() >= 4 {
            head.adaptive_sync(adaptive_sync(host.vrr));
        }
        let config = &mut host.output_config;
        config.mode.get_or_insert(mode);
        config.vrr = host.vrr;
        manager.done(config.serial);
        config.heads.push(Head {
            manager,
            head,
            mode: mode_object,
        });
    }

    fn can_view(client: Client, _global_data: &()) -> bool {
        unsandboxed(&client)
    }
}

/// A configuration being built: its serial, the head's place in it
/// (`Some(None)` disabled, `Some(Some(_))` enabled with these changes), and
/// whether it was applied or tested already.
#[derive(Default)]
struct Pending {
    serial: u32,
    head: Option<Option<ZwlrOutputConfigurationHeadV1>>,
    used: bool,
}

/// What a configuration asks of the head; anything left unset keeps its
/// current value.
#[derive(Default)]
struct HeadChange {
    mode: Option<Mode>,
    position: Option<(i32, i32)>,
    transform: Option<WEnum<Transform>>,
    scale: Option<f64>,
    vrr: Option<bool>,
}

impl HeadChange {
    /// Whether this keeps the head as it is, the only configuration there
    /// is to apply.
    fn keeps(&self, mode: Mode, vrr: bool) -> bool {
        self.mode.is_none_or(|m| {
            (m.width, m.height) == (mode.width, mode.height)
                // Zero asks for any refresh; otherwise within a hertz.
                && (m.refresh == 0 || (m.refresh - mode.refresh).abs() < 1000)
        }) && self.position.is_none_or(|p| p == (0, 0))
            && self
                .transform
                .is_none_or(|t| t == WEnum::Value(Transform::Normal))
            && self.scale.is_none_or(|s| (s - 1.0).abs() < 0.001)
            && self.vrr.is_none_or(|v| v == vrr)
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputManagerV1, ()> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    Mutex::new(Pending {
                        serial,
                        ..Pending::default()
                    }),
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                let config = &mut host.output_config;
                if let Some(i) = config.heads.iter().position(|h| &h.manager == manager) {
                    let head = config.heads.remove(i);
                    head.mode.finished();
                    head.head.finished();
                }
                manager.finished();
            }
            _ => {}
        }
    }

    fn destroyed(host: &mut Self, _client: ClientId, manager: &ZwlrOutputManagerV1, _data: &()) {
        host.output_config.heads.retain(|h| &h.manager != manager);
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputHeadV1, ()> for Host<S> {
    fn request(
        _host: &mut Self,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Release is its only request.
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputModeV1, Mode> for Host<S> {
    fn request(
        _host: &mut Self,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &Mode,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Release is its only request.
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputConfigurationV1, Mutex<Pending>> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        configuration: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        pending: &Mutex<Pending>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
        let configured = |pending: &Pending| {
            if pending.head.is_some() {
                configuration.post_error(
                    zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                    "the head is already in this configuration",
                );
            }
            pending.head.is_some()
        };
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, .. } => {
                if configured(&pending) {
                    return;
                }
                let head = data_init.init(id, Mutex::new(HeadChange::default()));
                pending.head = Some(Some(head));
            }
            zwlr_output_configuration_v1::Request::DisableHead { .. } => {
                if configured(&pending) {
                    return;
                }
                pending.head = Some(None);
            }
            zwlr_output_configuration_v1::Request::Apply
            | zwlr_output_configuration_v1::Request::Test => {
                if std::mem::replace(&mut pending.used, true) {
                    configuration.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "the configuration was already applied or tested",
                    );
                    return;
                }
                if pending.serial != host.output_config.serial {
                    configuration.cancelled();
                    return;
                }
                let Some(head) = &pending.head else {
                    configuration.post_error(
                        zwlr_output_configuration_v1::Error::UnconfiguredHead,
                        "the head is not in this configuration",
                    );
                    return;
                };
                let mode = current_mode(&host.output);
                // The only display cannot be turned off this way (output
                // power does that), and nothing about it can change.
                let keeps = head
                    .as_ref()
                    .and_then(|h| h.data::<Mutex<HeadChange>>())
                    .is_some_and(|c| {
                        c.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .keeps(mode, host.vrr)
                    });
                if keeps {
                    configuration.succeeded();
                } else {
                    configuration.failed();
                }
            }
            _ => {}
        }
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputConfigurationHeadV1, Mutex<HeadChange>> for Host<S> {
    fn request(
        _host: &mut Self,
        _client: &Client,
        head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        change: &Mutex<HeadChange>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_output_configuration_head_v1::{Error, Request};
        let mut change = change.lock().unwrap_or_else(|e| e.into_inner());
        let set = |head: &ZwlrOutputConfigurationHeadV1, already: bool, error: Error| {
            if already {
                head.post_error(error, "already set in this configuration");
            }
            !already
        };
        match request {
            Request::SetMode { mode } => {
                if set(head, change.mode.is_some(), Error::AlreadySet) {
                    change.mode = mode.data::<Mode>().copied();
                }
            }
            Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if width <= 0 || height <= 0 || refresh < 0 {
                    head.post_error(Error::InvalidCustomMode, "invalid custom mode");
                } else if set(head, change.mode.is_some(), Error::AlreadySet) {
                    change.mode = Some(Mode {
                        width,
                        height,
                        refresh,
                    });
                }
            }
            Request::SetPosition { x, y } => {
                if set(head, change.position.is_some(), Error::AlreadySet) {
                    change.position = Some((x, y));
                }
            }
            Request::SetTransform { transform } => {
                if set(head, change.transform.is_some(), Error::AlreadySet) {
                    change.transform = Some(transform);
                }
            }
            Request::SetScale { scale } => {
                if scale <= 0.0 {
                    head.post_error(Error::InvalidScale, "invalid scale");
                } else if set(head, change.scale.is_some(), Error::AlreadySet) {
                    change.scale = Some(scale);
                }
            }
            Request::SetAdaptiveSync { state }
                if set(head, change.vrr.is_some(), Error::AlreadySet) =>
            {
                change.vrr = Some(state == WEnum::Value(AdaptiveSyncState::Enabled));
            }
            _ => {}
        }
    }
}

impl<S: Shell + 'static> GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for Host<S> {
    fn bind(
        _host: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _global_data: &()) -> bool {
        unsandboxed(&client)
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputPowerManagerV1, ()> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        _manager: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // There is one output, so the output argument needs no lookup.
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, .. } = request {
            let power = data_init.init(id, ());
            power.mode(if host.output_config.powered {
                zwlr_output_power_v1::Mode::On
            } else {
                zwlr_output_power_v1::Mode::Off
            });
            host.output_config.powers.push(power);
        }
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrOutputPowerV1, ()> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        power: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_output_power_v1::Request::SetMode { mode } = request {
            match mode {
                WEnum::Value(mode) => host.set_power(mode == zwlr_output_power_v1::Mode::On),
                WEnum::Unknown(_) => power.post_error(
                    zwlr_output_power_v1::Error::InvalidMode,
                    "unknown power mode",
                ),
            }
        }
    }

    fn destroyed(host: &mut Self, _client: ClientId, power: &ZwlrOutputPowerV1, _data: &()) {
        host.output_config.powers.retain(|p| p != power);
    }
}

impl<S: Shell + 'static> GlobalDispatch<ZwlrGammaControlManagerV1, ()> for Host<S> {
    fn bind(
        _host: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _global_data: &()) -> bool {
        unsandboxed(&client)
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrGammaControlManagerV1, ()> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, .. } = request {
            let control = data_init.init(id, ());
            // One client at a time, and only where there is a ramp.
            match host.gamma_size() {
                Some(size) if host.output_config.gamma.is_none() => {
                    control.gamma_size(size);
                    host.output_config.gamma = Some(control);
                }
                _ => control.failed(),
            }
        }
    }
}

impl<S: Shell + 'static> Dispatch<ZwlrGammaControlV1, ()> for Host<S> {
    fn request(
        host: &mut Self,
        _client: &Client,
        control: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };
        if host.output_config.gamma.as_ref() != Some(control) {
            return;
        }
        let Some(size) = host.gamma_size() else {
            return;
        };
        // Three ramps of `size` 16-bit entries, from the file's start.
        let mut bytes = vec![0; 3 * size as usize * 2];
        let mut file = File::from(fd);
        let read = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.read_exact(&mut bytes));
        if read.is_err() {
            control.post_error(
                zwlr_gamma_control_v1::Error::InvalidGamma,
                "the gamma ramps could not be read",
            );
            return;
        }
        let ramp: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_ne_bytes(b))
            .collect();
        if let Err(e) = host.set_gamma(Some(&ramp)) {
            warn!(error = %e, "cannot set the gamma ramp");
            host.output_config.gamma = None;
            control.failed();
        }
    }

    fn destroyed(host: &mut Self, _client: ClientId, control: &ZwlrGammaControlV1, _data: &()) {
        if host.output_config.gamma.as_ref() == Some(control) {
            host.output_config.gamma = None;
            if let Err(e) = host.set_gamma(None) {
                warn!(error = %e, "cannot put the gamma ramp back");
            }
        }
    }
}
