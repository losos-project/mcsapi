use mcsapi_ui::{App, ClickTiming, Theme, egui, run_frame};

/// One button filling the window, counting its clicks.
#[derive(Default)]
struct Button {
    clicks: u32,
}

impl App for Button {
    fn title(&self) -> &str {
        "Button"
    }

    fn ui(&mut self, ui: &mut egui::Ui, _: &Theme) {
        let size = ui.available_size();
        if ui.add_sized(size, egui::Button::new("Press")).clicked() {
            self.clicks += 1;
        }
    }
}

const AT: egui::Pos2 = egui::pos2(200.0, 150.0);

fn button(pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos: AT,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    }
}

/// A press and a release `held_ms` apart on the input's clock, the press
/// delivered by a frame at 1 s and the release by one at `released_at` s
/// (1 s too when both are in one frame). Returns how many clicks the button
/// saw, with and without `ClickTiming`.
fn click(held_ms: u32, released_at: f64, timed: bool) -> u32 {
    let context = egui::Context::default();
    let mut app = Button::default();
    let mut timing = ClickTiming::default();
    let mut frame = |time: f64, events: Vec<egui::Event>, timing: &mut ClickTiming| {
        if timed {
            timing.frame(&context, time);
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            events,
            time: Some(time),
            focused: true,
            ..Default::default()
        };
        run_frame(&mut app, &context, input, &Theme::default())
            .textures_delta
            .clear();
    };
    frame(0.0, vec![egui::Event::PointerMoved(AT)], &mut timing);
    timing.button(true, 50_000);
    if released_at > 1.0 {
        frame(1.0, vec![button(true)], &mut timing);
        timing.button(false, 50_000 + held_ms);
        frame(released_at, vec![button(false)], &mut timing);
    } else {
        timing.button(false, 50_000 + held_ms);
        frame(1.0, vec![button(true), button(false)], &mut timing);
    }
    frame(released_at + 0.1, vec![], &mut timing);
    app.clicks
}

#[test]
fn a_quick_click_counts_however_slow_the_frames_were() {
    // A 0.15 s click whose release was delivered 4 s after its press, as in
    // a VM drawing without acceleration: egui alone drops it.
    assert_eq!(click(150, 5.0, false), 0);
    assert_eq!(click(150, 5.0, true), 1);
}

#[test]
fn a_long_press_is_not_a_click_however_fast_the_frames_were() {
    // Held 2 s, but press and release reached the app in one frame: egui
    // alone sees no time between them.
    assert_eq!(click(2_000, 1.0, false), 1);
    assert_eq!(click(2_000, 1.0, true), 0);
}

#[test]
fn ordinary_clicks_are_unchanged() {
    assert_eq!(click(150, 1.15, true), 1);
    assert_eq!(click(1_500, 2.5, true), 0);
}
