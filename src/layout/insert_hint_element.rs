use niri_config::CornerRadius;
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::focus_ring::{FocusRing, FocusRingRenderElement};
use crate::render_helpers::renderer::NiriRenderer;

/// Which kind of drop the hint represents, selecting its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    /// A split/move target (place beside/above/below, or a new section).
    Split,
    /// A swap target (centre-drop in the in-place drag).
    Swap,
    /// A tab-add target (drop on a tabbing container's titlebar).
    Tab,
}

#[derive(Debug)]
pub struct InsertHintElement {
    inner: FocusRing,
}

pub type InsertHintRenderElement = FocusRingRenderElement;

impl InsertHintElement {
    pub fn new(config: niri_config::InsertHint) -> Self {
        Self {
            inner: FocusRing::new(Self::ring_config(config)),
        }
    }

    pub fn update_config(&mut self, config: niri_config::InsertHint) {
        self.inner.update_config(Self::ring_config(config));
    }

    // The three FocusRing colour slots are repurposed for the three drop kinds, selected at render
    // time via the (is_active, is_urgent) flags: active = Split, inactive = Swap, urgent = Tab.
    fn ring_config(config: niri_config::InsertHint) -> niri_config::FocusRing {
        niri_config::FocusRing {
            off: config.off,
            width: 0.,
            active_color: config.color,
            inactive_color: config.swap_color,
            urgent_color: config.tab_color,
            active_gradient: config.gradient,
            inactive_gradient: config.swap_gradient,
            urgent_gradient: config.tab_gradient,
        }
    }

    pub fn update_shaders(&mut self) {
        self.inner.update_shaders();
    }

    pub fn update_render_elements(
        &mut self,
        size: Size<f64, Logical>,
        kind: HintKind,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
    ) {
        let (is_active, is_urgent) = match kind {
            HintKind::Split => (true, false),
            HintKind::Swap => (false, false),
            HintKind::Tab => (false, true),
        };
        self.inner
            .update_render_elements(size, is_active, false, is_urgent, view_rect, radius, scale, 1.);
    }

    pub fn render(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        self.inner.render(renderer, location, push)
    }
}
