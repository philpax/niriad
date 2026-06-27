use std::cell::RefCell;

use pango::FontDescription;
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportMem;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::to_physical_precise_round;

use super::tab_indicator::TabInfo;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;

niri_render_elements! {
    TabBarRenderElement => {
        Background = SolidColorRenderElement,
        Text = PrimaryGpuTextureRenderElement,
    }
}

/// Cached title texture for a single tab.
#[derive(Debug, Default)]
struct CachedTitle {
    title: RefCell<String>,
    scale: RefCell<f64>,
    texture: RefCell<Option<Option<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>>>>,
}

impl CachedTitle {
    fn get(
        &self,
        renderer: &mut GlesRenderer,
        title: &str,
        scale: f64,
        font: &str,
    ) -> Option<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>> {
        if *self.title.borrow() != title || *self.scale.borrow() != scale {
            *self.texture.borrow_mut() = None;
            *self.title.borrow_mut() = title.to_owned();
            *self.scale.borrow_mut() = scale;
        }

        let mut tex = self.texture.borrow_mut();
        tex.get_or_insert_with(|| {
            generate_title_texture(renderer, title, scale, font).ok()
        })
        .clone()
    }
}

/// Generate a title texture via pangocairo.
fn generate_title_texture(
    renderer: &mut GlesRenderer,
    title: &str,
    scale: f64,
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
    cr.set_source_rgb(1., 1., 1.);
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
    /// Cached geometry for each tab (computed during update_render_elements).
    tab_rects: Vec<Rectangle<f64, Logical>>,
    /// Index of the active tab (set during update_render_elements).
    active_idx: usize,
    /// Whether textures need regeneration (scale changed).
    cached_scale: f64,
    /// Open animation.
    open_anim: Option<Animation>,
    /// Config.
    config: niri_config::TabBarConfig,
}

impl TabBar {
    pub fn new(config: niri_config::TabBarConfig) -> Self {
        Self {
            cached_titles: RefCell::new(Vec::new()),
            tab_rects: Vec::new(),
            active_idx: 0,
            cached_scale: 0.,
            open_anim: None,
            config,
        }
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

        // Invalidate textures if scale changed.
        if self.cached_scale != scale {
            self.cached_titles.borrow_mut().clear();
            self.cached_scale = scale;
        }

        // Ensure cached_titles has the right number of entries.
        self.cached_titles.borrow_mut().resize_with(tab_count, Default::default);

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
            if tab.is_active {
                self.active_idx = i;
            }
            let x = area.loc.x + i as f64 * (tab_width + gap);
            *rect = Rectangle::new(
                Point::from((x, bar_y)),
                Size::from((tab_width, bar_height)),
            );
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

            let buffer = SolidColorBuffer::new(tab_size, bg_color);
            let elem = SolidColorRenderElement::from_buffer(
                &buffer,
                tab_pos,
                1.0,
                Kind::Unspecified,
            );
            push(TabBarRenderElement::Background(elem));
        }
    }

    /// Renders the tab bar: backgrounds first, then title textures on top.
    /// Texture caching uses interior mutability (RefCell), so this only needs &self.
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        pos: Point<f64, Logical>,
        scale: f64,
        is_column_active: bool,
        titles: &[&str],
        push: &mut dyn FnMut(TabBarRenderElement),
    ) {
        self.render_backgrounds(pos, is_column_active, push);
        self.render_titles(renderer, pos, scale, titles, push);
    }

    /// Renders cached title textures for each tab.
    /// Call after render_backgrounds.
    pub fn render_titles(
        &self,
        renderer: &mut GlesRenderer,
        pos: Point<f64, Logical>,
        scale: f64,
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

            if let Some(texture) = cached_titles[i].get(renderer, title, scale, &self.config.font) {
                let text_size = texture.logical_size();
                let text_pos: Point<f64, Logical> = Point::from((
                    pos.x + rect.loc.x + 4.,
                    pos.y + rect.loc.y + (rect.size.h - text_size.h) / 2.,
                ));
                let text_elem = TextureRenderElement::from_texture_buffer(
                    texture,
                    text_pos,
                    1.0,
                    None,
                    None,
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
                push(TabBarRenderElement::Text(PrimaryGpuTextureRenderElement(
                    text_elem,
                )));
            }
        }
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
