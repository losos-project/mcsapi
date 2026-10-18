//! Hot-patching a shell while its nested session keeps running.

use std::{
    ops::{Deref, DerefMut},
    time::Duration,
};

use mcsapi::WindowId;

use crate::{
    Blur, Capture, ClientRequest, Command, KeyInput, KeyRoute, OutputTiming, Placement, Press,
    Shell, TextField, Theme, WindowHint, WindowPictures, WorkspaceInfo, a11y, accesskit, egui,
};

/// Calls a shell method through Subsecond's jump table when the `hotpatch`
/// feature is on, so the newest patched version runs. Release builds and
/// builds without the feature call it directly.
macro_rules! hot {
    ($method:path, $($arg:expr),+ $(,)?) => {{
        #[cfg(feature = "hotpatch")]
        {
            subsecond::HotFn::current($method).call(($($arg,)+))
        }
        #[cfg(not(feature = "hotpatch"))]
        {
            $method($($arg),+)
        }
    }};
}

/// A [`Shell`] whose methods are hot-patched in debug builds.
///
/// Wrap the shell with it and run the binary under `dx serve --hot-patch`
/// with the `hotpatch` feature: editing a `Shell` method in the binary's own
/// crate (an example or `main.rs`) swaps in the new code on save, while the
/// nested window, its Wayland clients and the shell's state stay alive.
/// Changes to a struct's fields, or to other crates, still need a restart.
///
/// Without the feature, or in release builds, every call goes straight to
/// the inner shell.
///
/// ```no_run
/// # use mcsapi::WindowId;
/// # use mcsapi_compositor::{Compositor, Hot, Placement, Shell};
/// # struct Tiling;
/// # impl Shell for Tiling {
/// #     fn map_window(&mut self, _: &str, _: &str) -> WindowId { WindowId::new(1).unwrap() }
/// #     fn unmap_window(&mut self, _: WindowId) {}
/// #     fn set_output(&mut self, _: (i32, i32)) {}
/// #     fn focused(&self) -> Option<WindowId> { None }
/// #     fn placements(&self) -> Vec<Placement> { Vec::new() }
/// # }
/// Compositor::new(Hot(Tiling)).run()?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Hot<S>(pub S);

impl<S> Deref for Hot<S> {
    type Target = S;

    fn deref(&self) -> &S {
        &self.0
    }
}

impl<S> DerefMut for Hot<S> {
    fn deref_mut(&mut self) -> &mut S {
        &mut self.0
    }
}

impl<S: Shell> Shell for Hot<S> {
    fn map_window(&mut self, app_id: &str, title: &str) -> WindowId {
        hot!(S::map_window, &mut self.0, app_id, title)
    }

    fn unmap_window(&mut self, window: WindowId) {
        hot!(S::unmap_window, &mut self.0, window)
    }

    fn set_output(&mut self, size: (i32, i32)) {
        hot!(S::set_output, &mut self.0, size)
    }

    fn focused(&self) -> Option<WindowId> {
        hot!(S::focused, &self.0)
    }

    fn placements(&self) -> Vec<Placement> {
        hot!(S::placements, &self.0)
    }

    fn set_title(&mut self, window: WindowId, title: &str) {
        hot!(S::set_title, &mut self.0, window, title)
    }

    fn set_app_id(&mut self, window: WindowId, app_id: &str) {
        hot!(S::set_app_id, &mut self.0, window, app_id)
    }

    fn window_sandbox(&mut self, window: WindowId, sandbox: &crate::Sandbox) {
        hot!(S::window_sandbox, &mut self.0, window, sandbox)
    }

    fn focus(&mut self, window: WindowId) {
        hot!(S::focus, &mut self.0, window)
    }

    fn session_started(&mut self, wayland_display: &str) {
        hot!(S::session_started, &mut self.0, wayland_display)
    }

    fn tick(&mut self) {
        hot!(S::tick, &mut self.0)
    }

    fn needs_frame(&mut self) -> bool {
        hot!(S::needs_frame, &mut self.0)
    }

    fn chrome_wants_pointer(&self, at: (i32, i32)) -> bool {
        hot!(S::chrome_wants_pointer, &self.0, at)
    }

    fn pointer_down(&mut self, at: (i32, i32), time_ms: u64) -> Press {
        hot!(S::pointer_down, &mut self.0, at, time_ms)
    }

    fn pointer_motion(&mut self, at: (i32, i32)) {
        hot!(S::pointer_motion, &mut self.0, at)
    }

    fn pointer_up(&mut self) {
        hot!(S::pointer_up, &mut self.0)
    }

    fn key(&mut self, key: &KeyInput) -> KeyRoute {
        hot!(S::key, &mut self.0, key)
    }

    fn gesture(&mut self, event: &crate::GestureEvent) -> bool {
        hot!(S::gesture, &mut self.0, event)
    }

    fn client_request(&mut self, window: WindowId, request: ClientRequest) {
        hot!(S::client_request, &mut self.0, window, request)
    }

    fn theme(&self) -> Theme {
        hot!(S::theme, &self.0)
    }

    fn paint_background(&mut self, painter: &egui::Painter, screen: egui::Rect) {
        hot!(S::paint_background, &mut self.0, painter, screen)
    }

    fn paint_decoration(&mut self, painter: &egui::Painter, placement: &Placement) {
        hot!(S::paint_decoration, &mut self.0, painter, placement)
    }

    fn wants_window_pictures(&self) -> bool {
        hot!(S::wants_window_pictures, &self.0)
    }

    fn window_pictures(&mut self, pictures: Option<WindowPictures>) {
        hot!(S::window_pictures, &mut self.0, pictures)
    }

    fn chrome(&mut self, ui: &mut egui::Ui, elapsed_ms: u32) {
        hot!(S::chrome, &mut self.0, ui, elapsed_ms)
    }

    fn blur_regions(&self) -> Vec<Blur> {
        hot!(S::blur_regions, &self.0)
    }

    fn frame_interval(&self, timing: &OutputTiming) -> Duration {
        hot!(S::frame_interval, &self.0, timing)
    }

    fn spawn_argv(&mut self, app: &str) -> Vec<String> {
        hot!(S::spawn_argv, &mut self.0, app)
    }

    fn set_reserved(&mut self, reserved: crate::Reserved) {
        hot!(S::set_reserved, &mut self.0, reserved)
    }

    fn take_commands(&mut self) -> Vec<Command> {
        hot!(S::take_commands, &mut self.0)
    }

    fn access_subtrees(&mut self) -> Vec<a11y::Subtree> {
        hot!(S::access_subtrees, &mut self.0)
    }

    fn access_action(
        &mut self,
        window: WindowId,
        node: accesskit::NodeId,
        action: accesskit::Action,
        value: Option<&str>,
    ) {
        hot!(S::access_action, &mut self.0, window, node, action, value)
    }

    fn described(&mut self, request: u64, tree: a11y::Snapshot) {
        hot!(S::described, &mut self.0, request, tree)
    }

    fn captured(&mut self, request: u64, frame: Result<Capture, String>) {
        hot!(S::captured, &mut self.0, request, frame)
    }

    fn input_source(&mut self, synthetic: bool) {
        hot!(S::input_source, &mut self.0, synthetic)
    }

    fn text_input(&mut self, field: Option<TextField>) {
        hot!(S::text_input, &mut self.0, field)
    }

    fn window_hint(&mut self, window: WindowId, hint: WindowHint) {
        hot!(S::window_hint, &mut self.0, window, hint)
    }

    fn bell(&mut self, window: Option<WindowId>) {
        hot!(S::bell, &mut self.0, window)
    }

    fn activate(&mut self, window: WindowId) {
        hot!(S::activate, &mut self.0, window)
    }

    fn idle_inhibited(&mut self, inhibited: bool) {
        hot!(S::idle_inhibited, &mut self.0, inhibited)
    }

    fn session_locked(&mut self, locked: bool) {
        hot!(S::session_locked, &mut self.0, locked)
    }

    fn workspaces(&self) -> Vec<WorkspaceInfo> {
        hot!(S::workspaces, &self.0)
    }

    fn activate_workspace(&mut self, id: &str) {
        hot!(S::activate_workspace, &mut self.0, id)
    }

    fn inhibit_shortcuts(&mut self, window: WindowId) -> bool {
        hot!(S::inhibit_shortcuts, &mut self.0, window)
    }
}

/// Starts receiving patches from `dx serve` (a no-op when not launched by it).
#[cfg(feature = "hotpatch")]
pub(crate) fn connect() {
    if cfg!(debug_assertions) {
        dioxus_devtools::connect_subsecond();
    }
}

#[cfg(not(feature = "hotpatch"))]
pub(crate) fn connect() {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestShell {
        blurs: Vec<Blur>,
        interval: Duration,
        pictures: Option<WindowPictures>,
    }

    impl Shell for TestShell {
        fn map_window(&mut self, _: &str, _: &str) -> WindowId {
            WindowId::new(1).unwrap()
        }

        fn unmap_window(&mut self, _: WindowId) {}

        fn set_output(&mut self, _: (i32, i32)) {}

        fn focused(&self) -> Option<WindowId> {
            None
        }

        fn placements(&self) -> Vec<Placement> {
            Vec::new()
        }

        fn blur_regions(&self) -> Vec<Blur> {
            self.blurs.clone()
        }

        fn frame_interval(&self, timing: &OutputTiming) -> Duration {
            self.interval.max(timing.refresh_interval())
        }

        fn wants_window_pictures(&self) -> bool {
            true
        }

        fn window_pictures(&mut self, pictures: Option<WindowPictures>) {
            self.pictures = pictures;
        }
    }

    #[test]
    fn forwards_blur_and_frame_interval() {
        let mut shell = Hot(TestShell {
            blurs: vec![Blur {
                area: mcsapi::Geometry::new((10, 20).into(), (420, 56).into()),
                corner_radius: 12,
                strength: 3,
            }],
            interval: Duration::from_millis(33),
            pictures: None,
        });
        assert_eq!(shell.blur_regions(), shell.blurs);
        let timing = OutputTiming::default();
        assert_eq!(shell.frame_interval(&timing), shell.interval);

        shell.blurs.clear();
        shell.interval = Duration::ZERO;
        assert!(shell.blur_regions().is_empty());
        assert_eq!(shell.frame_interval(&timing), timing.refresh_interval());
    }

    #[test]
    fn forwards_window_pictures() {
        let mut shell = Hot(TestShell {
            blurs: Vec::new(),
            interval: Duration::ZERO,
            pictures: None,
        });
        assert!(shell.wants_window_pictures());
        let pictures = WindowPictures {
            texture: egui::TextureId::User(3),
            size: egui::vec2(1920.0, 1080.0),
        };
        shell.window_pictures(Some(pictures));
        assert_eq!(shell.pictures, Some(pictures));
    }
}
