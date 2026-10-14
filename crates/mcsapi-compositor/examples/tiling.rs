//! A minimal tiling session on mcsapi's `Desktop`: tall layout, title bars,
//! Super shortcuts, and one in-process app next to Wayland clients.
//!
//! ```console
//! $ cargo run -p mcsapi-compositor --example tiling -- foot
//! ```
//!
//! For a nested instance that hot-patches as you edit this file, run it
//! under the Dioxus CLI instead (see the crate README):
//!
//! ```console
//! $ dx serve --hot-patch -p mcsapi-compositor --example tiling --features hotpatch
//! ```
//!
//! Arguments are apps to launch; `clock` is the built-in app. Keys:
//! Super+Enter launches foot, Super+C the clock, Super+J/K move focus,
//! Super+Space promotes, Super+1…4 switch workspaces, Super+Q closes,
//! Super+B toggles the frosted dock's blur, Super+L toggles a ≤30 fps low
//! power mode, Super+Escape quits.

use std::{collections::BTreeMap, time::Duration};

use mcsapi::{Desktop, Geometry, WindowId, WorkspaceId};
use mcsapi_compositor::{
    App, AppId, Apps, Blur, Command, Compositor, Hot, InstanceId, KeyInput, KeyRoute, Keysym,
    OutputTiming, Placement, Shell, Theme, WindowHint, WorkspaceInfo, egui,
};
use mcsapi_runtime::{Manifest, Runtime};

const TITLE_BAR: i32 = 26;
const GAP: i32 = 6;
const DOCK: (i32, i32) = (420, 56);

struct Tiling {
    desktop: Desktop,
    titles: BTreeMap<WindowId, String>,
    next: u64,
    size: (i32, i32),
    theme: Theme,
    commands: Vec<Command>,
    blur: bool,
    low_power: bool,
}

impl Tiling {
    /// A floating dock centred near the bottom edge.
    fn dock(&self) -> Geometry {
        Geometry::new(
            ((self.size.0 - DOCK.0) / 2, self.size.1 - DOCK.1 - 24).into(),
            DOCK.into(),
        )
    }
}

impl Shell for Tiling {
    fn map_window(&mut self, app_id: &str, title: &str) -> WindowId {
        self.next += 1;
        let id = WindowId::new(self.next).expect("IDs start at 1");
        self.desktop.insert(id).expect("fresh IDs are unique");
        let label = if title.is_empty() { app_id } else { title };
        self.titles.insert(id, label.to_owned());
        id
    }

    fn unmap_window(&mut self, window: WindowId) {
        let _ = self.desktop.remove(window);
        self.titles.remove(&window);
    }

    fn set_output(&mut self, size: (i32, i32)) {
        self.size = size;
    }

    fn focused(&self) -> Option<WindowId> {
        self.desktop.active().focused()
    }

    fn placements(&self) -> Vec<Placement> {
        let bounds = Geometry::new(
            (GAP, GAP).into(),
            (self.size.0 - 2 * GAP, self.size.1 - 2 * GAP).into(),
        );
        let focused = self.focused();
        let Ok(placements) = self.desktop.active().arrange(bounds) else {
            return Vec::new();
        };
        placements
            .map(|p| {
                let g = p.geometry;
                let frame = Geometry::new(
                    (g.loc.x + GAP / 2, g.loc.y + GAP / 2).into(),
                    (g.size.w - GAP, g.size.h - GAP).into(),
                );
                let client = Geometry::new(
                    (frame.loc.x, frame.loc.y + TITLE_BAR).into(),
                    (frame.size.w, (frame.size.h - TITLE_BAR).max(1)).into(),
                );
                Placement {
                    window: p.window,
                    frame,
                    client,
                    focused: focused == Some(p.window),
                    tiled: mcsapi_compositor::Edges::ALL,
                    maximized: false,
                    motion: None,
                }
            })
            .collect()
    }

    fn set_title(&mut self, window: WindowId, title: &str) {
        self.titles.insert(window, title.to_owned());
    }

    // A real shell would float dialogs over their parent and show icons in a
    // task list; this one only logs what clients say.
    fn window_hint(&mut self, window: WindowId, hint: WindowHint) {
        tracing::info!(?window, ?hint, "window hint");
    }

    fn bell(&mut self, window: Option<WindowId>) {
        tracing::info!(?window, "bell");
    }

    fn idle_inhibited(&mut self, inhibited: bool) {
        tracing::info!(inhibited, "idle inhibited");
    }

    fn session_locked(&mut self, locked: bool) {
        tracing::info!(locked, "session locked");
    }

    fn workspaces(&self) -> Vec<WorkspaceInfo> {
        let active = self.desktop.active().id();
        self.desktop
            .workspaces()
            .map(|w| {
                let n = w.id().get().to_string();
                WorkspaceInfo::new(n.clone(), n, w.id() == active)
            })
            .collect()
    }

    fn activate_workspace(&mut self, id: &str) {
        if let Some(ws) = id.parse().ok().and_then(WorkspaceId::new) {
            let _ = self.desktop.switch_to(ws);
        }
    }

    fn client_request(&mut self, window: WindowId, request: mcsapi_compositor::ClientRequest) {
        tracing::info!(?window, ?request, "client request");
    }

    fn focus(&mut self, window: WindowId) {
        let _ = self.desktop.focus(window);
    }

    fn pointer_down(&mut self, at: (i32, i32), _time_ms: u64) -> mcsapi_compositor::Press {
        if let Some(p) = self.placements().into_iter().find(|p| {
            let g = p.frame;
            at.0 >= g.loc.x
                && at.1 >= g.loc.y
                && at.0 < g.loc.x + g.size.w
                && at.1 < g.loc.y + g.size.h
        }) {
            let _ = self.desktop.focus(p.window);
        }
        mcsapi_compositor::Press::Client
    }

    fn key(&mut self, key: &KeyInput) -> KeyRoute {
        if !key.mods.logo {
            return KeyRoute::Client;
        }
        if !key.pressed {
            return KeyRoute::Consume;
        }
        match key.sym {
            Keysym::Return => self.commands.push(Command::Launch("foot".into())),
            Keysym::c => self.commands.push(Command::Launch("clock".into())),
            Keysym::j => {
                self.desktop.focus_next();
            }
            Keysym::k => {
                self.desktop.focus_previous();
            }
            Keysym::space => self.desktop.promote_focused(),
            Keysym::b => self.blur = !self.blur,
            Keysym::l => self.low_power = !self.low_power,
            Keysym::q => {
                if let Some(w) = self.focused() {
                    self.commands.push(Command::Close(w));
                }
            }
            Keysym::Escape => self.commands.push(Command::Quit),
            sym => {
                let digit = sym.key_char().and_then(|c| c.to_digit(10));
                if let Some(ws) = digit.and_then(|d| WorkspaceId::new(d.into())) {
                    let _ = self.desktop.switch_to(ws);
                }
            }
        }
        KeyRoute::Consume
    }

    fn theme(&self) -> Theme {
        self.theme
    }

    fn paint_background(&mut self, painter: &egui::Painter, screen: egui::Rect) {
        painter.rect_filled(screen, 0, self.theme.background);
    }

    fn paint_decoration(&mut self, painter: &egui::Painter, p: &Placement) {
        let g = p.frame;
        let frame = egui::Rect::from_min_size(
            egui::pos2(g.loc.x as f32, g.loc.y as f32),
            egui::vec2(g.size.w as f32, g.size.h as f32),
        );
        let bar = egui::Rect::from_min_size(frame.min, egui::vec2(frame.width(), TITLE_BAR as f32));
        let accent = if p.focused {
            self.theme.accent
        } else {
            self.theme.border
        };
        painter.rect_filled(bar, 6, self.theme.surface);
        painter.rect_stroke(
            frame,
            6,
            egui::Stroke::new(1.0, accent),
            egui::StrokeKind::Inside,
        );
        painter.text(
            bar.center(),
            egui::Align2::CENTER_CENTER,
            self.titles.get(&p.window).map_or("", String::as_str),
            egui::FontId::proportional(13.0),
            self.theme.foreground,
        );
    }

    fn chrome(&mut self, ui: &mut egui::Ui, _elapsed_ms: u32) {
        let g = self.dock();
        let dock = egui::Rect::from_min_size(
            egui::pos2(g.loc.x as f32, g.loc.y as f32),
            egui::vec2(g.size.w as f32, g.size.h as f32),
        );
        // A translucent fill over the blurred backdrop.
        let painter = ui.painter();
        painter.rect_filled(dock, 16, self.theme.surface.gamma_multiply(0.55));
        painter.rect_stroke(
            dock,
            16,
            egui::Stroke::new(1.0, self.theme.border),
            egui::StrokeKind::Inside,
        );
        painter.text(
            dock.center(),
            egui::Align2::CENTER_CENTER,
            format!(
                "blur {} · {}",
                if self.blur { "on" } else { "off" },
                if self.low_power {
                    "low power, ≤30 fps"
                } else {
                    "full frame rate"
                }
            ),
            egui::FontId::proportional(15.0),
            self.theme.foreground,
        );
    }

    fn blur_regions(&self) -> Vec<Blur> {
        if !self.blur {
            return Vec::new();
        }
        vec![Blur {
            area: self.dock(),
            corner_radius: 16,
            strength: 6,
        }]
    }

    fn frame_interval(&self, timing: &OutputTiming) -> Duration {
        if self.low_power {
            timing.interval_for(30)
        } else {
            timing.refresh_interval()
        }
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }
}

/// A built-in app, to show in-process apps next to Wayland clients.
struct Clock {
    started: std::time::Instant,
}

impl App for Clock {
    fn title(&self) -> &str {
        "Clock"
    }

    fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let secs = self.started.elapsed().as_secs();
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(
                egui::RichText::new(format!("{:02}:{:02}", secs / 60, secs % 60))
                    .size(48.0)
                    .color(theme.accent),
            );
            ui.label(egui::RichText::new("running in the compositor").color(theme.foreground));
        });
        ui.ctx().request_repaint();
    }
}

struct BuiltIn {
    runtime: Runtime,
    running: BTreeMap<InstanceId, Clock>,
}

impl Apps for BuiltIn {
    fn resolve(&self, name: &str) -> Option<AppId> {
        let id = if name == "clock" {
            "org.mcsapi.clock"
        } else {
            name
        };
        let id = AppId::new(id)?;
        self.runtime.app(&id).is_some().then_some(id)
    }

    fn launch(&mut self, app: &AppId) -> Result<InstanceId, mcsapi_runtime::Error> {
        let instance = self.runtime.launch(app)?;
        self.running.insert(
            instance,
            Clock {
                started: std::time::Instant::now(),
            },
        );
        Ok(instance)
    }

    fn stop(&mut self, instance: InstanceId) {
        let _ = self.runtime.stop(instance);
        self.running.remove(&instance);
    }

    fn app_mut(&mut self, instance: InstanceId) -> Option<&mut dyn App> {
        self.running.get_mut(&instance).map(|c| c as &mut dyn App)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The host reports through `tracing`; without a subscriber its warnings
    // (a client that cannot start, a failed render) would go nowhere.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let mut runtime = Runtime::new();
    runtime.register(Manifest::new(
        AppId::new("org.mcsapi.clock").expect("valid ID"),
        "Clock",
    ))?;
    let shell = Tiling {
        desktop: Desktop::new((1..=4).filter_map(WorkspaceId::new))?,
        titles: BTreeMap::new(),
        next: 0,
        size: (1280, 800),
        theme: Theme::default(),
        commands: Vec::new(),
        blur: true,
        low_power: false,
    };
    // `Hot` lets `dx serve --hot-patch` swap in edited `Tiling` methods live.
    let mut compositor = Compositor::new(Hot(shell))
        .title("mcsapi tiling")
        .apps(BuiltIn {
            runtime,
            running: BTreeMap::new(),
        });
    for app in std::env::args().skip(1) {
        compositor = compositor.launch(app);
    }
    compositor.run()
}
