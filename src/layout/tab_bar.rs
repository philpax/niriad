use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::renderer::NiriRenderer;

use super::tab_indicator::TabInfo;

niri_render_elements! {
    TabBarRenderElement => {
        Background = BorderRenderElement,
    }
}

/// i3/sway-style horizontal tab header bar with text labels.
///
/// Renders a horizontal bar at the top (or bottom) of a tabbed container,
/// showing text labels for each tab. The active tab is highlighted.
///
/// Title textures are rendered via pangocairo and cached per-tab,
/// invalidated on title change or scale change.
#[derive(Debug)]
pub struct TabBar {
    /// Cached geometry for each tab (computed during update_render_elements).
    tab_rects: Vec<Rectangle<f64, Logical>>,
    /// Open animation.
    open_anim: Option<Animation>,
    /// Config.
    config: niri_config::TabBarConfig,
}

impl TabBar {
    pub fn new(config: niri_config::TabBarConfig) -> Self {
        Self {
            tab_rects: Vec::new(),
            open_anim: None,
            config,
        }
    }

    pub fn update_config(&mut self, config: niri_config::TabBarConfig) {
        self.config = config;
    }

    pub fn update_shaders(&mut self) {
        // No shaders to update for the basic bar.
    }

    pub fn advance_animations(&mut self) {
        if let Some(anim) = &mut self.open_anim {
            if anim.is_done() {
                self.open_anim = None;
            }
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.open_anim.is_some()
    }

    pub fn start_open_animation(&mut self, clock: Clock, config: niri_config::Animation) {
        self.open_anim = Some(Animation::new(clock, 0., 1., 0., config));
    }

    /// Extra size taken up by the tab bar (height + gap).
    pub fn extra_size(&self, tab_count: usize, scale: f64) -> Size<f64, Logical> {
        if self.config.off || (self.config.height <= 0.) {
            return Size::from((0., 0.));
        }
        Size::from((0., self.config.height + 1.))
    }

    /// Content offset — shifts tab content to make room for the bar.
    pub fn content_offset(&self, tab_count: usize, scale: f64) -> Point<f64, Logical> {
        if self.config.off {
            return Point::from((0., 0.));
        }
        match self.config.position {
            niri_config::TabBarPosition::Top => {
                Point::from((0., self.config.height + 1.))
            }
            niri_config::TabBarPosition::Bottom => Point::from((0., 0.)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_render_elements(
        &mut self,
        enabled: bool,
        area: Rectangle<f64, Logical>,
        _area_view_rect: Rectangle<f64, Logical>,
        tab_count: usize,
        tabs: impl Iterator<Item = TabInfo>,
        is_active: bool,
        scale: f64,
    ) {
        if !enabled || self.config.off {
            self.tab_rects.clear();
            return;
        }

        let count = tab_count;
        self.tab_rects.resize_with(count, Default::default);

        // Distribute tabs evenly across the bar width.
        let bar_height = self.config.height;
        let gap = self.config.gaps_between_tabs;
        let total_gap = gap * (count as f64 - 1.0).max(0.);
        let available_width = area.size.w - total_gap;
        let tab_width = (available_width / count as f64).max(1.);

        let bar_y = match self.config.position {
            niri_config::TabBarPosition::Top => area.loc.y,
            niri_config::TabBarPosition::Bottom => area.loc.y + area.size.h - bar_height,
        };

        let progress = self.open_anim.as_ref().map_or(1., |a| a.value().max(0.));
        let bar_height = bar_height * progress;

        for (i, (tab, rect)) in tabs.zip(self.tab_rects.iter_mut()).enumerate() {
            let x = area.loc.x + i as f64 * (tab_width + gap);
            *rect = Rectangle::new(
                Point::from((x, bar_y)),
                Size::from((tab_width, bar_height)),
            );
        }
    }

    pub fn render<R: NiriRenderer>(
        &self,
        _renderer: &mut R,
        pos: Point<f64, Logical>,
        push: &mut dyn FnMut(TabBarRenderElement),
    ) {
        // TODO: render tab backgrounds with active/inactive colors and text labels.
        // For now, this is a structural skeleton. The actual rendering requires
        // pangocairo text texture generation and GPU texture management.
    }

    /// Hit-test against tab rectangles. Returns the tab index if hit.
    pub fn hit(
        &self,
        area: Rectangle<f64, Logical>,
        count: usize,
        scale: f64,
        pos: Point<f64, Logical>,
    ) -> Option<usize> {
        for (i, rect) in self.tab_rects.iter().enumerate() {
            if rect.contains(pos) {
                return Some(i);
            }
        }
        None
    }

    pub fn config(&self) -> &niri_config::TabBarConfig {
        &self.config
    }
}
