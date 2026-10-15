//! Clicks timed by the input, not by the frames that deliver it.
//!
//! egui calls a press and release a click when they are less than
//! [`egui::InputOptions::max_click_duration`] apart, but it times them by
//! [`egui::RawInput::time`], one time per frame, so it measures the frames
//! that delivered them. When a frame is slow (a machine under load, or a VM
//! without hardware acceleration, where frames took seconds) a quick click
//! whose press and release land in frames more than that apart was never a
//! click, and a button simply ignored it. [`ClickTiming`] is given each
//! button's own event time and moves the limit for one frame by however far
//! the frames' clock and the input's disagree, so egui decides on the
//! input's clock.

/// Tracks one pointer's presses for one egui context.
///
/// Call [`ClickTiming::button`] as a button event is queued, with the time
/// the input device stamped it, and [`ClickTiming::frame`] before every
/// frame of that context.
#[derive(Debug, Default)]
pub struct ClickTiming {
    /// The press being held: its event time in milliseconds, and the time of
    /// the frame that delivered it once one has.
    press: Option<(u32, Option<f64>)>,
    /// When the held press was released, in milliseconds, until a frame
    /// delivers it.
    release: Option<u32>,
}

impl ClickTiming {
    /// Records a button press or release queued for the next frame, at
    /// `time_ms` on the input's clock. Only differences between these
    /// times are used, so any millisecond clock that wraps at `u32` will do.
    pub fn button(&mut self, pressed: bool, time_ms: u32) {
        if pressed {
            self.press = Some((time_ms, None));
            self.release = None;
        } else if self.press.is_some() {
            self.release = Some(time_ms);
        }
    }

    /// Sets `context`'s click duration for the frame about to run at `now`,
    /// the [`egui::RawInput::time`] it will be given.
    ///
    /// In the frame that delivers the release, the limit is the default
    /// moved by how much longer the frames say the press was held than the
    /// input does, so egui's comparison comes out as the input's would,
    /// even for a press and release delivered in one frame. While the
    /// button is still held there is no input time to compare with, so the
    /// frames' delay is all allowed for: holding still never stops a click
    /// by time alone before the release is known, and moving still does.
    pub fn frame(&mut self, context: &egui::Context, now: f64) {
        let limit = egui::InputOptions::default().max_click_duration;
        let limit = match &mut self.press {
            None => limit,
            Some((pressed_ms, frame)) => {
                let held_by_frames = now - *frame.get_or_insert(now);
                match self.release.take() {
                    Some(released_ms) => {
                        let held = f64::from(released_ms.wrapping_sub(*pressed_ms)) / 1000.0;
                        self.press = None;
                        limit + held_by_frames - held
                    }
                    None => limit + held_by_frames,
                }
            }
        };
        context.options_mut(|o| o.input_options.max_click_duration = limit);
    }
}
