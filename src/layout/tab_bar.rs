use std::cell::RefCell;

use pango::FontDescription;
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{round_logical_in_physical, to_physical_precise_round};

/// Minimum (target) width of a single tab. Tabs never shrink below this; once the
/// available width can't fit every tab at this width, the bar overflows and scrolls.
const MIN_TAB_WIDTH: f64 = 100.;

/// Horizontal padding between a tab's edge and its title text.
const TITLE_PADDING: f64 = 4.;

use super::tab_indicator::TabInfo;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;

niri_render_elements! {
    TabBarRenderElement => {
        Background = SolidColorRenderElement,
        Text = PrimaryGpuTextureRenderElement,
    }
}

/// Cached title texture for a single tab.
///
/// The text color is baked into the texture, so it's part of the cache key.
#[derive(Debug, Default)]
struct CachedTitle {
    title: RefCell<String>,
    scale: RefCell<f64>,
    color: RefCell<[f32; 4]>,
    texture: RefCell<Option<Option<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>>>>,
}

impl CachedTitle {
    fn get(
        &self,
        renderer: &mut GlesRenderer,
        title: &str,
        scale: f64,
        color: [f32; 4],
        font: &str,
    ) -> Option<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>> {
        if *self.title.borrow() != title
            || *self.scale.borrow() != scale
            || *self.color.borrow() != color
        {
            *self.texture.borrow_mut() = None;
            *self.title.borrow_mut() = title.to_owned();
            *self.scale.borrow_mut() = scale;
            *self.color.borrow_mut() = color;
        }

        let mut tex = self.texture.borrow_mut();
        tex.get_or_insert_with(|| {
            generate_title_texture(renderer, title, scale, color, font).ok()
        })
        .clone()
    }
}

/// Generate a title texture via pangocairo.
fn generate_title_texture(
    renderer: &mut GlesRenderer,
    title: &str,
    scale: f64,
    color: [f32; 4],
    font_desc: &str,
) -> anyhow::Result<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>> {
    let mut font = FontDescription::from_string(font_desc);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_single_paragraph_mode(true);
    layout.set_font_description(Some(&font));
    layout.set_text(title);

    let (width, height) = layout.pixel_size();
    if width == 0 || height == 0 {
        anyhow::bail!("empty title texture");
    }

    let width = width.min(16383);
    let height = height.min(16383);

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    let [r, g, b, a] = color;
    cr.set_source_rgba(r as f64, g as f64, b as f64, a as f64);
    pangocairo::functions::show_layout(&cr, &layout);

    drop(cr);
    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
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
    /// Cached title textures for each tab.
    cached_titles: RefCell<Vec<CachedTitle>>,
    /// Persistent background buffers for each tab. Kept across frames so their buffer
    /// Ids stay stable and damage tracking works; only resized/recolored on change.
    backgrounds: RefCell<Vec<SolidColorBuffer>>,
    /// Cached geometry for each tab (computed during update_render_elements).
    tab_rects: Vec<Rectangle<f64, Logical>>,
    /// Index of the active tab (set during update_render_elements).
    active_idx: usize,
    /// Whether textures need regeneration (scale changed).
    cached_scale: f64,
    /// Horizontal scroll offset for overflowing tabs (in logical pixels).
    scroll_offset: f64,
    /// Total width of all tabs (including gaps), for computing max_scroll.
    total_tabs_width: f64,
    /// Open animation.
    open_anim: Option<Animation>,
    /// Whether this header is for a Stacked layout (one full-width title row per tab, stacked
    /// vertically) rather than a Tabbed layout (a single row of side-by-side tabs).
    stacked: bool,
    /// Config.
    config: niri_config::TabBarConfig,
}

impl TabBar {
    pub fn new(config: niri_config::TabBarConfig) -> Self {
        Self {
            cached_titles: RefCell::new(Vec::new()),
            backgrounds: RefCell::new(Vec::new()),
            tab_rects: Vec::new(),
            active_idx: 0,
            cached_scale: 0.,
            scroll_offset: 0.,
            total_tabs_width: 0.,
            open_anim: None,
            stacked: false,
            config,
        }
    }

    pub fn set_stacked(&mut self, stacked: bool) {
        self.stacked = stacked;
    }

    pub fn update_config(&mut self, config: niri_config::TabBarConfig) {
        self.config = config;
        self.cached_titles.borrow_mut().clear();
    }

    pub fn update_shaders(&mut self) {
        self.cached_titles.borrow_mut().clear();
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

    /// Extra size taken up by the tab bar (rounded bar height plus a 1px gap). A Stacked layout
    /// reserves one title row per tab; a Tabbed layout reserves a single row.
    pub fn extra_size(&self, tab_count: usize, scale: f64) -> Size<f64, Logical> {
        if self.config.off || (self.config.height <= 0.) {
            return Size::from((0., 0.));
        }
        let height = round_logical_in_physical(scale, self.config.height);
        let rows = if self.stacked { tab_count.max(1) as f64 } else { 1. };
        Size::from((0., height * rows + 1.))
    }

    /// Content offset — shifts tab content to make room for the bar.
    pub fn content_offset(&self, tab_count: usize, scale: f64) -> Point<f64, Logical> {
        if self.config.off {
            return Point::from((0., 0.));
        }
        match self.config.position {
            niri_config::TabBarPosition::Top => self.extra_size(tab_count, scale).to_point(),
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
        _is_active: bool,
        scale: f64,
    ) {
        if !enabled || self.config.off {
            self.tab_rects.clear();
            return;
        }

        // Invalidate textures if scale changed.
        if self.cached_scale != scale {
            self.cached_titles.borrow_mut().clear();
            self.cached_scale = scale;
        }

        // Ensure cached_titles has the right number of entries.
        self.cached_titles.borrow_mut().resize_with(tab_count, Default::default);

        let count = tab_count;
        self.tab_rects.resize_with(count, Default::default);

        let round = |logical: f64| round_logical_in_physical(scale, logical);
        let extra_size = self.extra_size(count, scale);

        // Distribute tabs across the bar width. Each tab targets at least MIN_TAB_WIDTH;
        // a few tabs fill the bar, but many tabs overflow and the bar becomes scrollable.
        let gap = round(self.config.gaps_between_tabs);
        let total_gap = gap * (count as f64 - 1.0).max(0.);
        let available_width = area.size.w;
        let ideal_width = (available_width - total_gap) / count as f64;
        let tab_width = round(ideal_width.max(MIN_TAB_WIDTH));

        // The area origin already includes the bar's content offset (it's shifted past the
        // reserved band), so the band sits *outside* the area: above it for Top, below it for
        // Bottom. Draw the bar into that reserved band rather than over the content.
        let bar_y = match self.config.position {
            niri_config::TabBarPosition::Top => area.loc.y - extra_size.h,
            niri_config::TabBarPosition::Bottom => {
                area.loc.y + area.size.h + (extra_size.h - round(self.config.height))
            }
        };
        let bar_y = round(bar_y);

        let progress = self.open_anim.as_ref().map_or(1., |a| a.value().max(0.));
        let bar_height = round(self.config.height * progress);
        let row_height = round(self.config.height);

        if self.stacked {
            // Stacked: one full-width title row per tab, stacked vertically in the reserved band.
            for (i, (tab, rect)) in tabs.zip(self.tab_rects.iter_mut()).enumerate() {
                if tab.is_active {
                    self.active_idx = i;
                }
                let y = round(bar_y + i as f64 * row_height);
                *rect = Rectangle::new(
                    Point::from((area.loc.x, y)),
                    Size::from((available_width, bar_height)),
                );
            }
            // No horizontal scrolling for stacked rows.
            self.total_tabs_width = available_width;
            self.scroll_offset = 0.;
            return;
        }

        // Tabbed: a single row of side-by-side tabs (without scroll offset).
        for (i, (tab, rect)) in tabs.zip(self.tab_rects.iter_mut()).enumerate() {
            if tab.is_active {
                self.active_idx = i;
            }
            let x = round(area.loc.x + i as f64 * (tab_width + gap));
            *rect = Rectangle::new(
                Point::from((x, bar_y)),
                Size::from((tab_width, bar_height)),
            );
        }

        // Compute total tabs width and max scroll offset.
        self.total_tabs_width = count as f64 * tab_width + total_gap;
        let max_scroll = (self.total_tabs_width - available_width).max(0.);

        // Auto-scroll to keep the active tab visible.
        if count > 0 {
            let active_rect = &self.tab_rects[self.active_idx];
            let active_left = active_rect.loc.x - area.loc.x;
            let active_right = active_left + active_rect.size.w;
            if active_left < self.scroll_offset {
                // Active tab is scrolled off the left; scroll to show it.
                self.scroll_offset = active_left;
            } else if active_right > self.scroll_offset + available_width {
                // Active tab is scrolled off the right; scroll to show it.
                self.scroll_offset = active_right - available_width;
            }
        }

        // Clamp scroll offset.
        self.scroll_offset = self.scroll_offset.clamp(0., max_scroll);

        // Apply scroll offset to all tab rects.
        for rect in self.tab_rects.iter_mut() {
            rect.loc.x -= self.scroll_offset;
        }
    }

    /// Renders the tab bar backgrounds.
    pub fn render_backgrounds(
        &self,
        pos: Point<f64, Logical>,
        is_column_active: bool,
        push: &mut dyn FnMut(TabBarRenderElement),
    ) {
        if self.config.off || self.tab_rects.is_empty() {
            return;
        }

        // Keep one persistent buffer per tab so buffer Ids are stable across frames; only
        // resize/recolor on change, otherwise damage tracking is defeated and we full-repaint.
        let mut backgrounds = self.backgrounds.borrow_mut();
        backgrounds.resize_with(self.tab_rects.len(), SolidColorBuffer::default);

        for (i, rect) in self.tab_rects.iter().enumerate() {
            let tab_pos = pos + rect.loc;
            let tab_size = rect.size;

            // Tab background color.
            let bg_color = if i == self.active_idx && is_column_active {
                self.config.active_color
                    .unwrap_or(niri_config::Color::new_unpremul(0.35, 0.35, 0.35, 1.))
            } else {
                self.config.inactive_color
                    .unwrap_or(niri_config::Color::new_unpremul(0.2, 0.2, 0.2, 1.))
            };

            let buffer = &mut backgrounds[i];
            buffer.update(tab_size, bg_color);
            let elem = SolidColorRenderElement::from_buffer(
                buffer,
                tab_pos,
                1.0,
                Kind::Unspecified,
            );
            push(TabBarRenderElement::Background(elem));
        }
    }

    /// Renders the tab bar: title textures, then backgrounds behind them.
    /// Texture caching uses interior mutability (RefCell), so this only needs &self.
    ///
    /// Render elements are ordered front-to-back (the first pushed is topmost), so the
    /// titles must be pushed *before* the backgrounds; otherwise the opaque background
    /// rectangles occlude the text and you get blank grey boxes.
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        pos: Point<f64, Logical>,
        scale: f64,
        is_column_active: bool,
        titles: &[&str],
        push: &mut dyn FnMut(TabBarRenderElement),
    ) {
        self.render_titles(renderer, pos, scale, is_column_active, titles, push);
        self.render_backgrounds(pos, is_column_active, push);
    }

    /// Renders cached title textures for each tab.
    /// Call after render_backgrounds.
    pub fn render_titles(
        &self,
        renderer: &mut GlesRenderer,
        pos: Point<f64, Logical>,
        scale: f64,
        is_column_active: bool,
        titles: &[&str],
        push: &mut dyn FnMut(TabBarRenderElement),
    ) {
        if self.config.off || self.tab_rects.is_empty() {
            return;
        }

        let cached_titles = self.cached_titles.borrow();
        // Ensure we have enough cached entries.
        if cached_titles.len() < titles.len() {
            drop(cached_titles);
            self.cached_titles.borrow_mut().resize_with(titles.len(), Default::default);
        }

        let cached_titles = self.cached_titles.borrow();
        for (i, rect) in self.tab_rects.iter().enumerate() {
            if i >= titles.len() {
                break;
            }
            let title = titles[i];
            if title.is_empty() {
                continue;
            }

            if i >= cached_titles.len() {
                break;
            }

            // Text color: active vs inactive, falling back to the shared text_color.
            let text_color = if i == self.active_idx && is_column_active {
                self.config.active_text_color.unwrap_or(self.config.text_color)
            } else {
                self.config.inactive_text_color.unwrap_or(self.config.text_color)
            };
            let color = text_color.to_array_unpremul();

            if let Some(texture) = cached_titles[i].get(renderer, title, scale, color, &self.config.font)
            {
                let text_size = texture.logical_size();
                let text_pos: Point<f64, Logical> = Point::from((
                    pos.x + rect.loc.x + TITLE_PADDING,
                    pos.y + rect.loc.y + (rect.size.h - text_size.h) / 2.,
                ));

                // Clip the title to the tab width (minus padding on both sides) so long
                // titles don't bleed into the neighbouring tab.
                let max_width = (rect.size.w - 2. * TITLE_PADDING).max(0.);
                let src = if text_size.w > max_width {
                    Some(Rectangle::from_size(Size::from((max_width, text_size.h))))
                } else {
                    None
                };

                let text_elem = TextureRenderElement::from_texture_buffer(
                    texture,
                    text_pos,
                    1.0,
                    src,
                    None,
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
                push(TabBarRenderElement::Text(PrimaryGpuTextureRenderElement(
                    text_elem,
                )));
            }
        }
    }

    /// Handle a scroll event over the tab bar.
    /// `delta` is the scroll amount in logical pixels (positive = scroll right).
    /// Returns true if the scroll offset changed.
    pub fn scroll(&mut self, delta: f64, area_width: f64) -> bool {
        let max_scroll = (self.total_tabs_width - area_width).max(0.);
        let new_offset = (self.scroll_offset + delta).clamp(0., max_scroll);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            true
        } else {
            false
        }
    }

    /// Hit-test against tab rectangles. Returns the tab index if hit.
    pub fn hit(
        &self,
        _area: Rectangle<f64, Logical>,
        _count: usize,
        _scale: f64,
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
