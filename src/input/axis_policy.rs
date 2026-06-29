use niri_config::{MainAxis, Modifiers};

use crate::layout::axis::PhysicalAxis;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputAxisPolicy {
    main_axis: MainAxis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewWheelTarget {
    Section,
    Workspace,
}

impl InputAxisPolicy {
    pub const fn from_main_axis(main_axis: MainAxis) -> Self {
        Self { main_axis }
    }

    pub const fn main_axis(self) -> MainAxis {
        self.main_axis
    }

    pub const fn is_vertical(self) -> bool {
        matches!(self.main_axis, MainAxis::Vertical)
    }

    pub fn gesture_prefers_view_offset(self, cumulative_x: f64, cumulative_y: f64) -> bool {
        if self.is_vertical() {
            cumulative_y.abs() > cumulative_x.abs()
        } else {
            cumulative_x.abs() > cumulative_y.abs()
        }
    }

    pub fn split_view_workspace_deltas(self, delta_x: f64, delta_y: f64) -> (f64, f64) {
        if self.is_vertical() {
            (delta_y, delta_x)
        } else {
            (delta_x, delta_y)
        }
    }

    /// Maps a layout-oriented action to the physical axis it should act on for the screenshot UI.
    ///
    /// In layout terms, section/window width actions and section moves act along the layout's main
    /// axis, and window-height actions and window moves act along the cross axis. The screenshot
    /// UI works in physical (X/Y) coordinates regardless of layout, so when the layout is vertical
    /// we need to swap the two: actions the user thinks of as "section/main-axis" should affect the
    /// selection vertically, and "window/cross-axis" actions should affect it horizontally.
    pub fn screenshot_main_axis(self) -> PhysicalAxis {
        if self.is_vertical() {
            PhysicalAxis::Height
        } else {
            PhysicalAxis::Width
        }
    }

    pub fn screenshot_cross_axis(self) -> PhysicalAxis {
        if self.is_vertical() {
            PhysicalAxis::Width
        } else {
            PhysicalAxis::Height
        }
    }

    pub fn overview_wheel_target(
        self,
        horizontal: bool,
        modifiers: Modifiers,
    ) -> Option<OverviewWheelTarget> {
        if horizontal {
            if modifiers.is_empty() {
                Some(if self.is_vertical() {
                    OverviewWheelTarget::Workspace
                } else {
                    OverviewWheelTarget::Section
                })
            } else {
                None
            }
        } else if modifiers.is_empty() {
            Some(if self.is_vertical() {
                OverviewWheelTarget::Section
            } else {
                OverviewWheelTarget::Workspace
            })
        } else if modifiers == Modifiers::SHIFT {
            Some(if self.is_vertical() {
                OverviewWheelTarget::Workspace
            } else {
                OverviewWheelTarget::Section
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_prefers_view_offset_respects_main_axis() {
        let horizontal = InputAxisPolicy::from_main_axis(MainAxis::Horizontal);
        let vertical = InputAxisPolicy::from_main_axis(MainAxis::Vertical);

        assert!(horizontal.gesture_prefers_view_offset(20., 1.));
        assert!(!horizontal.gesture_prefers_view_offset(1., 20.));
        assert!(vertical.gesture_prefers_view_offset(1., 20.));
        assert!(!vertical.gesture_prefers_view_offset(20., 1.));
        assert!(!horizontal.gesture_prefers_view_offset(10., 10.));
        assert!(!vertical.gesture_prefers_view_offset(10., 10.));
    }

    #[test]
    fn split_view_workspace_deltas_respects_main_axis() {
        let horizontal = InputAxisPolicy::from_main_axis(MainAxis::Horizontal);
        let vertical = InputAxisPolicy::from_main_axis(MainAxis::Vertical);

        assert_eq!(horizontal.split_view_workspace_deltas(3., -7.), (3., -7.));
        assert_eq!(vertical.split_view_workspace_deltas(3., -7.), (-7., 3.));
    }

    #[test]
    fn screenshot_axes_respect_main_axis() {
        let horizontal = InputAxisPolicy::from_main_axis(MainAxis::Horizontal);
        let vertical = InputAxisPolicy::from_main_axis(MainAxis::Vertical);

        assert_eq!(horizontal.screenshot_main_axis(), PhysicalAxis::Width);
        assert_eq!(horizontal.screenshot_cross_axis(), PhysicalAxis::Height);
        assert_eq!(vertical.screenshot_main_axis(), PhysicalAxis::Height);
        assert_eq!(vertical.screenshot_cross_axis(), PhysicalAxis::Width);
    }

    #[test]
    fn overview_wheel_target_respects_main_axis_and_shift() {
        let horizontal = InputAxisPolicy::from_main_axis(MainAxis::Horizontal);
        let vertical = InputAxisPolicy::from_main_axis(MainAxis::Vertical);

        assert_eq!(
            horizontal.overview_wheel_target(true, Modifiers::empty()),
            Some(OverviewWheelTarget::Section)
        );
        assert_eq!(
            vertical.overview_wheel_target(true, Modifiers::empty()),
            Some(OverviewWheelTarget::Workspace)
        );

        assert_eq!(
            horizontal.overview_wheel_target(false, Modifiers::empty()),
            Some(OverviewWheelTarget::Workspace)
        );
        assert_eq!(
            horizontal.overview_wheel_target(false, Modifiers::SHIFT),
            Some(OverviewWheelTarget::Section)
        );

        assert_eq!(
            vertical.overview_wheel_target(false, Modifiers::empty()),
            Some(OverviewWheelTarget::Section)
        );
        assert_eq!(
            vertical.overview_wheel_target(false, Modifiers::SHIFT),
            Some(OverviewWheelTarget::Workspace)
        );

        assert_eq!(
            horizontal.overview_wheel_target(true, Modifiers::SHIFT),
            None
        );
    }
}
