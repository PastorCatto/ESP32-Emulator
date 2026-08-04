//! Mimic touch: driving the emulated touch panel with a mouse.
//!
//! The host has a cursor; the board has a touch panel. This turns one into the
//! other, so clicking the on-screen display behaves like a finger.
//!
//! The translation is not just "pass the coordinates through". Three things
//! have to happen or firmware will not believe the input:
//!
//! - **Rotation.** The panel's native orientation rarely matches how the
//!   screen is mounted. A board rotated 90 degrees needs its touch
//!   coordinates rotated by the same amount, or taps land somewhere else.
//! - **Scaling.** The window is usually displayed larger than 320x240, so
//!   window pixels have to come back to panel pixels.
//! - **Press and release as distinct events.** A touch controller reports a
//!   sustained press, not a click. Firmware that debounces or waits for
//!   release needs both edges.
//!
//! This module is deliberately free of any UI dependency: it takes plain
//! coordinates so it can be unit tested, and so a driver can be fed synthetic
//! input from a script as easily as from a mouse.

use serde::{Deserialize, Serialize};

/// How the panel is mounted relative to its native orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Deg180,
    Cw270,
}

impl Rotation {
    /// Build from the `rotation` key in a board file, which is in degrees.
    pub fn from_degrees(deg: u16) -> Self {
        match deg % 360 {
            90 => Rotation::Cw90,
            180 => Rotation::Deg180,
            270 => Rotation::Cw270,
            _ => Rotation::None,
        }
    }

    pub fn degrees(self) -> u16 {
        match self {
            Rotation::None => 0,
            Rotation::Cw90 => 90,
            Rotation::Deg180 => 180,
            Rotation::Cw270 => 270,
        }
    }

    /// Does this rotation exchange width and height?
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Cw90 | Rotation::Cw270)
    }
}

/// What the pointer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerPhase {
    /// Button went down.
    Press,
    /// Moved while held. Ignored when no press is active, since a touch panel
    /// cannot report a hover.
    Drag,
    /// Button came up.
    Release,
}

/// A touch as the panel would report it, in panel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    /// Reported pressure. Resistive controllers report a real value; we hand
    /// back a fixed "definitely pressed" reading, since a mouse has no
    /// pressure and firmware only ever compares it against a threshold.
    pub pressure: u16,
}

/// Nominal pressure for a mimicked touch, comfortably above any threshold.
pub const MIMIC_PRESSURE: u16 = 1000;

/// The current state of the mimicked panel.
///
/// Touch controllers are polled: firmware asks "is anything being touched, and
/// where". So this holds state rather than emitting a stream, and a driver
/// reads [`TouchState::current`] whenever the firmware asks.
#[derive(Debug, Clone, Default)]
pub struct TouchState {
    panel_width: u16,
    panel_height: u16,
    rotation: Rotation,
    /// Present exactly while a press is active.
    active: Option<TouchPoint>,
    /// Set on press, cleared once a driver has reported the release. Without
    /// this, a click shorter than one poll interval would be missed entirely.
    pending_release: bool,
}

impl TouchState {
    pub fn new(panel_width: u16, panel_height: u16, rotation: Rotation) -> Self {
        TouchState {
            panel_width,
            panel_height,
            rotation,
            active: None,
            pending_release: false,
        }
    }

    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    pub fn set_rotation(&mut self, rotation: Rotation) {
        self.rotation = rotation;
    }

    /// The touch being reported right now, if any.
    pub fn current(&self) -> Option<TouchPoint> {
        self.active
    }

    pub fn is_touched(&self) -> bool {
        self.active.is_some()
    }

    /// True once after a press ends, so a poll cannot miss a fast click.
    ///
    /// Consuming this is what lets a driver report the release edge exactly
    /// once even if the press and release both happened between two polls.
    pub fn take_pending_release(&mut self) -> bool {
        std::mem::take(&mut self.pending_release)
    }

    /// Feed a pointer event, given the size of the area the screen is drawn in.
    ///
    /// `view_x`/`view_y` are pixels within that area, so the caller does not
    /// have to know anything about the panel.
    pub fn pointer(
        &mut self,
        phase: PointerPhase,
        view_x: f32,
        view_y: f32,
        view_width: f32,
        view_height: f32,
    ) {
        match phase {
            PointerPhase::Press | PointerPhase::Drag => {
                // A drag with no press behind it is a hover, which no touch
                // panel can report, so it is dropped.
                if phase == PointerPhase::Drag && self.active.is_none() {
                    return;
                }
                let Some((x, y)) =
                    self.to_panel(view_x, view_y, view_width, view_height)
                else {
                    // Dragged off the edge: treat as lifting off, which is
                    // what a finger leaving the glass would look like.
                    if phase == PointerPhase::Drag {
                        self.release();
                    }
                    return;
                };
                self.active = Some(TouchPoint {
                    x,
                    y,
                    pressure: MIMIC_PRESSURE,
                });
            }
            PointerPhase::Release => self.release(),
        }
    }

    fn release(&mut self) {
        if self.active.take().is_some() {
            self.pending_release = true;
        }
    }

    /// Map a point in the displayed area onto panel coordinates.
    ///
    /// Returns `None` for anything outside the area, or for a degenerate
    /// viewport, rather than clamping: a tap next to the screen is not a tap
    /// on its edge.
    fn to_panel(
        &self,
        view_x: f32,
        view_y: f32,
        view_width: f32,
        view_height: f32,
    ) -> Option<(u16, u16)> {
        if view_width <= 0.0 || view_height <= 0.0 {
            return None;
        }
        if view_x < 0.0 || view_y < 0.0 || view_x >= view_width || view_y >= view_height {
            return None;
        }

        // Normalise first, so scaling and rotation stay independent.
        let u = view_x / view_width;
        let v = view_y / view_height;

        // The displayed image is already rotated, so undo that to get back to
        // the panel's own axes.
        let (pu, pv) = match self.rotation {
            Rotation::None => (u, v),
            Rotation::Cw90 => (v, 1.0 - u),
            Rotation::Deg180 => (1.0 - u, 1.0 - v),
            Rotation::Cw270 => (1.0 - v, u),
        };

        let w = self.panel_width.max(1);
        let h = self.panel_height.max(1);
        // Clamp the final index: rounding at exactly 1.0 would otherwise land
        // one pixel past the edge.
        let x = ((pu * w as f32) as i32).clamp(0, w as i32 - 1) as u16;
        let y = ((pv * h as f32) as i32).clamp(0, h as i32 - 1) as u16;
        Some((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> TouchState {
        TouchState::new(320, 240, Rotation::None)
    }

    #[test]
    fn a_press_reports_a_touch_and_a_release_clears_it() {
        let mut t = panel();
        assert!(!t.is_touched());

        t.pointer(PointerPhase::Press, 160.0, 120.0, 320.0, 240.0);
        assert_eq!(
            t.current(),
            Some(TouchPoint { x: 160, y: 120, pressure: MIMIC_PRESSURE })
        );

        t.pointer(PointerPhase::Release, 160.0, 120.0, 320.0, 240.0);
        assert!(!t.is_touched());
    }

    #[test]
    fn a_scaled_window_maps_back_to_panel_pixels() {
        let mut t = panel();
        // Screen drawn at 3x. The centre must still be the panel's centre.
        t.pointer(PointerPhase::Press, 480.0, 360.0, 960.0, 720.0);
        let p = t.current().unwrap();
        assert_eq!((p.x, p.y), (160, 120));
    }

    #[test]
    fn rotation_is_undone_so_taps_land_where_they_look() {
        // Top-left of a 90-degree rotated view is the panel's bottom-left.
        let mut t = TouchState::new(320, 240, Rotation::Cw90);
        t.pointer(PointerPhase::Press, 0.0, 0.0, 320.0, 240.0);
        assert_eq!(t.current().unwrap(), TouchPoint { x: 0, y: 239, pressure: MIMIC_PRESSURE });

        let mut t = TouchState::new(320, 240, Rotation::Deg180);
        t.pointer(PointerPhase::Press, 0.0, 0.0, 320.0, 240.0);
        assert_eq!(t.current().unwrap(), TouchPoint { x: 319, y: 239, pressure: MIMIC_PRESSURE });

        let mut t = TouchState::new(320, 240, Rotation::Cw270);
        t.pointer(PointerPhase::Press, 0.0, 0.0, 320.0, 240.0);
        assert_eq!(t.current().unwrap(), TouchPoint { x: 319, y: 0, pressure: MIMIC_PRESSURE });
    }

    #[test]
    fn dragging_moves_the_touch_without_lifting() {
        let mut t = panel();
        t.pointer(PointerPhase::Press, 10.0, 10.0, 320.0, 240.0);
        t.pointer(PointerPhase::Drag, 100.0, 50.0, 320.0, 240.0);
        assert_eq!(t.current().map(|p| (p.x, p.y)), Some((100, 50)));
        assert!(t.is_touched());
    }

    #[test]
    fn a_hover_is_not_a_touch() {
        let mut t = panel();
        // Moving with no button held must not invent a press.
        t.pointer(PointerPhase::Drag, 100.0, 50.0, 320.0, 240.0);
        assert!(!t.is_touched());
    }

    #[test]
    fn dragging_off_the_edge_lifts_off() {
        let mut t = panel();
        t.pointer(PointerPhase::Press, 10.0, 10.0, 320.0, 240.0);
        t.pointer(PointerPhase::Drag, 400.0, 10.0, 320.0, 240.0);
        assert!(!t.is_touched(), "leaving the panel should look like lift-off");
        assert!(t.take_pending_release());
    }

    #[test]
    fn a_press_outside_the_screen_is_ignored() {
        let mut t = panel();
        t.pointer(PointerPhase::Press, -5.0, 10.0, 320.0, 240.0);
        assert!(!t.is_touched());
        t.pointer(PointerPhase::Press, 10.0, 999.0, 320.0, 240.0);
        assert!(!t.is_touched());
    }

    #[test]
    fn a_click_between_polls_still_reports_its_release() {
        let mut t = panel();
        t.pointer(PointerPhase::Press, 5.0, 5.0, 320.0, 240.0);
        t.pointer(PointerPhase::Release, 5.0, 5.0, 320.0, 240.0);

        // The driver polls only now, after both edges already happened.
        assert!(!t.is_touched());
        assert!(t.take_pending_release(), "the release edge must survive");
        assert!(!t.take_pending_release(), "and be reported only once");
    }

    #[test]
    fn releasing_without_a_press_reports_nothing() {
        let mut t = panel();
        t.pointer(PointerPhase::Release, 5.0, 5.0, 320.0, 240.0);
        assert!(!t.take_pending_release());
    }

    #[test]
    fn taps_at_the_far_edge_stay_inside_the_panel() {
        let mut t = panel();
        // One pixel short of the full width, the worst case for rounding.
        t.pointer(PointerPhase::Press, 319.99, 239.99, 320.0, 240.0);
        let p = t.current().unwrap();
        assert!(p.x < 320 && p.y < 240, "got {p:?}");
    }

    #[test]
    fn a_degenerate_viewport_is_ignored_rather_than_dividing_by_zero() {
        let mut t = panel();
        t.pointer(PointerPhase::Press, 0.0, 0.0, 0.0, 0.0);
        assert!(!t.is_touched());
    }

    #[test]
    fn rotation_parses_from_board_degrees() {
        assert_eq!(Rotation::from_degrees(90), Rotation::Cw90);
        assert_eq!(Rotation::from_degrees(450), Rotation::Cw90);
        assert_eq!(Rotation::from_degrees(37), Rotation::None);
        assert!(Rotation::Cw90.swaps_axes());
        assert!(!Rotation::Deg180.swaps_axes());
    }
}
