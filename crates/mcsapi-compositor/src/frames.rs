//! Frame statistics for the log: how often frames came and how long each
//! took to draw, so a slow frame rate can be measured on the machine it
//! happens on rather than guessed at.
//!
//! The host times every frame it draws. Every [`WINDOW`] the frames so far
//! are summed up in one `debug` event under the `mcsapi_compositor::frames`
//! target, which `RUST_LOG=mcsapi_compositor::frames=debug` turns on; while
//! it is off nothing is kept.

use std::time::{Duration, Instant};

use tracing::{Level, debug, enabled};

/// How long each summary covers.
pub(crate) const WINDOW: Duration = Duration::from_secs(5);

/// The frames drawn since the last summary.
#[derive(Debug, Default)]
pub(crate) struct FrameStats {
    /// When the window began.
    since: Option<Instant>,
    /// When the last frame started.
    last: Option<Instant>,
    /// Start to start of consecutive frames.
    periods: Vec<Duration>,
    /// The time spent drawing each frame.
    draws: Vec<Duration>,
    /// Ticks that found the display still busy with earlier frames.
    skipped: u32,
    /// Ticks that drew nothing because nothing on screen changed.
    idle: u32,
    /// Frames, among those drawn, that only moved the cursor over the
    /// scene drawn before.
    cursor: u32,
}

/// One window's frames, summed up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Summary {
    /// Frames drawn per second over the window.
    pub fps: f64,
    /// Frame periods at the 50th and 99th percentile, and the longest.
    pub period: [Duration; 3],
    /// Drawing times the same way.
    pub draw: [Duration; 3],
    /// Frames that came later than `target` and a half.
    pub late: usize,
}

impl FrameStats {
    /// Whether anyone reads the summaries.
    pub(crate) fn enabled() -> bool {
        enabled!(target: "mcsapi_compositor::frames", Level::DEBUG)
    }

    /// A frame started at `start` and took `draw` to draw.
    pub(crate) fn drawn(&mut self, start: Instant, draw: Duration) {
        self.since.get_or_insert(start);
        if let Some(last) = self.last.replace(start) {
            self.periods.push(start.saturating_duration_since(last));
        }
        self.draws.push(draw);
    }

    /// A frame started at `start` only moved the cursor, in `draw`.
    pub(crate) fn cursor(&mut self, start: Instant, draw: Duration) {
        self.drawn(start, draw);
        self.cursor += 1;
    }

    /// A tick drew nothing because the display had frames enough queued.
    pub(crate) fn skipped(&mut self) {
        self.skipped += 1;
    }

    /// A tick drew nothing because nothing on screen changed. The next
    /// frame's period is not counted: a pause is not a slow frame.
    pub(crate) fn idle(&mut self) {
        self.since.get_or_insert_with(Instant::now);
        self.last = None;
        self.idle += 1;
    }

    /// Logs a summary once a window is over; `target` is the frame interval
    /// the shell asked for.
    pub(crate) fn report(&mut self, now: Instant, target: Duration) {
        let Some(since) = self.since else {
            return;
        };
        let span = now.saturating_duration_since(since);
        if span < WINDOW {
            return;
        }
        if let Some(s) = self.summary(span, target) {
            debug!(
                target: "mcsapi_compositor::frames",
                fps = format_args!("{:.1}", s.fps),
                target_fps = format_args!("{:.1}", 1.0 / target.as_secs_f64()),
                period_p50_ms = ms(s.period[0]),
                period_p99_ms = ms(s.period[1]),
                period_max_ms = ms(s.period[2]),
                draw_p50_ms = ms(s.draw[0]),
                draw_p99_ms = ms(s.draw[1]),
                draw_max_ms = ms(s.draw[2]),
                late = s.late,
                skipped = self.skipped,
                idle = self.idle,
                cursor_only = self.cursor,
                "frames"
            );
        } else {
            // Too few frames in a row for periods: an idle screen.
            debug!(
                target: "mcsapi_compositor::frames",
                fps = format_args!("{:.1}", self.draws.len() as f64 / span.as_secs_f64()),
                target_fps = format_args!("{:.1}", 1.0 / target.as_secs_f64()),
                skipped = self.skipped,
                idle = self.idle,
                "frames"
            );
        }
        *self = Self {
            since: Some(now),
            last: self.last,
            ..Self::default()
        };
    }

    /// The window's frames over `span`, or `None` before two frames.
    pub(crate) fn summary(&mut self, span: Duration, target: Duration) -> Option<Summary> {
        if self.periods.is_empty() {
            return None;
        }
        self.periods.sort_unstable();
        self.draws.sort_unstable();
        let late = target + target / 2;
        Some(Summary {
            fps: self.draws.len() as f64 / span.as_secs_f64(),
            period: percentiles(&self.periods),
            draw: percentiles(&self.draws),
            late: self.periods.iter().filter(|p| **p > late).count(),
        })
    }
}

/// The 50th and 99th percentile and the largest of sorted, non-empty `v`.
fn percentiles(v: &[Duration]) -> [Duration; 3] {
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    [at(0.5), at(0.99), v[v.len() - 1]]
}

/// Milliseconds to a tenth, for the log.
fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_late_frames_and_rate() {
        let mut stats = FrameStats::default();
        let start = Instant::now();
        let target = Duration::from_micros(16_667);
        let mut at = start;
        for i in 0..60 {
            stats.drawn(at, Duration::from_millis(4));
            // Every tenth frame comes a whole refresh late.
            at += if i % 10 == 9 { target * 2 } else { target };
        }
        let s = stats.summary(Duration::from_secs(1), target).unwrap();
        assert_eq!(s.late, 5);
        assert!((s.fps - 60.0).abs() < 0.01);
        assert_eq!(s.draw, [Duration::from_millis(4); 3]);
        assert_eq!(s.period[0], target);
        assert_eq!(s.period[2], target * 2);
    }

    #[test]
    fn one_frame_is_no_summary() {
        let mut stats = FrameStats::default();
        stats.drawn(Instant::now(), Duration::from_millis(1));
        assert_eq!(stats.summary(WINDOW, WINDOW), None);
    }
}
