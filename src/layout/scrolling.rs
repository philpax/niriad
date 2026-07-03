use std::cmp::{max, min};
use std::iter::{self, zip};
use std::rc::Rc;
use std::time::Duration;

use niri_config::utils::MergeWith as _;
use niri_config::{CenterFocusedSection, PresetSize, Struts, TilingDrag};
use niri_ipc::{SectionDisplay, SizeChange, WindowLayout};
use ordered_float::NotNan;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};

use super::axis::AxisMap;
use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::monitor::InsertPosition;
use super::tab_indicator::{TabHeader, TabIndicator, TabIndicatorRenderElement, TabInfo};
use super::tab_bar::TabBarRenderElement;
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::tile_node::{ChildSpan, Layout, SplitAxis, SplitChildData, TileNode, TilePath};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{ConfigureIntent, HitType, InteractiveResizeData, LayoutElement, Options, RemovedTile};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::SizingMode;
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::ResizeEdge;
use crate::window::ResolvedWindowRules;

/// Amount of touchpad movement to scroll the view for the main-axis span of one working area.
const VIEW_GESTURE_WORKING_AREA_MOVEMENT: f64 = 1200.;

/// A vector in scrolling-space coordinates, where X is main axis and Y is cross axis.
fn main_space_vec(main: f64) -> Point<f64, Logical> {
    Point::from((main, 0.))
}

/// A vector in scrolling-space coordinates, where X is main axis and Y is cross axis.
fn cross_space_vec(cross: f64) -> Point<f64, Logical> {
    Point::from((0., cross))
}

/// A scrollable-tiling space for windows.
#[derive(Debug)]
pub struct ScrollingSpace<W: LayoutElement> {
    /// Sections of windows on this space.
    sections: Vec<Section<W>>,

    /// Index of the currently active section, if any.
    active_section_idx: usize,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Offset of the view computed from the active section.
    ///
    /// Any gaps, including left padding from work area left exclusive zone, is handled
    /// with this view offset (rather than added as a constant elsewhere in the code). This allows
    /// for natural handling of fullscreen windows, which must ignore work area padding.
    view_offset: ViewOffset,

    /// Whether to activate the previous, rather than the next, section upon section removal.
    ///
    /// When a new section is created and removed with no focus changes in-between, it is more
    /// natural to activate the previously-focused section. This variable tracks that.
    ///
    /// Since we only create-and-activate sections immediately to the right of the active section (in
    /// contrast to tabs in Firefox, for example), we can track this as a bool, rather than an
    /// index of the previous section to activate.
    ///
    /// The value is the view offset that the previous section had before, to restore it.
    activate_prev_section_on_removal: Option<f64>,

    /// View offset to restore after unfullscreening or unmaximizing.
    view_offset_to_restore: Option<f64>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    ///
    /// Takes into account layer-shell exclusive zones and niri struts.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this space excluding struts.
    ///
    /// Used for popup unconstraining. Popups can go over struts, but they shouldn't go over
    /// the layer-shell top layer (which renders on top of popups).
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

niri_render_elements! {
    ScrollingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
        TabIndicator = TabIndicatorRenderElement,
        TabBar = TabBarRenderElement,
    }
}

#[derive(Debug)]
pub(super) enum ViewOffset {
    /// The view offset is static.
    Static(f64),
    /// The view offset is animating.
    Animation(Animation),
    /// The view offset is controlled by the ongoing gesture.
    Gesture(ViewGesture),
}

#[derive(Debug)]
pub(super) struct ViewGesture {
    current_view_offset: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, when we need to activate a specific window during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    delta_from_tracker: f64,
    // The view offset we'll use if needed for activate_prev_section_on_removal.
    stationary_view_offset: f64,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
}

#[derive(Debug)]
pub struct Section<W: LayoutElement> {
    /// Root of the recursive tile tree: a `Leaf` (single window) or an `Internal` node arranging
    /// its children by `Layout` (split, tabbed, or stacked), nested arbitrarily.
    ///
    /// Must be non-empty (at least one leaf).
    root: TileNode<W>,

    /// Desired width of this section.
    ///
    /// If the section is full-width or full-screened, this is the width that should be restored
    /// upon unfullscreening and untoggling full-width.
    width: SectionWidth,

    /// Currently selected preset width index.
    preset_width_idx: Option<usize>,

    /// Whether this section is full-width.
    is_full_width: bool,

    /// Whether this section is going to be fullscreen.
    ///
    /// This is the compositor-side fullscreen state, so it changes immediately upon
    /// set_fullscreen(). The actual tiles will take some time to respond to the fullscreen request
    /// and become fullscreen.
    ///
    /// Similarly, unsetting fullscreen will change this value to false immediately, and tiles will
    /// take some time to catch up and actually unfullscreen.
    is_pending_fullscreen: bool,

    /// Whether this section is going to be maximized.
    ///
    /// Can be `true` together with `is_pending_fullscreen`, which means that the section is
    /// effectively pending fullscreen, but unfullscreening should go back to maximized state,
    /// rather than normal.
    is_pending_maximized: bool,

    /// Animation of the render offset during window swapping.
    move_animation: Option<MoveAnimation>,

    /// Latest known view size for this section's workspace.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this section's workspace.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this section's workspace excluding struts.
    ///
    /// Used for maximize-to-edges.
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the section is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Pending split direction for the split-then-open interaction model.
    pending_split_direction: Option<SplitAxis>,
}

/// Main-axis span of a section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SectionWidth {
    /// Proportion of the current view along the main axis.
    Proportion(f64),
    /// Fixed main-axis span in logical pixels.
    Fixed(f64),
}

/// Cross-axis span of a window in a section.
///
/// Every window but one in a section must be `Auto`-sized so that the total cross-axis span can add
/// up to the workspace cross-axis span. Resizing a window converts all other windows to `Auto`,
/// weighted to preserve their visual spans at the moment of the conversion.
///
/// In contrast to section widths, proportional cross-axis changes are converted to, and stored as,
/// fixed spans right away. With section widths you frequently want e.g. two sections side-by-side
/// with 50% main-axis span each, and you want them to remain this way when moving to a differently
/// sized monitor. Windows in a section, however, already auto-size to fill the available cross-axis
/// span, giving you this behavior. The main reason to set a different window cross-axis span,
/// then, is when you want something in the window to fit exactly, e.g. to fit 30 lines in a
/// terminal, which corresponds to the `Fixed` variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    /// Automatically computed *tile* cross span, distributed across the section according to
    /// weights.
    ///
    /// This controls the tile cross span rather than the window cross span because it's easier in
    /// the auto-size distribution algorithm.
    Auto { weight: f64 },
    /// Fixed *window* cross span in logical pixels.
    Fixed(f64),
    /// One of the preset cross spans (tile or window).
    Preset(usize),
}

/// Horizontal direction for an operation.
///
/// As operations often have a symmetrical counterpart, e.g. focus-right/focus-left, methods
/// on `Scrolling` can sometimes be factored using the direction of the operation as a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDirection {
    Left,
    Right,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
}

/// Render data for one *nested* (non-root) tabbed container within a section. Lets the renderer,
/// hit-tester, and element updater treat a tabbed node deep in the tree (e.g. a tabbed row) like a
/// mini tabbed section.
struct NestedTabbed {
    /// Path from the section root to the tabbed node.
    path: TilePath,
    /// Content rectangle (section-local); the header draws in the band just above it.
    content_area: Rectangle<f64, Logical>,
    /// Number of tabs (the node's direct children).
    tab_count: usize,
    /// The active tab index.
    active_idx: usize,
    /// Whether the node is currently shown (not hidden by an ancestor tab).
    visible: bool,
    /// Whether the node lies on the section's active path (used for the active highlight).
    active_on_path: bool,
    /// Flat leaf index of each tab's representative leaf (its subtree's active leaf).
    rep_leaf_idx: Vec<usize>,
}

/// Per-tab render info for one direct child of a tabbing container.
struct TabChild {
    /// Flat leaf index of this tab's representative leaf (its subtree's active leaf).
    rep_leaf_idx: usize,
    /// Union (bounding box) of every leaf under this tab's subtree (section-local coords).
    geometry: Rectangle<f64, Logical>,
    /// Tab label: the window title for a leaf child, or a group label (e.g. `H[2]`) for a nested
    /// container child.
    title: String,
}

#[derive(Debug, Clone, Copy)]
struct ViewSnap {
    view_main_pos: f64,
    col_idx: usize,
}

impl<W: LayoutElement> ScrollingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let axis = AxisMap::new(options.layout.main_axis);
        let view_size = axis.size_in(view_size);
        let parent_area = axis.rect_in(parent_area);
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        Self {
            sections: Vec::new(),
            active_section_idx: 0,
            interactive_resize: None,
            view_offset: ViewOffset::Static(0.),
            activate_prev_section_on_removal: None,
            view_offset_to_restore: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let axis = AxisMap::new(options.layout.main_axis);
        let view_size = axis.size_in(view_size);
        let parent_area = axis.rect_in(parent_area);
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        for section in &mut self.sections {
            section.update_config(view_size, working_area, parent_area, scale, options.clone());
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        // Apply always-center and such right away.
        if !self.sections.is_empty() && !self.view_offset.is_gesture() {
            self.animate_view_offset_to_section(None, self.active_section_idx, None);
        }
    }

    fn axis(&self) -> AxisMap {
        AxisMap::new(self.options.layout.main_axis)
    }

    fn map_point_in(&self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        self.axis().point_in(point)
    }

    fn map_point_out(&self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        self.axis().point_out(point)
    }

    fn map_size_out(&self, size: Size<f64, Logical>) -> Size<f64, Logical> {
        self.axis().size_out(size)
    }

    fn map_size_i32_out(&self, size: Size<i32, Logical>) -> Size<i32, Logical> {
        self.axis().size_out(size)
    }

    fn map_rect_out(&self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        self.axis().rect_out(rect)
    }

    pub fn update_shaders(&mut self) {
        for col in &mut self.sections {
            col.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let ViewOffset::Animation(anim) = &self.view_offset {
            if anim.is_done() {
                self.view_offset = ViewOffset::Static(anim.to());
            }
        }

        if let ViewOffset::Gesture(gesture) = &mut self.view_offset {
            // Make sure the last event time doesn't go too much out of date (for
            // workspaces not under cursor), causing sudden jumps.
            //
            // This happens after any dnd_scroll_gesture_scroll() calls (in
            // Layout::advance_animations()), so it doesn't mess up the time delta there.
            if let Some(last_time) = &mut gesture.dnd_last_event_time {
                let now = self.clock.now_unadjusted();
                if *last_time != now {
                    *last_time = now;

                    // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                    // updated the gesture already. Therefore, when this code runs, the pointer
                    // must be outside the DnD scrolling zone.
                    gesture.dnd_nonzero_start_time = None;
                }
            }

            if let Some(anim) = &mut gesture.animation {
                if anim.is_done() {
                    gesture.animation = None;
                }
            }
        }

        for col in &mut self.sections {
            col.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing()
            || self.sections.iter().any(Section::are_animations_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        !self.view_offset.is_static()
            || self.sections.iter().any(Section::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let view_main_offset = main_space_vec(self.view_main_pos());
        let view_size = self.view_size;
        let active_idx = self.active_section_idx;
        for (col_idx, (col, section_main)) in self.sections_mut().enumerate() {
            let is_active = is_active && col_idx == active_idx;
            let section_offset = main_space_vec(section_main);
            let section_pos = view_main_offset - section_offset - col.render_offset();
            let view_rect = Rectangle::new(section_pos, view_size);
            col.update_render_elements(is_active, view_rect);
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.sections.iter().flat_map(|col| col.tiles_enumerated().map(|(_, tile)| tile))
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.sections
            .iter_mut()
            .flat_map(|col| col.tiles_enumerated_mut().map(|(_, tile)| tile))
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.sections.is_empty() {
            return None;
        }

        let col = &self.sections[self.active_section_idx];
        Some(col.active_tile().window())
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.sections.is_empty() {
            return None;
        }

        let col = &mut self.sections[self.active_section_idx];
        Some(col.active_tile_mut().window_mut())
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        if self.sections.is_empty() {
            return None;
        }

        let col = &mut self.sections[self.active_section_idx];
        Some(col.active_tile_mut())
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        let col = &self.sections[self.active_section_idx];
        col.pending_sizing_mode().is_fullscreen()
    }

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_section_display
            .unwrap_or(self.options.layout.default_section_display);
        let will_tab = display_mode == SectionDisplay::Tabbed;
        let extra_size = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        let bounds = compute_toplevel_bounds(
            border_config,
            self.working_area.size,
            extra_size,
            self.options.layout.gaps,
        );
        self.map_size_i32_out(bounds)
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_section_display
            .unwrap_or(self.options.layout.default_section_display);
        let will_tab = display_mode == SectionDisplay::Tabbed;
        let extra = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        let working_size = self.working_area.size;

        let width = if let Some(size) = width {
            let size = match resolve_preset_size(size, &self.options, working_size.w, extra.w) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };

            max(1, size.floor() as i32)
        } else {
            0
        };

        let mut full_height = self.working_area.size.h - self.options.layout.gaps * 2.;
        if !border.off {
            full_height -= border.width * 2.;
        }

        let height = if let Some(height) = height {
            let height = match resolve_preset_size(height, &self.options, working_size.h, extra.h) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };
            f64::min(height, full_height)
        } else {
            full_height
        };

        let size = Size::from((width, max(height.floor() as i32, 1)));
        self.map_size_i32_out(size)
    }

    pub fn is_centering_focused_section(&self) -> bool {
        self.options.layout.center_focused_section == CenterFocusedSection::Always
            || (self.options.layout.always_center_single_section && self.sections.len() <= 1)
    }

    fn compute_new_view_offset_fit(
        &self,
        target_view_main: Option<f64>,
        section_main: f64,
        section_span: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return 0.;
        }

        let (work_area, padding) = if mode.is_maximized() {
            (self.parent_area, 0.)
        } else {
            (self.working_area, self.options.layout.gaps)
        };

        let target_view_main = target_view_main.unwrap_or_else(|| self.target_view_main_pos());

        let new_offset = compute_new_view_offset(
            target_view_main + work_area.loc.x,
            work_area.size.w,
            section_main,
            section_span,
            padding,
        );

        // Non-fullscreen windows are always offset at least by the working area position.
        new_offset - work_area.loc.x
    }

    fn compute_new_view_offset_centered(
        &self,
        target_view_main: Option<f64>,
        section_main: f64,
        section_span: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return self.compute_new_view_offset_fit(
                target_view_main,
                section_main,
                section_span,
                mode,
            );
        }

        let work_area = if mode.is_maximized() {
            self.parent_area
        } else {
            self.working_area
        };

        // Sections wider than the view are aligned to the start edge (the fit code can deal with
        // that).
        if work_area.size.w <= section_span {
            return self.compute_new_view_offset_fit(
                target_view_main,
                section_main,
                section_span,
                mode,
            );
        }

        -(work_area.size.w - section_span) / 2. - work_area.loc.x
    }

    fn compute_new_view_offset_for_section_fit(
        &self,
        target_view_main: Option<f64>,
        idx: usize,
    ) -> f64 {
        let col = &self.sections[idx];
        self.compute_new_view_offset_fit(
            target_view_main,
            self.section_main_pos(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_section_centered(
        &self,
        target_view_main: Option<f64>,
        idx: usize,
    ) -> f64 {
        let col = &self.sections[idx];
        self.compute_new_view_offset_centered(
            target_view_main,
            self.section_main_pos(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_section(
        &self,
        target_view_main: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) -> f64 {
        if self.is_centering_focused_section() {
            return self.compute_new_view_offset_for_section_centered(target_view_main, idx);
        }

        match self.options.layout.center_focused_section {
            CenterFocusedSection::Always => {
                self.compute_new_view_offset_for_section_centered(target_view_main, idx)
            }
            CenterFocusedSection::OnOverflow => {
                let Some(prev_idx) = prev_idx else {
                    return self.compute_new_view_offset_for_section_fit(target_view_main, idx);
                };

                // Activating the same section.
                if prev_idx == idx {
                    return self.compute_new_view_offset_for_section_fit(target_view_main, idx);
                }

                // Always take the left or right neighbor of the target as the source.
                let source_idx = if prev_idx > idx {
                    min(idx + 1, self.sections.len() - 1)
                } else {
                    idx.saturating_sub(1)
                };

                let source_section_main = self.section_main_pos(source_idx);
                let source_section_span = self.sections[source_idx].width();

                let target_section_main = self.section_main_pos(idx);
                let target_section_span = self.sections[idx].width();

                // NOTE: This logic won't work entirely correctly with small fixed-size maximized
                // windows (they have a different area and padding).
                let combined_span = if source_section_main < target_section_main {
                    // Source is before target along the main axis.
                    target_section_main - source_section_main + target_section_span
                } else {
                    // Source is after target along the main axis.
                    source_section_main - target_section_main + source_section_span
                } + self.options.layout.gaps * 2.;

                // If it fits together, do a normal animation, otherwise center the new section.
                if combined_span <= self.working_area.size.w {
                    self.compute_new_view_offset_for_section_fit(target_view_main, idx)
                } else {
                    self.compute_new_view_offset_for_section_centered(target_view_main, idx)
                }
            }
            CenterFocusedSection::Never => {
                self.compute_new_view_offset_for_section_fit(target_view_main, idx)
            }
        }
    }

    fn push_snap_if_between_bounds(
        snaps: &mut Vec<ViewSnap>,
        view_main_pos: f64,
        col_idx: usize,
        startmost_snap: f64,
        endmost_snap: f64,
    ) {
        if startmost_snap < view_main_pos && view_main_pos < endmost_snap {
            snaps.push(ViewSnap {
                view_main_pos,
                col_idx,
            });
        }
    }

    fn aligned_section_snap_range(
        &self,
        section_main: f64,
        col: &Section<W>,
        prev_section_span: Option<f64>,
        next_section_span: Option<f64>,
    ) -> (f64, f64) {
        let center_on_overflow = matches!(
            self.options.layout.center_focused_section,
            CenterFocusedSection::OnOverflow
        );

        let view_main_span = self.view_size.w;
        let gaps = self.options.layout.gaps;
        let section_span = col.width();
        let mode = col.sizing_mode();

        let work_area = if mode.is_maximized() {
            self.parent_area
        } else {
            self.working_area
        };

        let start_strut = work_area.loc.x;
        let end_strut = self.view_size.w - work_area.size.w - work_area.loc.x;

        // Normal sections align with the working area, but fullscreen sections align with the whole
        // view.
        if mode.is_fullscreen() {
            let start = section_main;
            let end = start + section_span;
            return (start, end);
        }

        let padding = if mode.is_maximized() {
            0.
        } else {
            ((work_area.size.w - section_span) / 2.).clamp(0., gaps)
        };

        let centered_view_main = if work_area.size.w <= section_span {
            section_main - start_strut
        } else {
            section_main - (work_area.size.w - section_span) / 2. - start_strut
        };
        let is_overflowing = |adjacent_section_span: Option<f64>| {
            center_on_overflow
                && adjacent_section_span
                    .filter(|adjacent_section_span| {
                        // NOTE: This logic won't work entirely correctly with small fixed-size
                        // maximized windows (they have a different area and padding).
                        adjacent_section_span + 3.0 * gaps + section_span > work_area.size.w
                    })
                    .is_some()
        };

        let start = if is_overflowing(next_section_span) {
            centered_view_main
        } else {
            section_main - padding - start_strut
        };
        let end = if is_overflowing(prev_section_span) {
            centered_view_main + view_main_span
        } else {
            section_main + section_span + padding + end_strut
        };
        (start, end)
    }

    fn collect_centered_view_snaps(&self) -> Vec<ViewSnap> {
        let mut snaps = Vec::with_capacity(self.sections.len());
        let mut section_main = 0.;
        for (col_idx, col) in self.sections.iter().enumerate() {
            let section_span = col.width();
            let mode = col.sizing_mode();

            let work_area = if mode.is_maximized() {
                self.parent_area
            } else {
                self.working_area
            };

            let start_strut = work_area.loc.x;

            let view_main_pos = if mode.is_fullscreen() {
                section_main
            } else if work_area.size.w <= section_span {
                section_main - start_strut
            } else {
                section_main - (work_area.size.w - section_span) / 2. - start_strut
            };
            snaps.push(ViewSnap {
                view_main_pos,
                col_idx,
            });

            section_main += section_span + self.options.layout.gaps;
        }
        snaps
    }

    fn collect_aligned_view_snaps(&self) -> Vec<ViewSnap> {
        let view_main_span = self.view_size.w;
        let gaps = self.options.layout.gaps;
        let last_col_idx = self.sections.len() - 1;

        let startmost_snap = self
            .aligned_section_snap_range(
                0.,
                &self.sections[0],
                None,
                self.sections.get(1).map(|c| c.width()),
            )
            .0;
        let last_section_main = self
            .sections
            .iter()
            .take(last_col_idx)
            .fold(0., |section_main, col| section_main + col.width() + gaps);
        let endmost_snap =
            self.aligned_section_snap_range(
                last_section_main,
                &self.sections[last_col_idx],
                last_col_idx
                    .checked_sub(1)
                    .and_then(|idx| self.sections.get(idx).map(|c| c.width())),
                None,
            )
            .1 - view_main_span;

        let mut snaps = vec![
            ViewSnap {
                view_main_pos: startmost_snap,
                col_idx: 0,
            },
            ViewSnap {
                view_main_pos: endmost_snap,
                col_idx: last_col_idx,
            },
        ];

        let mut section_main = 0.;
        for (col_idx, col) in self.sections.iter().enumerate() {
            let (start, end) = self.aligned_section_snap_range(
                section_main,
                col,
                col_idx
                    .checked_sub(1)
                    .and_then(|idx| self.sections.get(idx).map(|c| c.width())),
                self.sections.get(col_idx + 1).map(|c| c.width()),
            );
            Self::push_snap_if_between_bounds(
                &mut snaps,
                start,
                col_idx,
                startmost_snap,
                endmost_snap,
            );
            Self::push_snap_if_between_bounds(
                &mut snaps,
                end - view_main_span,
                col_idx,
                startmost_snap,
                endmost_snap,
            );

            section_main += col.width() + gaps;
        }

        snaps
    }

    fn collect_view_snaps(&self) -> Vec<ViewSnap> {
        let mut snaps = if self.is_centering_focused_section() {
            self.collect_centered_view_snaps()
        } else {
            self.collect_aligned_view_snaps()
        };
        snaps.sort_by_key(|snap| NotNan::new(snap.view_main_pos).unwrap());
        snaps
    }

    fn closest_view_snap<'a>(&self, snaps: &'a [ViewSnap], target_view_main: f64) -> &'a ViewSnap {
        snaps
            .iter()
            .min_by_key(|snap| NotNan::new((snap.view_main_pos - target_view_main).abs()).unwrap())
            .unwrap()
    }

    fn section_fully_visible_from_view_main(
        &self,
        view_main_pos: f64,
        col_idx: usize,
        towards_end: bool,
    ) -> bool {
        let col = &self.sections[col_idx];
        let section_main = self.section_main_pos(col_idx);
        let section_span = col.width();
        let mode = col.sizing_mode();

        let work_area = if mode.is_maximized() {
            self.parent_area
        } else {
            self.working_area
        };

        let start_strut = work_area.loc.x;

        if mode.is_fullscreen() {
            if towards_end {
                view_main_pos + self.view_size.w >= section_main + section_span
            } else {
                section_main >= view_main_pos
            }
        } else {
            let padding = if mode.is_maximized() {
                0.
            } else {
                ((work_area.size.w - section_span) / 2.).clamp(0., self.options.layout.gaps)
            };

            if towards_end {
                view_main_pos + start_strut + work_area.size.w
                    >= section_main + section_span + padding
            } else {
                section_main - padding >= view_main_pos + start_strut
            }
        }
    }

    fn furthest_visible_section_from_snap(
        &self,
        snap: &ViewSnap,
        target_view_offset: f64,
        current_view_offset: f64,
    ) -> usize {
        if self.is_centering_focused_section() {
            return snap.col_idx;
        }

        let towards_end = target_view_offset >= current_view_offset;
        let mut col_idx = snap.col_idx;
        if towards_end {
            for next_idx in (col_idx + 1)..self.sections.len() {
                if !self.section_fully_visible_from_view_main(snap.view_main_pos, next_idx, true) {
                    break;
                }
                col_idx = next_idx;
            }
        } else {
            for prev_idx in (0..col_idx).rev() {
                if !self.section_fully_visible_from_view_main(snap.view_main_pos, prev_idx, false) {
                    break;
                }
                col_idx = prev_idx;
            }
        }
        col_idx
    }

    fn animate_view_offset(&mut self, idx: usize, new_view_offset: f64) {
        self.animate_view_offset_with_config(
            idx,
            new_view_offset,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn animate_view_offset_with_config(
        &mut self,
        idx: usize,
        new_view_offset: f64,
        config: niri_config::Animation,
    ) {
        let new_section_main = self.section_main_pos(idx);
        let old_section_main = self.section_main_pos(self.active_section_idx);
        let offset_delta = old_section_main - new_section_main;
        self.view_offset.offset(offset_delta);

        let pixel = 1. / self.scale;

        // If our view offset is already this or animating towards this, we don't need to do
        // anything.
        let to_diff = new_view_offset - self.view_offset.target();
        if to_diff.abs() < pixel {
            // Correct for any inaccuracy.
            self.view_offset.offset(to_diff);
            return;
        }

        match &mut self.view_offset {
            ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_some() => {
                gesture.stationary_view_offset = new_view_offset;

                let current_pos = gesture.current_view_offset - gesture.delta_from_tracker;
                gesture.delta_from_tracker = new_view_offset - current_pos;
                let offset_delta = new_view_offset - gesture.current_view_offset;
                gesture.current_view_offset = new_view_offset;

                gesture.animate_from(-offset_delta, self.clock.clone(), config);
            }
            _ => {
                // FIXME: also compute and use current velocity.
                self.view_offset = ViewOffset::Animation(Animation::new(
                    self.clock.clone(),
                    self.view_offset.current(),
                    new_view_offset,
                    0.,
                    config,
                ));
            }
        }
    }

    fn animate_view_offset_to_section_centered(
        &mut self,
        target_view_main: Option<f64>,
        idx: usize,
        config: niri_config::Animation,
    ) {
        let new_view_offset =
            self.compute_new_view_offset_for_section_centered(target_view_main, idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_section_with_config(
        &mut self,
        target_view_main: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
        config: niri_config::Animation,
    ) {
        let new_view_offset =
            self.compute_new_view_offset_for_section(target_view_main, idx, prev_idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_section(
        &mut self,
        target_view_main: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) {
        self.animate_view_offset_to_section_with_config(
            target_view_main,
            idx,
            prev_idx,
            self.options.animations.horizontal_view_movement.0,
        )
    }

    fn activate_section(&mut self, idx: usize) {
        self.activate_section_with_anim_config(
            idx,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn activate_section_with_anim_config(&mut self, idx: usize, config: niri_config::Animation) {
        if self.active_section_idx == idx
            // During a DnD scroll, animate even when activating the same window, for DnD hold.
            && (self.sections.is_empty() || !self.view_offset.is_dnd_scroll())
        {
            return;
        }

        self.animate_view_offset_to_section_with_config(
            None,
            idx,
            Some(self.active_section_idx),
            config,
        );

        if self.active_section_idx != idx {
            self.active_section_idx = idx;

            // A different section was activated; reset the flag.
            self.activate_prev_section_on_removal = None;
            self.view_offset_to_restore = None;
            self.interactive_resize = None;
        }
    }

    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.sections.is_empty() {
            return InsertPosition::NewSection(0);
        }

        let pos = self.map_point_in(pos);

        // Raw (unfudged) cursor position in section-main / cross space. Used for the per-tile
        // interior hit-test and the rel_x/rel_y region map, which must line up with the actual
        // (unfudged) tile rects and the drawn header band.
        let main_raw = pos.x + self.view_main_pos();
        let cross_raw = pos.y;

        // Aim for the center of the gap. This fudge only biases the closest-gap searches below; it
        // must NOT leak into the interior hit-test (see `main_raw`/`cross_raw`).
        let main = main_raw + self.options.layout.gaps / 2.;
        let cross = cross_raw + self.options.layout.gaps / 2.;

        // Insert position is before the first section.
        if main < 0. {
            return InsertPosition::NewSection(0);
        }

        // Find the closest gap between sections.
        let (closest_col_idx, closest_col_main) = self
            .section_main_positions()
            .enumerate()
            .min_by_key(|(_, col_main)| NotNan::new((col_main - main).abs()).unwrap())
            .unwrap();

        // Find the section containing the position.
        let (col_idx, _) = self
            .section_main_positions()
            .enumerate()
            .take_while(|(_, col_main)| *col_main <= main)
            .last()
            .unwrap_or((0, 0.));

        // Insert position is past the last section.
        if col_idx == self.sections.len() {
            return InsertPosition::NewSection(closest_col_idx);
        }

        // Find the closest gap between tiles.
        let col = &self.sections[col_idx];

        let (closest_tile_idx, closest_tile_cross) = if col.is_tabbed() {
            // In tabbed mode, there's only one tile visible, and we want to check its top and
            // bottom.
            let active_off = col.active_tile_offset();
            let top = active_off.y;
            let active_path = col.root.active_leaf_path();
            let bottom = top + col.root.leaf_data(&active_path).map(|d| d.size.h).unwrap_or(0.);
            if (top - cross).abs() <= (bottom - cross).abs() {
                (col.active_tile_idx(), top)
            } else {
                (col.active_tile_idx() + 1, bottom)
            }
        } else {
            col.tile_offsets()
                .map(|tile_off| tile_off.y)
                .enumerate()
                .min_by_key(|(_, tile_cross)| NotNan::new((tile_cross - cross).abs()).unwrap())
                .unwrap()
        };

        // Return whichever gap is closer in main/cross space, or detect a tile interior for splits.
        let main_dist = (closest_col_main - main).abs();
        let cross_dist = (closest_tile_cross - cross).abs();

        let col_main_start = self.section_main_pos(col_idx);

        // Section-local position (no gap-aiming fudge), in the same coordinate space as
        // `leaf_positions` and the tab-header geometry. Used for the header-band hit-test.
        let local = Point::from((main_raw - col_main_start, cross_raw));

        // Tab-header band: if the cursor is over the titlebar band a tabbing container (the root
        // tabbed section, or a nested Tabbed/Stacked node) reserves at its top, add the dragged
        // window as a new tab of that container. Below the band, the per-window region map applies
        // to the visible content. This is the *only* way to add a tab over a tabbed section's body
        // now — the body itself follows the precise per-window map.
        if let Some(target) = col.header_band_target(local) {
            return InsertPosition::InsertTab(col_idx, target);
        }

        // Per-leaf tile size by flat-leaf index, resolved through the tree path so it stays
        // correct for nested splits (where the leaf index is not a root child index).
        let leaf_size = |idx: usize| -> Size<f64, Logical> {
            col.root
                .path_for_leaf_index(idx)
                .and_then(|p| col.root.leaf_data(&p))
                .map(|d| d.size)
                .unwrap_or_default()
        };

        // Gap band per axis. Normally `gaps * 2`, but clamped so that:
        //  - a tile always keeps an interior region (at least its centre third) even when it is
        //    smaller than ~4*gaps on that axis (otherwise swap/split/tab zones become unreachable);
        //  - the band keeps a small minimum width when `gaps == 0`, so the gap-insert zones
        //    (new-section / new-row) are reachable off the exact boundary pixel rather than never.
        // Both clamps are no-ops in the normal case (reasonable gaps, tiles larger than ~4*gaps).
        const MIN_GAP_BAND: f64 = 6.;
        let gaps = self.options.layout.gaps;
        // Clamp to a valid leaf: in a tabbed section `closest_tile_idx` can be one past the last
        // leaf (the "below the active tab" case), which has no size of its own.
        let close_idx = closest_tile_idx.min(col.root.leaf_count().saturating_sub(1));
        let close_size = leaf_size(close_idx);
        let gap_band = |tile_dim: f64| {
            let band = if gaps == 0. { MIN_GAP_BAND } else { gaps * 2. };
            // Never eat more than a third per side, so the centre third always stays interior.
            band.min((tile_dim / 3.).max(0.))
        };
        let main_threshold = gap_band(close_size.w);
        let cross_threshold = gap_band(close_size.h);

        // If the pointer is far from both gaps, it's in a tile interior; otherwise it's near a gap
        // and inserts a new section (side gap) or a new row above/below (top/bottom gap). The gap
        // branches keep a tabbed/stacked or nested section escapable.
        if main_dist > main_threshold && cross_dist > cross_threshold {
            // Hit-test the *visible* leaf under the pointer: a vertical stack disambiguates by cross
            // (y), a horizontal row by main (x). The hidden tabs of a tabbing container share the
            // visible tab's rect, so require the hit leaf to be visible — otherwise a centre-drop
            // would target a hidden tab instead of the one the user sees. Fall back to the
            // cross-closest tile if the pointer isn't inside any visible tile.
            let offsets: Vec<_> = col.tile_offsets().collect();
            let visibility = col.root.leaf_visibility();
            let tile_idx = offsets
                .iter()
                .enumerate()
                .find(|(idx, off)| {
                    let sz = leaf_size(*idx);
                    let left = col_main_start + off.x;
                    visibility.get(*idx).copied().unwrap_or(true)
                        && cross_raw >= off.y
                        && cross_raw <= off.y + sz.h
                        && main_raw >= left
                        && main_raw <= left + sz.w
                })
                .map(|(idx, _)| idx)
                .unwrap_or(closest_tile_idx);

            if let Some(&tile_off) = offsets.get(tile_idx) {
                // The visible window's rect, divided into a sway-style region map. The left/right
                // edge bands place the dragged window beside *this* window (a Main split); the
                // top/bottom bands stack it above/below *this* window (a Cross split); the centre
                // swaps the two (in-place mode) or groups them into tabs (detach mode). Every target
                // is the single window under the cursor — never the whole row, stack, or section it
                // belongs to.
                let tile_w = leaf_size(tile_idx).w;
                let tile_h = leaf_size(tile_idx).h;
                let left = col_main_start + tile_off.x;
                let rel_x = ((main_raw - left) / tile_w.max(1.)).clamp(0., 1.);
                let rel_y = ((cross_raw - tile_off.y) / tile_h.max(1.)).clamp(0., 1.);

                let in_centre = (1. / 3. ..=2. / 3.).contains(&rel_x)
                    && (1. / 3. ..=2. / 3.).contains(&rel_y);
                if in_centre {
                    if self.options.layout.tiling_drag == TilingDrag::InPlace {
                        InsertPosition::Swap(col_idx, tile_idx)
                    } else {
                        InsertPosition::InsertTab(col_idx, tile_idx)
                    }
                } else if (rel_x - 0.5).abs() >= (rel_y - 0.5).abs() {
                    // Closer to a left/right edge → side-by-side (a Main split of this window).
                    InsertPosition::InSplit(col_idx, tile_idx, SplitAxis::Main, rel_x > 0.5)
                } else {
                    // Closer to a top/bottom edge → stack above/below (a Cross split of this window).
                    InsertPosition::InSplit(col_idx, tile_idx, SplitAxis::Cross, rel_y > 0.5)
                }
            } else {
                InsertPosition::InSplit(col_idx, tile_idx, SplitAxis::Main, false)
            }
        } else if main_dist <= cross_dist {
            InsertPosition::NewSection(closest_col_idx)
        } else {
            InsertPosition::InSection(col_idx, closest_tile_idx)
        }
    }

    /// Adds a tile as a split child of the tile at (col_idx, tile_idx) along the given axis.
    pub fn add_tile_to_split(
        &mut self,
        col_idx: usize,
        tile_idx: usize,
        axis: SplitAxis,
        place_after: bool,
        tile: Tile<W>,
        activate: bool,
    ) {
        // Don't create a split in a fullscreen/maximized section.
        if !self.sections[col_idx].pending_sizing_mode().is_normal() {
            self.add_tile_to_section(col_idx, None, tile, activate);
            return;
        }

        let prev_next_x = self.section_main_pos(col_idx + 1);

        let target_section = &mut self.sections[col_idx];
        target_section.add_tile_to_split(tile_idx, tile, axis, place_after, activate);

        if activate
            && self.active_section_idx != col_idx {
                self.activate_section(col_idx);
            }

        // Move sections to account for width changes.
        let offset = self.section_main_pos(col_idx + 1) - prev_next_x;
        if offset != 0. {
            if self.active_section_idx <= col_idx {
                for col in &mut self.sections[col_idx + 1..] {
                    col.animate_move_from(-offset);
                }
            } else {
                for col in &mut self.sections[..=col_idx] {
                    col.animate_move_from(offset);
                }
            }
        }
    }

    /// Groups a dragged tile into a tabbed container with the tile at (col_idx, tile_idx).
    pub fn add_tile_as_tab(
        &mut self,
        col_idx: usize,
        tile_idx: usize,
        tile: Tile<W>,
        activate: bool,
    ) {
        if !self.sections[col_idx].pending_sizing_mode().is_normal() {
            self.add_tile_to_section(col_idx, None, tile, activate);
            return;
        }

        let prev_next_x = self.section_main_pos(col_idx + 1);
        self.sections[col_idx].add_tile_as_tab(tile_idx, tile, activate);
        if activate && self.active_section_idx != col_idx {
            self.activate_section(col_idx);
        }

        let offset = self.section_main_pos(col_idx + 1) - prev_next_x;
        if offset != 0. {
            if self.active_section_idx <= col_idx {
                for col in &mut self.sections[col_idx + 1..] {
                    col.animate_move_from(-offset);
                }
            } else {
                for col in &mut self.sections[..=col_idx] {
                    col.animate_move_from(offset);
                }
            }
        }
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: SectionWidth,
        is_full_width: bool,
        anim_config: Option<niri_config::Animation>,
    ) {
        // Split-then-open: if no explicit section was requested and the active section has a pending
        // split direction (set by `split-window`), place the new window into that section as a
        // split with the focused tile, rather than opening a brand-new section.
        if col_idx.is_none() && !self.sections.is_empty() {
            let active = self.active_section_idx;
            let col = &self.sections[active];
            if col.pending_sizing_mode().is_normal() {
                if let Some(axis) = col.pending_split_direction {
                    self.sections[active].pending_split_direction = None;
                    let target_idx = self.sections[active].active_leaf_idx();
                    self.add_tile_to_split(active, target_idx, axis, true, tile, activate);
                    return;
                }
            }
        }

        let section = Section::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            is_full_width,
        );

        self.add_section(col_idx, section, activate, anim_config);
    }

    pub fn add_tile_to_section(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        let prev_next_x = self.section_main_pos(col_idx + 1);

        let target_section = &mut self.sections[col_idx];

        // Check for pending split direction (split-then-open).
        // Don't create a split in a fullscreen/maximized section — the invariant requires
        // single-leaf or tabbed for non-normal sizing modes.
        if let Some(split_axis) = target_section.pending_split_direction.take() {
            if !target_section.pending_sizing_mode().is_normal() {
                // Fall through to normal tile insertion.
                target_section.pending_split_direction = None;
            } else {
                // The next window opened in this section should be placed in a split
                // with the currently-focused tile, rather than appended.
                let active_idx = target_section.active_leaf_idx();
                target_section.add_tile_to_split(active_idx, tile, split_axis, true, activate);

                if activate && self.active_section_idx != col_idx {
                    self.activate_section(col_idx);
                }

                // Move sections to account for width changes.
                let offset = self.section_main_pos(col_idx + 1) - prev_next_x;
                if offset != 0. {
                    if self.active_section_idx <= col_idx {
                        for col in &mut self.sections[col_idx + 1..] {
                            col.animate_move_from(-offset);
                        }
                    } else {
                        for col in &mut self.sections[..=col_idx] {
                            col.animate_move_from(offset);
                        }
                    }
                }
                return;
            }
        }

        let leaf_idx = tile_idx.unwrap_or(target_section.tiles_len());
        let prev_active_id = target_section.active_tile().window().id().clone();

        // add_tile_at handles insertion (including wrapping a Main row), activation and animation.
        let new_leaf_idx = target_section.add_tile_at(leaf_idx, tile, activate);

        if activate && self.active_section_idx != col_idx {
            self.activate_section(col_idx);
        }

        let target_section = &mut self.sections[col_idx];
        let anim = self.options.animations.window_movement.0;
        if target_section.is_tabbed() {
            if activate {
                // Fade out the previously active tab.
                if let Some(i) = target_section.position(&prev_active_id) {
                    target_section.tile_mut(i).animate_alpha(1., 0., anim);
                }
            } else {
                // Added a background tab; fade it out (it sits behind the active one).
                target_section.tile_mut(new_leaf_idx).animate_alpha(1., 0., anim);
            }
        }

        // Adding a wider window into a section increases its width now (even if the window will
        // shrink later). Move the sections to account for this.
        let offset = self.section_main_pos(col_idx + 1) - prev_next_x;
        if self.active_section_idx <= col_idx {
            for col in &mut self.sections[col_idx + 1..] {
                col.animate_move_from(-offset);
            }
        } else {
            for col in &mut self.sections[..=col_idx] {
                col.animate_move_from(offset);
            }
        }
    }

    pub fn add_tile_right_of(
        &mut self,
        right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: SectionWidth,
        is_full_width: bool,
    ) {
        let right_of_idx = self
            .sections
            .iter()
            .position(|col| col.contains(right_of))
            .unwrap();
        let col_idx = right_of_idx + 1;

        self.add_tile(Some(col_idx), tile, activate, width, is_full_width, None);
    }

    pub fn add_section(
        &mut self,
        idx: Option<usize>,
        mut section: Section<W>,
        activate: bool,
        anim_config: Option<niri_config::Animation>,
    ) {
        let was_empty = self.sections.is_empty();

        let idx = idx.unwrap_or_else(|| {
            if was_empty {
                0
            } else {
                self.active_section_idx + 1
            }
        });

        section.update_config(
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            self.options.clone(),
        );
        self.sections.insert(idx, section);

        if !was_empty && idx <= self.active_section_idx {
            self.active_section_idx += 1;
        }

        // Animate movement of other sections.
        let offset = self.section_main_pos(idx + 1) - self.section_main_pos(idx);
        let config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        if self.active_section_idx <= idx {
            for col in &mut self.sections[idx + 1..] {
                col.animate_move_from_with_config(-offset, config);
            }
        } else {
            for col in &mut self.sections[..idx] {
                col.animate_move_from_with_config(offset, config);
            }
        }

        if activate {
            // If this is the first window on an empty workspace, remove the effect of whatever
            // view_offset was left over and skip the animation.
            if was_empty {
                self.view_offset = ViewOffset::Static(0.);
                self.view_offset =
                    ViewOffset::Static(self.compute_new_view_offset_for_section(None, idx, None));
            }

            let prev_offset = (!was_empty && idx == self.active_section_idx + 1)
                .then(|| self.view_offset.stationary());

            let anim_config =
                anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);
            self.activate_section_with_anim_config(idx, anim_config);
            self.activate_prev_section_on_removal = prev_offset;
        }
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        if self.sections.is_empty() {
            return None;
        }

        let section = &self.sections[self.active_section_idx];
        // `remove_tile_by_idx` treats its index as a flat leaf index throughout, so pass the flat
        // index of the active leaf, not the root-child `active_tile_idx` (they differ once the
        // section is nested).
        Some(self.remove_tile_by_idx(
            self.active_section_idx,
            section.active_leaf_idx(),
            transaction,
            None,
        ))
    }

    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let section_idx = self
            .sections
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let section = &self.sections[section_idx];

        let tile_idx = section.position(window).unwrap();
        self.remove_tile_by_idx(section_idx, tile_idx, transaction, None)
    }

    pub fn remove_tile_by_idx(
        &mut self,
        section_idx: usize,
        tile_idx: usize,
        transaction: Transaction,
        anim_config: Option<niri_config::Animation>,
    ) -> RemovedTile<W> {
        // If this is the only tile in the section, remove the whole section.
        if self.sections[section_idx].tiles_len() == 1 {
            let mut section = self.remove_section_by_idx(section_idx, anim_config);
            return RemovedTile {
                tile: section.remove_tile(tile_idx),
                width: section.width,
                is_full_width: section.is_full_width,
                is_floating: false,
            };
        }

        let section = &mut self.sections[section_idx];
        let prev_width = section.width();

        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);

        // Animate movement of other tiles.
        // FIXME: tiles can move along the main axis too, in a centered or resizing layout with
        // one window smaller
        // than the others.
        let offset_y = section.tile_offset(tile_idx + 1).y - section.tile_offset(tile_idx).y;
        for i in (tile_idx + 1)..section.tiles_len() {
            section.tile_mut(i).animate_move_y_from(offset_y);
        }

        if section.is_tabbed() && tile_idx != section.active_tile_idx() {
            // Fade in when removing background tab from a tabbed section.
            let tile = section.tile_mut(tile_idx);
            tile.animate_alpha(0., 1., movement_config);
        }

        let was_normal = section.sizing_mode().is_normal();
        // Capture whether the tree is flat *before* the removal: `remove_tile` runs `simplify`,
        // which can flatten nesting away, and the root-level `active_idx` fixup below is only valid
        // when the pre-removal tree was flat (otherwise `remove_leaf` already maintained
        // `active_idx` at every level). `tile_idx` is a pre-removal flat index, so it must not be
        // interpreted against the post-removal tree.
        let was_flat = !section.root.has_nested_children();

        let tile = section.remove_tile(tile_idx);

        // If an active section became non-fullscreen after removing the tile, clear the stored
        // unfullscreen offset.
        if section_idx == self.active_section_idx && !was_normal && section.sizing_mode().is_normal() {
            self.view_offset_to_restore = None;
        }

        // If one window is left, reset its weight to 1.
        if section.tiles_len() == 1 {
            if let ChildSpan::Auto { weight } = &mut section.data_mut()[0].span {
                *weight = 1.;
            }
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        let tile = RemovedTile {
            tile,
            width: section.width,
            is_full_width: section.is_full_width,
            is_floating: false,
        };

        #[allow(clippy::comparison_chain)] // What do you even want here?
        if was_flat {
            if tile_idx < section.active_tile_idx() {
                // A tile above was removed; preserve the current position.
                section.set_active_tile_idx(section.active_tile_idx() - 1);
            } else if tile_idx == section.active_tile_idx() {
                // The active tile was removed, so the active tile index shifted to the next tile.
                if tile_idx == section.tiles_len() {
                    // The bottom tile was removed and it was active, update active idx to remain valid.
                    section.activate_idx(tile_idx - 1);
                } else {
                    // Ensure the newly active tile animates to opaque.
                    section.tile_mut(tile_idx).ensure_alpha_animates_to_1();
                }
            }
        } else {
            // For nested splits, remove_leaf already adjusted active_idx at each level.
            // Just ensure the active leaf animates to opaque.
            section.active_tile_mut().ensure_alpha_animates_to_1();
        }

        section.update_tile_sizes_with_transaction(true, transaction);
        let offset = prev_width - section.width();

        // Animate movement of the other sections.
        if self.active_section_idx <= section_idx {
            for col in &mut self.sections[section_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.sections[..=section_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        tile
    }

    pub fn remove_active_section(&mut self) -> Option<Section<W>> {
        if self.sections.is_empty() {
            return None;
        }

        Some(self.remove_section_by_idx(self.active_section_idx, None))
    }

    pub fn remove_section_by_idx(
        &mut self,
        section_idx: usize,
        anim_config: Option<niri_config::Animation>,
    ) -> Section<W> {
        // Animate movement of the other sections.
        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        let offset = self.section_main_pos(section_idx + 1) - self.section_main_pos(section_idx);
        if self.active_section_idx <= section_idx {
            for col in &mut self.sections[section_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.sections[..section_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        let section = self.sections.remove(section_idx);

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if section
                .tiles_enumerated()
                .any(|(_, tile)| tile.window().id() == &resize.window)
            {
                self.interactive_resize = None;
            }
        }

        if section_idx + 1 == self.active_section_idx {
            // The previous section, that we were going to activate upon removal of the active
            // section, has just been itself removed.
            self.activate_prev_section_on_removal = None;
        }

        if section_idx == self.active_section_idx {
            self.view_offset_to_restore = None;
        }

        if self.sections.is_empty() {
            return section;
        }

        let view_config = anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);

        if section_idx < self.active_section_idx {
            // A section to the left was removed; preserve the current position.
            // FIXME: preserve activate_prev_section_on_removal.
            self.active_section_idx -= 1;
            self.activate_prev_section_on_removal = None;
        } else if section_idx == self.active_section_idx
            && self.activate_prev_section_on_removal.is_some()
        {
            // The active section was removed, and we needed to activate the previous section.
            if 0 < section_idx {
                let prev_offset = self.activate_prev_section_on_removal.unwrap();

                self.activate_section_with_anim_config(self.active_section_idx - 1, view_config);

                // Restore the view offset but make sure to scroll the view in case the
                // previous window had resized.
                self.animate_view_offset_with_config(
                    self.active_section_idx,
                    prev_offset,
                    view_config,
                );
                self.animate_view_offset_to_section_with_config(
                    None,
                    self.active_section_idx,
                    None,
                    view_config,
                );
            }
        } else {
            self.activate_section_with_anim_config(
                min(self.active_section_idx, self.sections.len() - 1),
                view_config,
            );
        }

        section
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        let (col_idx, section) = self
            .sections
            .iter_mut()
            .enumerate()
            .find(|(_, col)| col.contains(window))
            .unwrap();
        let was_normal = section.sizing_mode().is_normal();
        let prev_origin = section.tiles_origin();

        let (tile_idx, tile) = section
            .tiles_enumerated_mut()
            .find(|(_, tile)| tile.window().id() == window)
            .unwrap();

        let resize = tile.window_mut().interactive_resize_data();

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        let prev_width = section.width();

        section.update_window(window);
        section.update_tile_sizes(false);

        let offset = prev_width - section.width();

        // Move other sections in tandem with resizing.
        let ongoing_resize_anim = section.tile(tile_idx).resize_animation().is_some();
        if offset != 0. {
            if self.active_section_idx <= col_idx {
                for col in &mut self.sections[col_idx + 1..] {
                    // If there's a resize animation on the tile (that may have just started in
                    // section.update_window()), then the apparent size change is smooth with no
                    // sudden jumps. This corresponds to adding an X animation to adjacent sections.
                    //
                    // There could also be no resize animation with nonzero offset. This could
                    // happen for example:
                    // - if the window resized on its own, which we don't animate
                    // - if the window resized by less than 10 px (the resize threshold)
                    //
                    // The latter case could also cancel an ongoing resize animation.
                    //
                    // Now, stationary sections shouldn't react to this offset change in any way,
                    // i.e. their apparent X position should jump together with the resize.
                    // However, adjacent sections that are already animating an X movement should
                    // offset their animations to avoid the jump.
                    //
                    // Notably, this is necessary to fix the animation jump when resizing width back
                    // and forth in quick succession (in a way that cancels the resize animation).
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(offset);
                    }
                }
            } else {
                for col in &mut self.sections[..=col_idx] {
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            -offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(-offset);
                    }
                }
            }
        }

        // When a section goes between fullscreen and non-fullscreen, the tiles origin can change.
        // The change comes from things like ignoring struts and hiding the tab indicator in
        // fullscreen, so it can happen on both the main and cross axes.
        let section = &mut self.sections[col_idx];
        let new_origin = section.tiles_origin();
        let origin_delta = prev_origin - new_origin;
        if origin_delta != Point::new(0., 0.) {
            for (tile, _pos) in section.tiles_mut() {
                tile.animate_move_from(origin_delta);
            }
        }

        if col_idx == self.active_section_idx {
            // If offset == 0, then don't mess with the view or the gesture. Some clients (Firefox,
            // Chromium, Electron) currently don't commit after the ack of a configure that drops
            // the Resizing state, which can trigger this code path for a while.
            let resize = if offset != 0. { resize } else { None };
            if let Some(resize) = resize {
                // Don't bother with the gesture.
                self.view_offset.cancel_gesture();

                // If this is an interactive resize commit of an active window, then we need to
                // either preserve the view offset or adjust it accordingly.
                let centered = self.is_centering_focused_section();

                let width = self.sections[col_idx].width();
                let offset = if centered {
                    // FIXME: when view_offset becomes fractional, this can be made additive too.
                    let new_offset =
                        -(self.working_area.size.w - width) / 2. - self.working_area.loc.x;
                    new_offset - self.view_offset.target()
                } else if resize.edges.contains(ResizeEdge::LEFT) {
                    -offset
                } else {
                    0.
                };

                self.view_offset.offset(offset);
            }

            // When the active section goes fullscreen, store the view offset to restore later.
            let is_normal = self.sections[col_idx].sizing_mode().is_normal();
            if was_normal && !is_normal {
                self.view_offset_to_restore = Some(self.view_offset.stationary());
            }

            // Upon unfullscreening, restore the view offset.
            //
            // In tabbed display mode, there can be multiple tiles in a fullscreen section. They
            // will unfullscreen one by one, and the section width will shrink only when the
            // last tile unfullscreens. This is when we want to restore the view offset,
            // otherwise it will immediately reset back by the animate_view_offset below.
            let unfullscreen_offset = if !was_normal && is_normal {
                // Take the value unconditionally, even if the view is currently frozen by
                // a view gesture. It shouldn't linger around because it's only valid for this
                // particular unfullscreen.
                self.view_offset_to_restore.take()
            } else {
                None
            };

            // We might need to move the view to ensure the resized window is still visible. But
            // only do it when the view isn't frozen by an interactive resize or a view gesture.
            if self.interactive_resize.is_none() && !self.view_offset.is_gesture() {
                // Synchronize the view movement along the main axis with the resize so that it
                // looks nice. This is especially important for always-centered view.
                let config = if ongoing_resize_anim {
                    self.options.animations.window_resize.anim
                } else {
                    self.options.animations.horizontal_view_movement.0
                };

                // Restore the view offset upon unfullscreening if needed.
                if let Some(prev_offset) = unfullscreen_offset {
                    self.animate_view_offset_with_config(col_idx, prev_offset, config);
                }

                // FIXME: we will want to skip the animation in some cases here to make continuously
                // resizing windows not look janky.
                self.animate_view_offset_to_section_with_config(None, col_idx, None, config);
            }
        }
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        let section_idx = self
            .sections
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if self.active_section_idx == section_idx {
            return 0.;
        }

        // Consider the end of an ongoing animation because that's what compute-to-fit does too.
        let target_view_main = self.target_view_main_pos();
        let new_view_offset = self.compute_new_view_offset_for_section(
            Some(target_view_main),
            section_idx,
            Some(self.active_section_idx),
        );

        let target_section_main = self.section_main_pos(section_idx);
        let current_offset_from_section = target_view_main - target_section_main;

        (current_offset_from_section - new_view_offset).abs() / self.working_area.size.w
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let section_idx = self.sections.iter().position(|col| col.contains(window));
        let Some(section_idx) = section_idx else {
            return false;
        };
        let section = &mut self.sections[section_idx];

        section.activate_window(window);
        self.activate_section(section_idx);

        true
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let axis = self.axis();
        let (tile, mut tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == window)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = axis.size_out(tile.tile_size());

        let (col_idx, tile_idx) = self
            .sections
            .iter()
            .enumerate()
            .find_map(|(col_idx, col)| {
                col.tiles_enumerated()
                    .find_map(|(tile_idx, tile)| (tile.window().id() == window).then_some(tile_idx))
                    .map(move |tile_idx| (col_idx, tile_idx))
            })
            .unwrap();

        let col = &self.sections[col_idx];
        let removing_last = col.tiles_len() == 1;

        // Skip the closing animation for currently-invisible tiles (a hidden background tab). Use
        // per-leaf visibility rather than comparing to the root-child `active_tile_idx`: a split
        // that is itself a tab shows several leaves at once, and a nested active leaf has a flat
        // index unrelated to the root child index.
        if !col.root.leaf_visibility().get(tile_idx).copied().unwrap_or(true) {
            return;
        }

        tile_pos += axis.main_vec(self.view_main_pos());

        if col_idx < self.active_section_idx {
            let offset = if removing_last {
                self.section_main_pos(col_idx + 1) - self.section_main_pos(col_idx)
            } else {
                // `data()` is indexed by root child, so exclude the removed leaf's *root child*
                // (mapping the flat `tile_idx` through the tree) rather than comparing a flat index
                // to a root-child index.
                let removed_root_child = col.leaf_idx_to_root_child(tile_idx);
                self.sections[col_idx].width()
                    - col
                        .data()
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, data)| {
                            (idx != removed_root_child).then_some(NotNan::new(data.size.w).unwrap())
                        })
                        .max()
                        .map(NotNan::into_inner)
                        .unwrap()
            };
            tile_pos += axis.main_vec(-offset);
        }

        self.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        let scale = Scale::from(self.scale);
        let res = ClosingWindow::new(
            renderer, snapshot, scale, tile_size, tile_pos, blocker, anim,
        );
        match res {
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.sections
            .iter_mut()
            .any(|col| col.start_open_animation(id))
    }

    pub fn focus_left(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        // Navigate within the section's Main-axis splits first (at any nesting depth); only fall
        // through to the previous section when at the left edge of the tree.
        if self.sections[self.active_section_idx].focus_in_axis(SplitAxis::Main, -1) {
            return true;
        }

        if self.active_section_idx == 0 {
            return false;
        }
        self.activate_section(self.active_section_idx - 1);
        true
    }

    pub fn focus_right(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        // Navigate within the section's Main-axis splits first (at any nesting depth); only fall
        // through to the next section when at the right edge of the tree.
        if self.sections[self.active_section_idx].focus_in_axis(SplitAxis::Main, 1) {
            return true;
        }

        if self.active_section_idx + 1 >= self.sections.len() {
            return false;
        }

        self.activate_section(self.active_section_idx + 1);
        true
    }

    pub fn focus_section_first(&mut self) {
        self.activate_section(0);
    }

    pub fn focus_section_last(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        self.activate_section(self.sections.len() - 1);
    }

    pub fn focus_section(&mut self, index: usize) {
        if self.sections.is_empty() {
            return;
        }

        self.activate_section(index.saturating_sub(1).min(self.sections.len() - 1));
    }

    pub fn focus_window_in_section(&mut self, index: u8) {
        if self.sections.is_empty() {
            return;
        }

        self.sections[self.active_section_idx].focus_index(index);
    }

    pub fn focus_down(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        self.sections[self.active_section_idx].focus_down()
    }

    pub fn focus_up(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        self.sections[self.active_section_idx].focus_up()
    }

    pub fn focus_down_or_left(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let section = &mut self.sections[self.active_section_idx];
        if !section.focus_down() {
            self.focus_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let section = &mut self.sections[self.active_section_idx];
        if !section.focus_down() {
            self.focus_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let section = &mut self.sections[self.active_section_idx];
        if !section.focus_up() {
            self.focus_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let section = &mut self.sections[self.active_section_idx];
        if !section.focus_up() {
            self.focus_right();
        }
    }

    pub fn focus_top(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        self.sections[self.active_section_idx].focus_top()
    }

    pub fn focus_bottom(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        self.sections[self.active_section_idx].focus_bottom()
    }

    pub fn move_section_to_index(&mut self, index: usize) {
        if self.sections.is_empty() {
            return;
        }

        self.move_section_to(index.saturating_sub(1).min(self.sections.len() - 1));
    }

    fn move_section_to(&mut self, new_idx: usize) {
        if self.active_section_idx == new_idx {
            return;
        }

        let current_section_main = self.section_main_pos(self.active_section_idx);
        let next_section_main = self.section_main_pos(self.active_section_idx + 1);

        let mut section = self.sections.remove(self.active_section_idx);
        cancel_resize_for_section(&mut self.interactive_resize, &mut section);
        self.sections.insert(new_idx, section);

        // Preserve the camera position when moving toward the start of the main axis.
        let view_offset_delta = -self.section_main_pos(self.active_section_idx) + current_section_main;
        self.view_offset.offset(view_offset_delta);

        // The section we just moved is offset by the difference between its new and old position.
        let new_section_main = self.section_main_pos(new_idx);
        self.sections[new_idx].animate_move_from(current_section_main - new_section_main);

        // All sections in between move by the span of the section that we just moved.
        let between_sections_main_delta = next_section_main - current_section_main;
        if self.active_section_idx < new_idx {
            for col in &mut self.sections[self.active_section_idx..new_idx] {
                col.animate_move_from(between_sections_main_delta);
            }
        } else {
            for col in &mut self.sections[new_idx + 1..=self.active_section_idx] {
                col.animate_move_from(-between_sections_main_delta);
            }
        }

        self.activate_section_with_anim_config(new_idx, self.options.animations.window_movement.0);
    }

    pub fn move_left(&mut self) -> bool {
        if self.active_section_idx == 0 {
            return false;
        }

        self.move_section_to(self.active_section_idx - 1);
        true
    }

    pub fn move_right(&mut self) -> bool {
        let new_idx = self.active_section_idx + 1;
        if new_idx >= self.sections.len() {
            return false;
        }

        self.move_section_to(new_idx);
        true
    }

    pub fn move_section_to_first(&mut self) {
        self.move_section_to(0);
    }

    pub fn move_section_to_last(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let new_idx = self.sections.len() - 1;
        self.move_section_to(new_idx);
    }

    pub fn move_down(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        self.sections[self.active_section_idx].move_down()
    }

    pub fn move_up(&mut self) -> bool {
        if self.sections.is_empty() {
            return false;
        }

        self.sections[self.active_section_idx].move_up()
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if self.sections.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.sections
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles_enumerated()
                        .find_map(|(tile_idx, tile)| (tile.window().id() == window).then_some(tile_idx))
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_section_idx;
            let source_tile_idx = self.sections[self.active_section_idx].active_leaf_idx();
            (source_col_idx, source_tile_idx)
        };

        let source_section = &self.sections[source_col_idx];
        let prev_off = source_section.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_section_idx == source_col_idx
            && source_section.active_leaf_idx() == source_tile_idx;

        if source_section.tiles_len() == 1 {
            if source_col_idx == 0 {
                return;
            }

            // Move into adjacent section.
            let target_section_idx = source_col_idx - 1;

            let main_delta = if self.active_section_idx <= source_col_idx {
                // Tiles on the end side animate from the following section.
                self.section_main_pos(source_col_idx) - self.section_main_pos(target_section_idx)
            } else {
                // Tiles on the start side animate to preserve their end edge position.
                f64::max(
                    0.,
                    self.sections[target_section_idx].width() - self.sections[source_col_idx].width(),
                )
            };
            let mut move_offset = main_space_vec(main_delta);

            if source_tile_was_active {
                // Make sure the previous (target) section is activated so the animation looks right.
                //
                // However, if it was already going to be activated, leave the offset as is. This
                // improves the workflow that has become common with tabbed sections: open a new
                // window, then immediately consume it left as a new tab.
                self.activate_prev_section_on_removal
                    .get_or_insert(self.view_offset.stationary() + main_delta);
            }

            move_offset += main_space_vec(self.sections[source_col_idx].render_offset().x);
            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_section(target_section_idx, None, tile, source_tile_was_active);

            let target_section = &mut self.sections[target_section_idx];
            move_offset -= main_space_vec(target_section.render_offset().x);
            move_offset += prev_off - target_section.tile_offset(target_section.tiles_len() - 1);

            let new_tile = target_section.last_tile_mut();
            new_tile.animate_move_from(move_offset);
        } else {
            // Move out of section.
            let mut move_offset = main_space_vec(source_section.render_offset().x);

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            // We're inserting into the source section position.
            let target_section_idx = source_col_idx;

            self.add_tile(
                Some(target_section_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            if source_tile_was_active {
                // We added to the left, don't activate even further left on removal.
                self.activate_prev_section_on_removal = None;
            }

            if target_section_idx <= self.active_section_idx {
                // Tiles on the start side animate from the following section.
                move_offset += main_space_vec(
                    self.section_main_pos(target_section_idx + 1)
                        - self.section_main_pos(target_section_idx),
                );
            }

            let new_col = &mut self.sections[target_section_idx];
            move_offset += prev_off - new_col.tile_offset(0);
            new_col.tile_mut(0).animate_move_from(move_offset);
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if self.sections.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.sections
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles_enumerated()
                        .find_map(|(tile_idx, tile)| (tile.window().id() == window).then_some(tile_idx))
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_section_idx;
            let source_tile_idx = self.sections[self.active_section_idx].active_leaf_idx();
            (source_col_idx, source_tile_idx)
        };

        let source_section_main = self.section_main_pos(source_col_idx);

        let source_section = &self.sections[source_col_idx];
        let mut move_offset = main_space_vec(source_section.render_offset().x);
        let prev_off = source_section.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_section_idx == source_col_idx
            && source_section.active_leaf_idx() == source_tile_idx;

        if source_section.tiles_len() == 1 {
            if source_col_idx + 1 == self.sections.len() {
                return;
            }

            // Move into adjacent section.
            let target_section_idx = source_col_idx;

            move_offset +=
                main_space_vec(source_section_main - self.section_main_pos(source_col_idx + 1));
            move_offset -= main_space_vec(self.sections[source_col_idx + 1].render_offset().x);

            if source_tile_was_active {
                // Make sure the target section gets activated.
                self.activate_prev_section_on_removal = None;
            }

            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_section(target_section_idx, None, tile, source_tile_was_active);

            let target_section = &mut self.sections[target_section_idx];
            move_offset += prev_off - target_section.tile_offset(target_section.tiles_len() - 1);

            let new_tile = target_section.last_tile_mut();
            new_tile.animate_move_from(move_offset);
        } else {
            // Move out of section.
            let prev_width = self.sections[source_col_idx].width();

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            let target_section_idx = source_col_idx + 1;

            self.add_tile(
                Some(target_section_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            move_offset += main_space_vec(if self.active_section_idx <= target_section_idx {
                // Tiles on the end side animate to the following section.
                source_section_main - self.section_main_pos(target_section_idx)
            } else {
                // Tiles on the start side animate for a change in width.
                -f64::max(0., prev_width - self.sections[target_section_idx].width())
            });

            let new_col = &mut self.sections[target_section_idx];
            move_offset += prev_off - new_col.tile_offset(0);
            new_col.tile_mut(0).animate_move_from(move_offset);
        }
    }

    pub fn consume_into_section(&mut self) {
        if self.sections.len() < 2 {
            return;
        }

        if self.active_section_idx == self.sections.len() - 1 {
            return;
        }

        let target_section_idx = self.active_section_idx;
        let source_section_idx = self.active_section_idx + 1;

        let main_delta = self.section_main_pos(source_section_idx)
            + self.sections[source_section_idx].render_offset().x
            - self.section_main_pos(target_section_idx);
        let mut move_offset = main_space_vec(main_delta);
        let prev_off = self.sections[source_section_idx].tile_offset(0);

        let removed = self.remove_tile_by_idx(source_section_idx, 0, Transaction::new(), None);
        self.add_tile_to_section(target_section_idx, None, removed.tile, false);

        let target_section = &mut self.sections[target_section_idx];
        move_offset += prev_off - target_section.tile_offset(target_section.tiles_len() - 1);
        move_offset -= main_space_vec(target_section.render_offset().x);

        let new_tile = target_section.last_tile_mut();
        new_tile.animate_move_from(move_offset);
    }

    pub fn expel_from_section(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let source_col_idx = self.active_section_idx;
        let target_col_idx = self.active_section_idx + 1;
        let source_section_main = self.section_main_pos(source_col_idx);

        let source_section = &self.sections[self.active_section_idx];
        if source_section.tiles_len() == 1 {
            return;
        }

        let source_tile_idx = source_section.tiles_len() - 1;

        let mut move_offset = main_space_vec(source_section.render_offset().x);
        let prev_off = source_section.tile_offset(source_tile_idx);

        let removed =
            self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

        self.add_tile(
            Some(target_col_idx),
            removed.tile,
            false,
            removed.width,
            removed.is_full_width,
            Some(self.options.animations.window_movement.0),
        );

        move_offset += main_space_vec(source_section_main - self.section_main_pos(target_col_idx));

        let new_col = &mut self.sections[target_col_idx];
        move_offset += prev_off - new_col.tile_offset(0);
        new_col.tile_mut(0).animate_move_from(move_offset);
    }

    /// Sets the pending split direction for the focused section.
    /// The next window opened in this section will be placed in a split
    /// with the currently-focused window, rather than appended to the section.
    pub fn split_window(&mut self, direction: Option<SplitAxis>) {
        if self.sections.is_empty() {
            return;
        }

        let direction = direction.unwrap_or(SplitAxis::Main);
        let col = &mut self.sections[self.active_section_idx];
        col.pending_split_direction = Some(direction);
    }

    /// Consumes a window from an adjacent section into a split with the focused window.
    ///
    /// Takes the focused window from an adjacent section (to the right by default)
    /// and places it side-by-side with the focused window in the current section
    /// via a main-axis split.
    pub fn consume_window_into_split(
        &mut self,
        direction: Option<SplitAxis>,
        id: Option<&W::Id>,
    ) {
        if self.sections.len() < 2 {
            return;
        }

        let split_axis = direction.unwrap_or(SplitAxis::Main);

        // Find the source window: either by id or the active window in the adjacent section.
        let (source_col_idx, source_tile_idx) = if let Some(id) = id {
            // Find the window by id in any section except the active one.
            self.sections
                .iter()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    if col_idx == self.active_section_idx {
                        return None;
                    }
                    col.tiles_enumerated()
                        .find(|(_, tile)| tile.window().id() == id)
                        .map(|(tile_idx, _)| (col_idx, tile_idx))
                })
                .unwrap_or_else(|| {
                    // Fallback: if the window is in the active section, use the adjacent section's active tile.
                    if self.active_section_idx == 0 {
                        (1, self.sections[1].active_leaf_idx())
                    } else {
                        (self.active_section_idx - 1,
                         self.sections[self.active_section_idx - 1].active_leaf_idx())
                    }
                })
        } else {
            // Default: consume from the section to the right.
            let source_col_idx = if self.active_section_idx + 1 < self.sections.len() {
                self.active_section_idx + 1
            } else {
                self.active_section_idx.saturating_sub(1)
            };
            if source_col_idx == self.active_section_idx {
                return;
            }
            (source_col_idx, self.sections[source_col_idx].active_leaf_idx())
        };

        let target_col_idx = self.active_section_idx;
        let target_tile_idx = self.sections[target_col_idx].active_leaf_idx();

        // Don't create a split in a fullscreen/maximized section — the invariant requires
        // single-leaf or tabbed for non-normal sizing modes.
        if !self.sections[target_col_idx].pending_sizing_mode().is_normal() {
            return;
        }

        // Capture positions for animation.
        let _prev_target_pos = self.sections[target_col_idx].active_tile_offset();
        let prev_source_pos = self.sections[source_col_idx].active_tile_offset()
            + main_space_vec(
                self.section_main_pos(source_col_idx) - self.section_main_pos(target_col_idx),
            );

        // Remove the source tile.
        let removed =
            self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

        // After removal, the source section may have been removed entirely (if it had only one
        // tile), shifting section indices. Adjust the target section index accordingly.
        let target_col_idx = if source_col_idx < target_col_idx {
            target_col_idx - 1
        } else {
            target_col_idx
        };

        // Capture the start position of the sections after the target before the split changes the
        // target section's width, so we can animate the shift.
        let prev_next_main_pos = self.section_main_pos(target_col_idx + 1);

        // Now add the removed tile as a split child of the focused tile in the target section.
        let target_section = &mut self.sections[target_col_idx];
        target_section.add_tile_to_split(target_tile_idx, removed.tile, split_axis, true, true);


        // Animate the new tile from its previous position.
        let target_section = &mut self.sections[target_col_idx];
        let new_pos = target_section.active_tile_offset();
        let move_offset = prev_source_pos - new_pos;
        target_section.active_tile_mut().animate_move_from(move_offset);

        // Animate section movements if width changed.
        // (The split may have changed the section width.)
        self.animate_sections_after_change(target_col_idx, prev_next_main_pos);
    }

    /// Animates section movements after a change that might have affected section width.
    ///
    /// `prev_next_main_pos` is the main-axis position of the first section after `col_idx`,
    /// captured *before* the change.
    fn animate_sections_after_change(&mut self, col_idx: usize, prev_next_main_pos: f64) {
        // Move other sections to account for width changes.
        let offset = self.section_main_pos(col_idx + 1) - prev_next_main_pos;
        if offset != 0. {
            let movement_config = self.options.animations.window_movement.0;
            if self.active_section_idx <= col_idx {
                for col in &mut self.sections[col_idx + 1..] {
                    col.animate_move_from_with_config(-offset, movement_config);
                }
            } else {
                for col in &mut self.sections[..=col_idx] {
                    col.animate_move_from_with_config(offset, movement_config);
                }
            }
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if self.sections.is_empty() {
            return;
        }

        // Swap within the section's Main-axis splits first (at any nesting depth); only fall
        // through to inter-section movement at the tree edge.
        let delta = match direction {
            ScrollDirection::Left => -1,
            ScrollDirection::Right => 1,
        };
        if self.sections[self.active_section_idx].swap_in_axis(SplitAxis::Main, delta) {
            return;
        }

        // if this is the first (resp. last section), then this operation is equivalent
        // to an `consume_or_expel_window_left` (resp. `consume_or_expel_window_right`)
        match direction {
            ScrollDirection::Left => {
                if self.active_section_idx == 0 {
                    return;
                }
            }
            ScrollDirection::Right => {
                if self.active_section_idx == self.sections.len() - 1 {
                    return;
                }
            }
        }

        let source_section_idx = self.active_section_idx;
        let target_section_idx = self.active_section_idx.wrapping_add_signed(match direction {
            ScrollDirection::Left => -1,
            ScrollDirection::Right => 1,
        });

        // if both source and target sections contain a single tile, then the operation is equivalent
        // to a simple section move
        if self.sections[source_section_idx].tiles_len() == 1
            && self.sections[target_section_idx].tiles_len() == 1
        {
            return self.move_section_to(target_section_idx);
        }

        let source_tile_idx = self.sections[source_section_idx].active_leaf_idx();
        let target_tile_idx = self.sections[target_section_idx].active_leaf_idx();
        let source_section_drained = self.sections[source_section_idx].tiles_len() == 1;

        // Capture the two windows' identities up front. Insertions can reshape the target tree
        // (e.g. `add_tile_at` wraps a SplitH root in a new Cross split, so the target tile's flat
        // index does *not* simply shift by one), so we re-resolve positions by window id afterward
        // instead of trusting index arithmetic.
        let source_window = self.sections[source_section_idx]
            .tile(source_tile_idx)
            .window()
            .id()
            .clone();
        let target_window = self.sections[target_section_idx]
            .tile(target_tile_idx)
            .window()
            .id()
            .clone();

        // capture the original positions of the tiles
        let (mut source_pt, mut target_pt) = (
            self.sections[source_section_idx].render_offset()
                + self.sections[source_section_idx].tile_offset(source_tile_idx),
            self.sections[target_section_idx].render_offset()
                + self.sections[target_section_idx].tile_offset(target_tile_idx),
        );
        source_pt.x += self.section_main_pos(source_section_idx);
        target_pt.x += self.section_main_pos(target_section_idx);

        let transaction = Transaction::new();

        // If the source section contains a single tile, this will also remove the section.
        // When this happens `source_section_drained` will be set and the section will need to be
        // recreated with `add_tile`
        let source_removed = self.remove_tile_by_idx(
            source_section_idx,
            source_tile_idx,
            transaction.clone(),
            None,
        );

        {
            // special case when the source section disappears after removing its last tile
            let adjusted_target_section_idx =
                if direction == ScrollDirection::Right && source_section_drained {
                    target_section_idx - 1
                } else {
                    target_section_idx
                };

            self.add_tile_to_section(
                adjusted_target_section_idx,
                Some(target_tile_idx),
                source_removed.tile,
                false,
            );

            // Re-resolve the original target tile by identity: after inserting the source tile the
            // target's flat index may be unchanged (SplitH-root wrap) or shifted, so `+ 1` is wrong.
            let target_flat_idx = self.sections[adjusted_target_section_idx]
                .position(&target_window)
                .expect("target window still present after inserting the source tile");
            let RemovedTile {
                tile: target_tile, ..
            } = self.remove_tile_by_idx(
                adjusted_target_section_idx,
                target_flat_idx,
                transaction.clone(),
                None,
            );

            if source_section_drained {
                // recreate the drained section with only the target tile
                self.add_tile(
                    Some(source_section_idx),
                    target_tile,
                    true,
                    source_removed.width,
                    source_removed.is_full_width,
                    None,
                )
            } else {
                // simply add the removed target tile to the source section
                self.add_tile_to_section(
                    source_section_idx,
                    Some(source_tile_idx),
                    target_tile,
                    false,
                );
            }
        }

        // Activate the swapped-in window in each section by identity — `set_active_tile_idx` writes
        // the root `active_idx` directly with no bounds check, so feeding it a flat leaf index can
        // put it out of range and panic later. After the swap the source section holds the target
        // window and vice versa.
        self.sections[source_section_idx].activate_window(&target_window);
        self.sections[target_section_idx].activate_window(&source_window);

        // Animations. Re-resolve each moved tile's flat index by identity (the tree may have been
        // reshaped by the insertions).
        let src_in_target = self.sections[target_section_idx]
            .position(&source_window)
            .expect("source window present in the target section after the swap");
        self.sections[target_section_idx]
            .tile_mut(src_in_target)
            .animate_move_from(source_pt - target_pt);
        self.sections[target_section_idx]
            .tile_mut(src_in_target)
            .ensure_alpha_animates_to_1();

        // FIXME: this stop_move_animations() causes the target tile animation to "reset" when
        // swapping. It's here as a workaround to stop the unwanted animation of moving the source
        // tile down when adding the target tile above it. This code needs to be written in some
        // other way not to trigger that animation, or to cancel it properly, so that swap doesn't
        // cancel all ongoing target tile animations.
        let tgt_in_source = self.sections[source_section_idx]
            .position(&target_window)
            .expect("target window present in the source section after the swap");
        self.sections[source_section_idx]
            .tile_mut(tgt_in_source)
            .stop_move_animations();
        self.sections[source_section_idx]
            .tile_mut(tgt_in_source)
            .animate_move_from(target_pt - source_pt);
        self.sections[source_section_idx]
            .tile_mut(tgt_in_source)
            .ensure_alpha_animates_to_1();

        self.activate_section(target_section_idx);
    }

    /// The (section index, flat leaf index) slot of `id`, if it lives in this scrolling layout.
    pub fn position_of(&self, id: &W::Id) -> Option<(usize, usize)> {
        for (sec_idx, section) in self.sections.iter().enumerate() {
            if let Some(leaf_idx) = section
                .root
                .leaves()
                .position(|(tile, _)| tile.window().id() == id)
            {
                return Some((sec_idx, leaf_idx));
            }
        }
        None
    }

    /// Swaps the two tiles addressed by `(section_idx, flat_leaf_idx)` — sway's centre-drop. The
    /// two windows exchange slots; window count and the tree shape are unchanged (no new tab/split).
    /// Same parent uses the cheap `swap_leaves` (children + data move together); otherwise the leaf
    /// `Tile` payloads are swapped in place so each window adopts the other's slot.
    pub fn swap_tiles(&mut self, a: (usize, usize), b: (usize, usize)) {
        if a == b {
            return;
        }

        let (a_sec, a_leaf) = a;
        let (b_sec, b_leaf) = b;
        if a_sec >= self.sections.len() || b_sec >= self.sections.len() {
            return;
        }

        if a_sec == b_sec {
            self.sections[a_sec].swap_leaves_by_flat_idx(a_leaf, b_leaf);
            return;
        }

        // Cross-section: swap the two leaf tiles' contents and resize each to fit its new slot.
        let (lo_sec, hi_sec) = (a_sec.min(b_sec), a_sec.max(b_sec));
        let (lo_leaf, hi_leaf) = if a_sec < b_sec {
            (a_leaf, b_leaf)
        } else {
            (b_leaf, a_leaf)
        };
        let (left, right) = self.sections.split_at_mut(hi_sec);
        let sec_lo = &mut left[lo_sec];
        let sec_hi = &mut right[0];
        if lo_leaf >= sec_lo.tiles_len() || hi_leaf >= sec_hi.tiles_len() {
            return;
        }

        let prev_lo = sec_lo.leaf_positions_by_id();
        let prev_hi = sec_hi.leaf_positions_by_id();
        std::mem::swap(sec_lo.tile_mut(lo_leaf), sec_hi.tile_mut(hi_leaf));
        sec_lo.update_tile_sizes(true);
        sec_hi.update_tile_sizes(true);
        sec_lo.animate_leaves_if_moved(&prev_lo);
        sec_hi.animate_leaves_if_moved(&prev_hi);
    }

    /// The tile at `(section_idx, flat_leaf_idx)`, if in range. Used to inspect an in-place drag's
    /// swap target (e.g. its sizing mode) before committing to a swap.
    pub fn tile_at(&self, section_idx: usize, flat_leaf_idx: usize) -> Option<&Tile<W>> {
        let section = self.sections.get(section_idx)?;
        if flat_leaf_idx >= section.tiles_len() {
            return None;
        }
        Some(section.tile(flat_leaf_idx))
    }

    /// Mutable access to the tile at `(section_idx, flat_leaf_idx)`, if in range. Used by the
    /// cross-workspace swap to exchange two tiles that live in different `ScrollingSpace`s.
    pub fn tile_mut_at(&mut self, section_idx: usize, flat_leaf_idx: usize) -> Option<&mut Tile<W>> {
        let section = self.sections.get_mut(section_idx)?;
        if flat_leaf_idx >= section.tiles_len() {
            return None;
        }
        Some(section.tile_mut(flat_leaf_idx))
    }

    /// The pre-swap leaf positions of `section_idx` (keyed by window id), captured before a
    /// cross-workspace swap so its tiles can slide into their new slots afterwards. Mirrors the
    /// `leaf_positions_by_id()` capture in `swap_tiles`' cross-section path.
    pub fn section_leaf_positions(&self, section_idx: usize) -> Vec<(W::Id, Point<f64, Logical>)> {
        self.sections
            .get(section_idx)
            .map(|section| section.leaf_positions_by_id())
            .unwrap_or_default()
    }

    /// Animates any leaf in `section_idx` whose position changed from `prev` — the tail of a
    /// cross-workspace swap on a shared output, mirroring `swap_tiles`' `animate_leaves_if_moved`.
    pub fn animate_section_leaves_if_moved(
        &mut self,
        section_idx: usize,
        prev: &[(W::Id, Point<f64, Logical>)],
    ) {
        if let Some(section) = self.sections.get_mut(section_idx) {
            section.animate_leaves_if_moved(prev);
        }
    }

    /// Resizes the tiles in `section_idx` to fit their slots — the tail of a cross-tree swap, where
    /// the adopted tile may have come from a differently-sized slot (mirrors the `update_tile_sizes`
    /// in `swap_tiles`' cross-section path).
    pub fn update_section_tile_sizes(&mut self, section_idx: usize, animate: bool) {
        if let Some(section) = self.sections.get_mut(section_idx) {
            section.update_tile_sizes(animate);
        }
    }

    pub fn toggle_section_tabbed_display(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let col = &mut self.sections[self.active_section_idx];
        let display = match col.display_mode() {
            SectionDisplay::Normal => SectionDisplay::Tabbed,
            SectionDisplay::Tabbed => SectionDisplay::Normal,
        };

        self.set_section_display(display);
    }

    /// Toggles the container *holding the active window* between split and tabbed.
    ///
    /// This is the generalized version that works at any tree level: if the focused window sits
    /// directly under the section root, it toggles the whole section (same as
    /// `toggle_section_tabbed_display`); if it sits inside a nested split (e.g. a side-by-side
    /// row), only that nested split becomes tabbed, leaving the rest of the section in place.
    pub fn toggle_tabbed(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let col = &self.sections[self.active_section_idx];
        let path = col.root.active_path();
        // The parent of the active leaf is the container we toggle.
        let parent_len = path.len().saturating_sub(1);

        if parent_len == 0 {
            // The active window's container is the section root. Use the section-level toggle, which
            // has the nicer cross-axis fade animation and clears fullscreen/maximized when leaving
            // tabbed mode with more than one tile.
            let display = if col.is_tabbed() {
                SectionDisplay::Normal
            } else {
                SectionDisplay::Tabbed
            };
            self.set_section_display(display);
            return;
        }

        // The active window is nested: toggle just its parent split/tabbed node in place.
        let parent_path = path[..parent_len].to_vec();
        let col = &mut self.sections[self.active_section_idx];
        cancel_resize_for_section(&mut self.interactive_resize, col);

        let tab_header_config = col.options.layout.tab_header.clone();
        let node = col.root.node_at_mut(&parent_path);
        let became_tabbed = !node.is_tabbed();
        node.toggle_tabbed(tab_header_config);

        // Fade the new tab header in, mirroring the section-level transition.
        if became_tabbed {
            let clock = col.clock.clone();
            let anim = col.options.animations.window_movement.0;
            if let TileNode::Internal { tab_header: Some(tab_header), .. } =
                col.root.node_at_mut(&parent_path)
            {
                tab_header.start_open_animation(clock, anim);
            }
        }

        // Un-tabbing can leave the node a same-family plain split of its parent (e.g. a tabbed leaf
        // under a SplitV root un-tabs to a SplitV directly inside SplitV), violating the merge
        // invariant. Normalize as `set_active_layout` does.
        col.root.simplify();
        col.collapse_redundant_root_wrapper();

        col.update_tile_sizes(true);
    }

    /// Sets the layout of the container holding the active window (sway's `layout` command). A
    /// window directly under the section root sets the whole section's layout; a window inside a
    /// nested split sets just that nested container. Same-family redundancy is then merged away.
    pub fn set_active_layout(&mut self, new_layout: Layout) {
        if self.sections.is_empty() {
            return;
        }

        let col = &self.sections[self.active_section_idx];
        let path = col.root.active_path();
        let parent_len = path.len().saturating_sub(1);
        let parent_path = path[..parent_len].to_vec();

        let col = &mut self.sections[self.active_section_idx];
        cancel_resize_for_section(&mut self.interactive_resize, col);

        let cfg = col.options.layout.tab_header.clone();
        let node = col.root.node_at_mut(&parent_path);
        let was_tabbing = node.is_tabbed();
        node.set_layout(new_layout, cfg);

        // Fade in a new header when entering a tabbing layout.
        if new_layout.is_tabbing() && !was_tabbing {
            let clock = col.clock.clone();
            let anim = col.options.animations.window_movement.0;
            if let TileNode::Internal { tab_header: Some(h), .. } = col.root.node_at_mut(&parent_path)
            {
                h.start_open_animation(clock, anim);
            }
        }

        // Changing a node's layout can leave it same-family-redundant with its parent.
        col.root.simplify();
        col.collapse_redundant_root_wrapper();

        // A non-tabbed multi-tile section can't stay fullscreen/maximized.
        let col = &self.sections[self.active_section_idx];
        if !col.is_tabbed() && col.tiles_len() > 1 {
            let window = col.active_tile().window().id().clone();
            self.set_fullscreen(&window, false);
            self.set_maximized(&window, false);
        }

        self.sections[self.active_section_idx].update_tile_sizes(true);
    }

    /// Toggles the container holding the active window between horizontal and vertical split
    /// (sway's `layout toggle split`). A tabbing container converts to its family's split.
    pub fn toggle_split_layout(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        let col = &self.sections[self.active_section_idx];
        let path = col.root.active_path();
        let parent_len = path.len().saturating_sub(1);
        let parent_path = path[..parent_len].to_vec();
        let new = match col.root.node_at(&parent_path).layout() {
            Some(Layout::SplitH) => Layout::SplitV,
            Some(Layout::SplitV) => Layout::SplitH,
            Some(l) if l.is_tabbing() => l.split_of_family(),
            _ => return,
        };
        self.set_active_layout(new);
    }

    /// Moves the active tab left or right within its tabbed container.
    pub fn move_tab(&mut self, direction: ScrollDirection) {
        if self.sections.is_empty() {
            return;
        }

        let col = &mut self.sections[self.active_section_idx];
        if !col.is_tabbed() {
            return;
        }

        // Tabs are the root's *children* (each may itself be a split), so operate on child indices,
        // not flat leaf indices.
        let active_idx = col.root.active_idx();
        let new_idx = match direction {
            ScrollDirection::Left => active_idx.saturating_sub(1),
            ScrollDirection::Right => {
                let max = col.root.child_count().saturating_sub(1);
                (active_idx + 1).min(max)
            }
        };

        if new_idx == active_idx {
            return;
        }

        // Swap the two tabs (children + data) and follow the moved tab.
        col.root.swap_leaves(active_idx, new_idx);
        col.set_active_tile_idx(new_idx);
        col.active_tile_mut().ensure_alpha_animates_to_1();
        col.update_tile_sizes(true);
    }

    pub fn set_section_display(&mut self, display: SectionDisplay) {
        if self.sections.is_empty() {
            return;
        }

        let col = &mut self.sections[self.active_section_idx];
        if col.display_mode() == display {
            return;
        }

        cancel_resize_for_section(&mut self.interactive_resize, col);
        col.set_section_display(display);
        col.update_tile_sizes(true);

        // Refresh the cached section data after the size recompute (toggling display can change the
        // section width, e.g. tabbing a Main split widens it).

        // Disable fullscreen if needed.
        let col = &self.sections[self.active_section_idx];
        if !col.is_tabbed() && col.tiles_len() > 1 {
            let window = col.active_tile().window().id().clone();
            self.set_fullscreen(&window, false);
            self.set_maximized(&window, false);
        }
    }

    pub fn center_section(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        self.animate_view_offset_to_section_centered(
            None,
            self.active_section_idx,
            self.options.animations.horizontal_view_movement.0,
        );

        let col = &mut self.sections[self.active_section_idx];
        cancel_resize_for_section(&mut self.interactive_resize, col);
    }

    pub fn center_window(&mut self, window: Option<&W::Id>) {
        if self.sections.is_empty() {
            return;
        }

        let col_idx = if let Some(window) = window {
            self.sections
                .iter()
                .position(|col| col.contains(window))
                .unwrap()
        } else {
            self.active_section_idx
        };

        // We can reasonably center only the active section.
        if col_idx != self.active_section_idx {
            return;
        }

        self.center_section();
    }

    pub fn center_visible_sections(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        if self.is_centering_focused_section() {
            return;
        }

        // Consider the end of an ongoing animation because that's what compute-to-fit does too.
        let target_view_main = self.target_view_main_pos();
        let work_area_main = self.working_area.loc.x;
        let work_area_span = self.working_area.size.w;

        // Count all sections that are fully visible inside the working area.
        let mut occupied_span = 0.;
        let mut first_visible_section_main = None;
        let mut active_section_main = None;

        let gap = self.options.layout.gaps;
        let section_mains = self.section_main_positions();
        for (idx, section_main) in section_mains.take(self.sections.len()).enumerate() {
            if section_main < target_view_main + work_area_main + gap {
                // Section goes off-screen on the start side.
                continue;
            }

            first_visible_section_main.get_or_insert(section_main);

            let section_span = self.sections[idx].width();
            if target_view_main + work_area_main + work_area_span < section_main + section_span + gap
            {
                // Section goes off-screen on the end side. We can stop here.
                break;
            }

            if idx == self.active_section_idx {
                active_section_main = Some(section_main);
            }

            occupied_span += section_span + gap;
        }

        if active_section_main.is_none() {
            // The active section wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.sections[self.active_section_idx];
        cancel_resize_for_section(&mut self.interactive_resize, col);

        let free_span = work_area_span - occupied_span + gap;
        let new_view_main = first_visible_section_main.unwrap() - free_span / 2. - work_area_main;

        self.animate_view_offset(
            self.active_section_idx,
            new_view_main - active_section_main.unwrap(),
        );
        // Just in case.
        self.animate_view_offset_to_section(None, self.active_section_idx, None);
    }

    pub fn view_main_pos(&self) -> f64 {
        self.section_main_pos(self.active_section_idx) + self.view_offset.current()
    }

    pub fn view_pos(&self) -> f64 {
        self.view_main_pos()
    }

    pub fn target_view_main_pos(&self) -> f64 {
        self.section_main_pos(self.active_section_idx) + self.view_offset.target()
    }

    pub fn target_view_pos(&self) -> f64 {
        self.target_view_main_pos()
    }

    /// The main-axis start position of each section (one entry per section, plus one past the last).
    /// Section widths are computed on the fly from the sections themselves, so there is no cache to
    /// keep in sync.
    fn section_main_positions(&self) -> impl Iterator<Item = f64> {
        let gaps = self.options.layout.gaps;
        // Collect the widths into an owned Vec so the returned iterator doesn't borrow `self`
        // (callers like `sections_mut` need to mutably borrow the sections afterwards).
        let widths: Vec<f64> = self.sections.iter().map(|c| c.width()).collect();
        let mut main = 0.;
        // Chain with a dummy width to be able to get one past all sections' position.
        widths.into_iter().chain(iter::once(0.)).map(move |w| {
            let rv = main;
            main += w + gaps;
            rv
        })
    }

    fn section_main_pos(&self, section_idx: usize) -> f64 {
        self.section_main_positions()
            .nth(section_idx)
            .unwrap()
    }

    fn section_main_positions_in_render_order(&self) -> impl Iterator<Item = f64> {
        let active_idx = self.active_section_idx;
        let active_pos = self.section_main_pos(active_idx);
        let offsets = self
            .section_main_positions()
            .enumerate()
            .filter_map(move |(idx, pos)| (idx != active_idx).then_some(pos));
        iter::once(active_pos).chain(offsets)
    }

    pub fn sections(&self) -> impl Iterator<Item = &Section<W>> {
        self.sections.iter()
    }

    fn sections_mut(&mut self) -> impl Iterator<Item = (&mut Section<W>, f64)> + '_ {
        let offsets = self.section_main_positions();
        zip(&mut self.sections, offsets)
    }

    fn sections_in_render_order(&self) -> impl Iterator<Item = (&Section<W>, f64)> + '_ {
        let offsets = self.section_main_positions_in_render_order();

        let (first, active, rest) = if self.sections.is_empty() {
            (&[][..], &[][..], &[][..])
        } else {
            let (first, rest) = self.sections.split_at(self.active_section_idx);
            let (active, rest) = rest.split_at(1);
            (first, active, rest)
        };

        let sections = active.iter().chain(first).chain(rest);
        zip(sections, offsets)
    }

    fn sections_in_render_order_mut(&mut self) -> impl Iterator<Item = (&mut Section<W>, f64)> + '_ {
        let offsets = self.section_main_positions_in_render_order();

        let (first, active, rest) = if self.sections.is_empty() {
            (&mut [][..], &mut [][..], &mut [][..])
        } else {
            let (first, rest) = self.sections.split_at_mut(self.active_section_idx);
            let (active, rest) = rest.split_at_mut(1);
            (first, active, rest)
        };

        let sections = active.iter_mut().chain(first).chain(rest);
        zip(sections, offsets)
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scale = self.scale;
        let axis = self.axis();
        let view_off = Point::from((-self.view_main_pos(), 0.));
        self.sections_in_render_order()
            .flat_map(move |(col, section_main)| {
                let section_offset = main_space_vec(section_main);
                let section_render_offset = col.render_offset();
                col.tiles_in_render_order()
                    .map(move |(tile, tile_off, visible)| {
                        let pos = view_off
                            + section_offset
                            + section_render_offset
                            + tile_off
                            + tile.render_offset();
                        let pos = axis.point_out(pos);
                        // Round to physical pixels.
                        let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        (tile, pos, visible)
                    })
            })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        let axis = self.axis();
        let view_off = Point::from((-self.view_main_pos(), 0.));
        self.sections_in_render_order_mut()
            .flat_map(move |(col, section_main)| {
                let section_offset = main_space_vec(section_main);
                let section_render_offset = col.render_offset();
                col.tiles_in_render_order_mut()
                    .map(move |(tile, tile_off)| {
                        let mut pos = view_off
                            + section_offset
                            + section_render_offset
                            + tile_off
                            + tile.render_offset();
                        pos = axis.point_out(pos);
                        // Round to physical pixels.
                        if round {
                            pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        }
                        (tile, pos)
                    })
            })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scale = self.scale;
        let axis = self.axis();
        let view_off = Point::from((-self.view_main_pos(), 0.));

        let section_mains = self.section_main_positions();
        zip(self.sections.iter(), section_mains).enumerate().flat_map(
            move |(col_idx, (col, section_main))| {
                let section_offset = main_space_vec(section_main);
                col.tiles()
                    .enumerate()
                    .map(move |(tile_idx, (tile, tile_off))| {
                        let pos = view_off + section_offset + tile_off;
                        let pos = axis.point_out(pos);
                        // Round to physical pixels.
                        let pos = pos.to_physical_precise_round(scale).to_logical(scale);

                        // The full tree path from the section root to this leaf, as 1-based child
                        // indices (consistent with the actions). For a flat section this is a
                        // single element; for split/tabbed sections it encodes the nesting.
                        let path: Vec<usize> = col
                            .root
                            .path_for_leaf_index(tile_idx)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|i| i + 1)
                            .collect();

                        let layout = WindowLayout {
                            tile_pos_in_workspace_view: Some(pos.into()),
                            pos_in_scrolling_layout: Some((col_idx + 1, path)),
                            ..tile.ipc_layout_template()
                        };
                        (tile, layout)
                    })
            },
        )
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        let mut hint_area = match position {
            InsertPosition::NewSection(section_index) => {
                if section_index == 0 || section_index == self.sections.len() {
                    let size = Size::from((
                        300.,
                        self.working_area.size.h - self.options.layout.gaps * 2.,
                    ));
                    let mut loc = Point::from((
                        self.section_main_pos(section_index),
                        self.working_area.loc.y + self.options.layout.gaps,
                    ));
                    if section_index == 0 && !self.sections.is_empty() {
                        loc.x -= size.w + self.options.layout.gaps;
                    }
                    Rectangle::new(loc, size)
                } else if section_index > self.sections.len() {
                    error!("insert hint section index is out of range");
                    return None;
                } else {
                    let size = Size::from((
                        300.,
                        self.working_area.size.h - self.options.layout.gaps * 2.,
                    ));
                    let loc = Point::from((
                        self.section_main_pos(section_index)
                            - size.w / 2.
                            - self.options.layout.gaps / 2.,
                        self.working_area.loc.y + self.options.layout.gaps,
                    ));
                    Rectangle::new(loc, size)
                }
            }
            InsertPosition::InSection(section_index, tile_index) => {
                if section_index > self.sections.len() {
                    error!("insert hint section index is out of range");
                    return None;
                }

                let col = &self.sections[section_index];
                if tile_index > col.tiles_len() {
                    error!("insert hint tile index is out of range");
                    return None;
                }

                let is_tabbed = col.is_tabbed();

                let (height, y) = if is_tabbed {
                    // In tabbed mode, there's only one tile visible, and we want to draw the hint
                    // at its top or bottom.
                    let active_off = col.active_tile_offset();
                    let top = active_off.y;
                    let active_path = col.root.active_leaf_path();
                    let bottom = top + col.root.leaf_data(&active_path).map(|d| d.size.h).unwrap_or(0.);

                    if tile_index <= col.active_tile_idx() {
                        (150., top)
                    } else {
                        (150., bottom - 150.)
                    }
                } else {
                    let top = col.tile_offset(tile_index).y;

                    if tile_index == 0 {
                        (150., top)
                    } else if tile_index == col.tiles_len() {
                        (150., top - self.options.layout.gaps - 150.)
                    } else {
                        (300., top - self.options.layout.gaps / 2. - 150.)
                    }
                };

                // Adjust for place-within-section tab indicator.
                let origin_x = col.tiles_origin().x;
                let extra_w = if is_tabbed && col.sizing_mode().is_normal() {
                    // One header row/tab per direct root child (matches the renderer), not per flat
                    // leaf — a Stacked band is `child_count` rows tall.
                    col.tab_header().unwrap().extra_size(col.root.child_count(), col.scale).w
                } else {
                    0.
                };

                let size = Size::from((self.sections[section_index].width() - extra_w, height));
                let loc = Point::from((self.section_main_pos(section_index) + origin_x, y));
                Rectangle::new(loc, size)
            }
            InsertPosition::InSplit(section_index, tile_index, axis, place_after) => {
                if section_index >= self.sections.len() {
                    return None;
                }
                let col = &self.sections[section_index];
                if tile_index >= col.tiles_len() {
                    return None;
                }

                let tile_off = col.tile_offset(tile_index);
                // Use path-based data access for nested split support.
                let tile_path = col.root.path_for_leaf_index(tile_index);
                let (tile_w, tile_h) = tile_path
                    .as_ref()
                    .and_then(|p| col.root.leaf_data(p))
                    .map(|d| (d.size.w, d.size.h))
                    .unwrap_or((0., 0.));
                let col_main = self.section_main_pos(section_index);

                // Show a half-size rectangle on the side the new tile will land: a half-width
                // rect (left/right) for a side-by-side Main split, a half-height rect (top/bottom)
                // for a stacking Cross split.
                match axis {
                    SplitAxis::Main => {
                        let half_w = tile_w / 2.;
                        let loc = if place_after {
                            Point::from((col_main + tile_off.x + half_w, tile_off.y))
                        } else {
                            Point::from((col_main + tile_off.x, tile_off.y))
                        };
                        Rectangle::new(loc, Size::from((half_w, tile_h)))
                    }
                    SplitAxis::Cross => {
                        let half_h = tile_h / 2.;
                        let loc = if place_after {
                            Point::from((col_main + tile_off.x, tile_off.y + half_h))
                        } else {
                            Point::from((col_main + tile_off.x, tile_off.y))
                        };
                        Rectangle::new(loc, Size::from((tile_w, half_h)))
                    }
                }
            }
            InsertPosition::InsertTab(section_index, tile_index)
            | InsertPosition::Swap(section_index, tile_index) => {
                if section_index >= self.sections.len() {
                    return None;
                }
                let col = &self.sections[section_index];
                if tile_index >= col.tiles_len() {
                    return None;
                }
                let tile_off = col.tile_offset(tile_index);
                let (tile_w, tile_h) = col
                    .root
                    .path_for_leaf_index(tile_index)
                    .as_ref()
                    .and_then(|p| col.root.leaf_data(p))
                    .map(|d| (d.size.w, d.size.h))
                    .unwrap_or((0., 0.));
                let col_main = self.section_main_pos(section_index);
                // The new tab fills the whole target tile.
                Rectangle::new(
                    Point::from((col_main + tile_off.x, tile_off.y)),
                    Size::from((tile_w, tile_h)),
                )
            }
            InsertPosition::Floating => return None,
        };

        // First window on an empty workspace will cancel out any view offset. Replicate this
        // effect here.
        if self.sections.is_empty() {
            let view_offset = if self.is_centering_focused_section() {
                self.compute_new_view_offset_centered(
                    Some(0.),
                    0.,
                    hint_area.size.w,
                    SizingMode::Normal,
                )
            } else {
                self.compute_new_view_offset_fit(Some(0.), 0., hint_area.size.w, SizingMode::Normal)
            };
            hint_area.loc.x -= view_offset;
        } else {
            hint_area.loc.x -= self.view_main_pos();
        }

        Some(self.map_rect_out(hint_area))
    }

    /// Returns the geometry of the active window relative to and clamped to the view.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let col = self.sections.get(self.active_section_idx)?;

        let final_view_offset = self.view_offset.target();
        let view_off = Point::from((-final_view_offset, 0.));

        let (tile, tile_off) = col.tiles().nth(col.active_leaf_idx()).unwrap();

        let window_pos = view_off + tile_off + self.map_point_in(tile.window_loc());
        let window_size = self.axis().size_in(tile.window_size());
        let window_rect = Rectangle::new(window_pos, window_size);

        let view = Rectangle::from_size(self.view_size);
        view.intersection(window_rect)
            .map(|rect| self.map_rect_out(rect))
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        for col in &self.sections {
            for (tile, pos) in col.tiles() {
                if tile.window().id() == id {
                    // In the scrolling layout, we try to position popups horizontally within the
                    // window geometry (so they remain visible even if the window scrolls flush with
                    // the left/right edge of the screen), and vertically within the whole parent
                    // working area.
                    let axis = self.axis();
                    let window_size = axis.size_out(tile.window_size());
                    let window_loc = axis.point_out(tile.window_loc());
                    let width = window_size.w;
                    let height = self.parent_area.size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.parent_area.loc.y;
                    target.loc.y -= pos.y;
                    target.loc.y -= window_loc.y;

                    return Some(self.map_rect_out(target));
                }
            }
        }
        None
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.sections.is_empty() {
            return;
        }

        let idx = self.active_section_idx;
        self.sections[idx].toggle_width(None, forwards);
        cancel_resize_for_section(&mut self.interactive_resize, &mut self.sections[idx]);
    }

    pub fn toggle_full_width(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let idx = self.active_section_idx;
        self.sections[idx].toggle_full_width();
        cancel_resize_for_section(&mut self.interactive_resize, &mut self.sections[idx]);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.sections.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
            let tile_idx = self.sections[col_idx]
                .tiles_enumerated()
                .find(|(_, tile)| tile.window().id() == window)
                .map(|(idx, _)| idx);
            (col_idx, tile_idx)
        } else {
            (self.active_section_idx, None)
        };

        let col = &mut self.sections[col_idx];
        // Resize the leaf's main-axis span within its nearest Main-axis split; with no such split
        // ancestor `set_split_child_width` falls back to resizing the whole section width. Pass a
        // flat leaf index (the active leaf when unspecified), which is what it expects.
        let tile_idx = tile_idx.unwrap_or_else(|| col.active_leaf_idx());
        col.set_split_child_width(change, tile_idx, true);

        // The section width may have changed; keep the cached section data in sync.
        cancel_resize_for_section(&mut self.interactive_resize, &mut self.sections[col_idx]);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.sections.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
            let tile_idx = self.sections[col_idx]
                .tiles_enumerated()
                .find(|(_, tile)| tile.window().id() == window)
                .map(|(idx, _)| idx);
            (col_idx, tile_idx)
        } else {
            (self.active_section_idx, None)
        };

        let col = &mut self.sections[col_idx];
        col.set_window_height(change, tile_idx, true);

        cancel_resize_for_section(&mut self.interactive_resize, col);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if self.sections.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
            let tile_idx = self.sections[col_idx]
                .tiles_enumerated()
                .find(|(_, tile)| tile.window().id() == window)
                .map(|(idx, _)| idx);
            (col_idx, tile_idx)
        } else {
            (self.active_section_idx, None)
        };

        let col = &mut self.sections[col_idx];
        col.reset_window_height(tile_idx);

        cancel_resize_for_section(&mut self.interactive_resize, col);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.sections.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
            let tile_idx = self.sections[col_idx]
                .tiles_enumerated()
                .find(|(_, tile)| tile.window().id() == window)
                .map(|(idx, _)| idx);
            (col_idx, tile_idx)
        } else {
            (self.active_section_idx, None)
        };

        self.sections[col_idx].toggle_width(tile_idx, forwards);
        cancel_resize_for_section(&mut self.interactive_resize, &mut self.sections[col_idx]);
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.sections.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
            let tile_idx = self.sections[col_idx]
                .tiles_enumerated()
                .find(|(_, tile)| tile.window().id() == window)
                .map(|(idx, _)| idx);
            (col_idx, tile_idx)
        } else {
            (self.active_section_idx, None)
        };

        let col = &mut self.sections[col_idx];
        col.toggle_window_height(tile_idx, forwards);

        cancel_resize_for_section(&mut self.interactive_resize, col);
    }

    pub fn expand_section_to_available_width(&mut self) {
        if self.sections.is_empty() {
            return;
        }

        let col = &mut self.sections[self.active_section_idx];
        if !col.pending_sizing_mode().is_normal() || col.is_full_width {
            return;
        }

        if self.is_centering_focused_section() {
            // Always-centered mode is different since the active window position cannot be
            // controlled (it's always at the center). I guess you could come up with different
            // logic here that computes the width in such a way so as to leave nearby sections fully
            // on screen while taking into account that the active section will remain centered
            // after resizing. But I'm not sure it's that useful? So let's do the simple thing.
            let idx = self.active_section_idx;
            self.sections[idx].toggle_full_width();
            cancel_resize_for_section(&mut self.interactive_resize, &mut self.sections[idx]);
            return;
        }

        // NOTE: This logic won't work entirely correctly with small fixed-size maximized windows
        // (they have a different area and padding).

        // Consider the end of an ongoing animation because that's what compute-to-fit does too.
        let target_view_main = self.target_view_main_pos();
        let work_area_main = self.working_area.loc.x;
        let work_area_span = self.working_area.size.w;

        // Count all sections that are fully visible inside the working area.
        let mut occupied_span = 0.;
        let mut first_visible_section_main = None;
        let mut active_section_main = None;
        let mut counted_non_active_section = false;

        let gap = self.options.layout.gaps;
        let section_mains = self.section_main_positions();
        for (idx, section_main) in section_mains.take(self.sections.len()).enumerate() {
            if section_main < target_view_main + work_area_main + gap {
                // Section goes off-screen on the start side.
                continue;
            }

            first_visible_section_main.get_or_insert(section_main);

            let section_span = self.sections[idx].width();
            if target_view_main + work_area_main + work_area_span < section_main + section_span + gap
            {
                // Section goes off-screen on the end side. We can stop here.
                break;
            }

            if idx == self.active_section_idx {
                active_section_main = Some(section_main);
            } else {
                counted_non_active_section = true;
            }

            occupied_span += section_span + gap;
        }

        if active_section_main.is_none() {
            // The active section wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.sections[self.active_section_idx];

        let available_span = work_area_span - gap - occupied_span - col.extra_size().w;
        if available_span <= 0. {
            // Nowhere to expand.
            return;
        }

        cancel_resize_for_section(&mut self.interactive_resize, col);

        let idx = self.active_section_idx;
        if !counted_non_active_section {
            // Only the active section was fully on-screen (maybe it's the only section), so we're
            // about to set its width to 100% of the working area. Let's do it via
            // toggle_full_width() as it lets you back out of it more intuitively.
            self.sections[idx].toggle_full_width();
            return;
        }

        let active_span = self.sections[idx].width();
        let col = &mut self.sections[idx];
        col.width = SectionWidth::Fixed(active_span + available_span);
        col.preset_width_idx = None;
        col.is_full_width = false;
        col.update_tile_sizes(true);

        // Put the first visible window into the view.
        let new_view_main = first_visible_section_main.unwrap() - gap - work_area_main;
        self.animate_view_offset(
            self.active_section_idx,
            new_view_main - active_section_main.unwrap(),
        );
        // Just in case.
        self.animate_view_offset_to_section(None, self.active_section_idx, None);
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let mut col_idx = self
            .sections
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if is_fullscreen == self.sections[col_idx].is_pending_fullscreen {
            return false;
        }

        let mut col = &mut self.sections[col_idx];
        let is_tabbed = col.is_tabbed();
        // A tabbed root whose active tab is a nested split would overlap several full-size windows;
        // expel the target instead (see `active_tab_is_nested`). The common all-leaf tabbed root is
        // unaffected.
        let active_tab_nested = col.active_tab_is_nested();

        cancel_resize_for_section(&mut self.interactive_resize, col);

        if is_fullscreen && ((col.tiles_len() > 1 && !is_tabbed) || active_tab_nested) {
            // This wasn't the only window in its section; extract it into a separate section.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.sections[col_idx];
        }

        col.set_fullscreen(is_fullscreen);

        // With place_within_section, the tab indicator changes the section size immediately.

        true
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let mut col_idx = self
            .sections
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if maximize == self.sections[col_idx].is_pending_maximized {
            return false;
        }

        let mut col = &mut self.sections[col_idx];
        let is_tabbed = col.is_tabbed();
        // Same overlap hazard as fullscreen: a tabbed root whose active tab is a nested split.
        let active_tab_nested = col.active_tab_is_nested();

        cancel_resize_for_section(&mut self.interactive_resize, col);

        if maximize && ((col.tiles_len() > 1 && !is_tabbed) || active_tab_nested) {
            // This wasn't the only window in its section; extract it into a separate section.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.sections[col_idx];
        }

        col.set_maximized(maximize);

        // With place_within_section, the tab indicator changes the section size immediately.

        true
    }

    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer if we're on a fullscreen window and the view is stationary.
        if self.sections.is_empty() {
            return false;
        }

        if !self.view_offset.is_static() {
            return false;
        }

        self.sections[self.active_section_idx]
            .sizing_mode()
            .is_fullscreen()
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        let view_size = self.map_size_out(self.view_size);
        let view_loc = self.map_point_out(main_space_vec(self.view_main_pos()));
        let view_rect = Rectangle::new(view_loc, view_size);
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            push(elem.into());
        }

        if self.sections.is_empty() {
            return;
        }

        let mut first = true;

        // This matches self.tiles_in_render_order().
        let view_off = main_space_vec(-self.view_main_pos());
        for (col, section_main) in self.sections_in_render_order() {
            let section_offset = main_space_vec(section_main);
            let section_render_offset = col.render_offset();

            // Draw the tab indicator on top.
            let header_pos = {
                let pos = view_off + section_offset + section_render_offset;
                let pos = self.map_point_out(pos);
                pos.to_physical_precise_round(scale).to_logical(scale)
            };
            if let Some(tab_indicator) = col.tab_header() {
                tab_indicator
                    .render(ctx.renderer, header_pos, &mut |elem| push(elem.into()));
            }
            // Nested tabbed headers, Indicator style (Bar style is drawn in the second pass below).
            for unit in col.collect_nested_tabbed() {
                if !unit.visible {
                    continue;
                }
                let header = col.root.node_at(&unit.path).tab_header();
                if let Some(header @ TabHeader::Indicator(_)) = header {
                    header.render(ctx.renderer, header_pos, &mut |elem| push(elem.into()));
                }
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                let tile_pos = view_off
                    + section_offset
                    + section_render_offset
                    + tile_off
                    + tile.render_offset();
                let tile_pos = self.map_point_out(tile_pos);
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                // And now the drawing logic.

                // For the active tile (which comes first), draw the focus ring.
                let focus_ring = focus_ring && first;
                first = false;

                // In the scrolling layout, we currently use visible only for hidden tabs in the
                // tabbed mode. We want to animate their opacity when going in and out of tabbed
                // mode, so we don't want to apply "visible" immediately. However, "visible" is
                // also used for input handling, and there we *do* want to apply it immediately.
                // So, let's just selectively ignore "visible" here when animating alpha.
                let visible = visible || tile.alpha_animation.is_some();
                if !visible {
                    continue;
                }

                let xray_pos = xray_pos.offset(tile_pos);
                tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                    push(elem.into())
                });
            }
        }

        // Second pass: render TabBar (i3/sway-style header bar) for Bar-style tab headers.
        // We collect the immutable per-section data first, then do a mutable pass to render,
        // because TabBar::render needs &mut self (for texture caching) while we also need
        // immutable access to section positions and titles.
        let view_off = main_space_vec(-self.view_main_pos());
        let section_mains: Vec<f64> = self.section_main_positions().collect();
        let active_section_idx = self.active_section_idx;

        // Collect render data for each Bar-style header: the section root header, plus any nested
        // tabbed nodes (a tabbed row, etc.). `path` is empty for the section root.
        /// Per-header data for TabBar rendering.
        struct BarRenderData {
            col_idx: usize,
            path: TilePath,
            pos: Point<f64, Logical>,
            titles: Vec<String>,
            is_active: bool,
        }
        let mut bar_render_data: Vec<BarRenderData> = Vec::new();
        for (col_idx, col) in self.sections.iter().enumerate() {
            let section_main = section_mains[col_idx];
            let section_offset = main_space_vec(section_main);
            let section_render_offset = col.render_offset();
            let pos = view_off + section_offset + section_render_offset;
            let pos = self.map_point_out(pos);
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            let is_section_active = col_idx == active_section_idx;

            // The section root header.
            if col.is_tabbed() && matches!(col.tab_header(), Some(TabHeader::Bar(_))) {
                // One title per *direct child* of the root: a tab whose child is a nested container
                // shows a group label (e.g. `H[2]`) rather than one descendant window's title.
                let titles: Vec<String> =
                    col.tab_children(&[]).into_iter().map(|c| c.title).collect();
                bar_render_data.push(BarRenderData {
                    col_idx,
                    path: Vec::new(),
                    pos,
                    titles,
                    is_active: is_section_active,
                });
            }

            // Nested tabbed Bar headers.
            for unit in col.collect_nested_tabbed() {
                if !unit.visible {
                    continue;
                }
                if !matches!(
                    col.root.node_at(&unit.path).tab_header(),
                    Some(TabHeader::Bar(_))
                ) {
                    continue;
                }
                let titles: Vec<String> =
                    col.tab_children(&unit.path).into_iter().map(|c| c.title).collect();
                bar_render_data.push(BarRenderData {
                    col_idx,
                    path: unit.path,
                    pos,
                    titles,
                    is_active: is_section_active && unit.active_on_path,
                });
            }
        }

        // Now render each Bar-style tab header.
        for data in bar_render_data {
            let title_refs: Vec<&str> = data.titles.iter().map(|s| s.as_str()).collect();
            let col = &self.sections[data.col_idx];
            if let Some(TabHeader::Bar(bar)) = col.root.node_at(&data.path).tab_header() {
                let gles_ctx = ctx.as_gles();
                bar.render(
                    gles_ctx.renderer,
                    data.pos,
                    self.scale,
                    data.is_active,
                    &title_refs,
                    &mut |elem| push(elem.into()),
                );
            }
        }
    }

    /// Scrolls the tab bar under the given pointer position, if any.
    /// Returns true if a tab bar was scrolled.
    pub fn scroll_tab_bar(&mut self, pos: Point<f64, Logical>, delta: f64) -> bool {
        let pos_in = self.map_point_in(pos);
        let scale = self.scale;
        let view_off = main_space_vec(-self.view_main_pos());
        let section_mains: Vec<f64> = self.section_main_positions().collect();

        // First pass: find which section's tab bar is under the pointer. The bar is drawn in the
        // reserved header band *outside* the content rect (above it for Top, below for Bottom — see
        // `TabBar::update_render_elements`), NOT at `tab_indicator_area()` which starts at
        // `tiles_origin()` (already shifted past the band). Hit-test that reserved band, mirroring
        // how `header_band_target` / the click hit-test locate the drawn header.
        let mut target_col_idx = None;
        for (col_idx, col) in self.sections.iter().enumerate() {
            if !col.is_tabbed() || !col.sizing_mode().is_normal() {
                continue;
            }
            let Some(header) = col.tab_header() else {
                continue;
            };
            let count = col.root.child_count();
            let extra = header.extra_size(count, scale);
            if extra.w <= 0. && extra.h <= 0. {
                continue;
            }
            let offset = header.content_offset(count, scale);
            let content = col.tab_indicator_area();
            let band = Rectangle::new(content.loc - offset, content.size + extra);

            let section_main = section_mains[col_idx];
            let section_offset = main_space_vec(section_main);
            let section_render_offset = col.render_offset();
            let section_pos = view_off + section_offset + section_render_offset;
            let section_pos = section_pos.to_physical_precise_round(scale).to_logical(scale);
            let local = pos_in - section_pos;
            if band.contains(local) && !content.contains(local) {
                target_col_idx = Some(col_idx);
                break;
            }
        }

        // Second pass: scroll the tab bar.
        if let Some(col_idx) = target_col_idx {
            let view_size = self.map_size_out(self.view_size);
            let col = &mut self.sections[col_idx];
            let area = col.tab_indicator_area();
            let area_width = area.size.w;
            let is_active = col_idx == self.active_section_idx;
            let scrolled = if let Some(TabHeader::Bar(bar)) = col.tab_header_mut() {
                bar.scroll(delta, area_width)
            } else {
                false
            };
            if scrolled {
                col.update_render_elements(
                    is_active,
                    Rectangle::new(Point::from((0., 0.)), view_size),
                );
            }
            return scrolled;
        }
        false
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // This matches self.tiles_with_render_positions().
        let pos_in = self.map_point_in(pos);
        let scale = self.scale;
        let view_off = main_space_vec(-self.view_main_pos());
        for (col, section_main) in self.sections_in_render_order() {
            let section_offset = main_space_vec(section_main);
            let section_render_offset = col.render_offset();

            // Hit the tab indicator.
            if col.is_tabbed() && col.sizing_mode().is_normal() {
                let section_pos = view_off + section_offset + section_render_offset;
                let section_pos = section_pos
                    .to_physical_precise_round(scale)
                    .to_logical(scale);

                // One tab per direct root child (a nested child is a single group tab), matching the
                // renderer's `child_count`-based layout — not one tab per flat leaf. Map the hit tab
                // to that child's representative leaf so activating it switches to that tab.
                let children = col.tab_children(&[]);
                if let Some(idx) = col.tab_header().unwrap().hit(
                    col.tab_indicator_area(),
                    children.len(),
                    scale,
                    pos_in - section_pos,
                ) {
                    let leaf = children.get(idx).map_or(0, |c| c.rep_leaf_idx);
                    let hit = HitType::Activate {
                        is_tab_indicator: true,
                    };
                    return Some((col.tile(leaf).window(), hit));
                }
            }

            // Hit nested tabbed headers (a tabbed row, etc.). The returned index is the tab index;
            // map it to that tab's representative leaf so activating it switches to that tab.
            if col.sizing_mode().is_normal() {
                let section_pos = (view_off + section_offset + section_render_offset)
                    .to_physical_precise_round(scale)
                    .to_logical(scale);
                for unit in col.collect_nested_tabbed() {
                    if !unit.visible {
                        continue;
                    }
                    let Some(header) = col.root.node_at(&unit.path).tab_header() else {
                        continue;
                    };
                    if let Some(idx) =
                        header.hit(unit.content_area, unit.tab_count, scale, pos_in - section_pos)
                    {
                        let leaf = unit.rep_leaf_idx.get(idx).copied().unwrap_or(0);
                        let hit = HitType::Activate {
                            is_tab_indicator: true,
                        };
                        return Some((col.tile(leaf).window(), hit));
                    }
                }
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                if !visible {
                    continue;
                }

                let tile_pos = view_off
                    + section_offset
                    + section_render_offset
                    + tile_off
                    + tile.render_offset();
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
                let tile_pos = self.map_point_out(tile_pos);

                if let Some((win, hit)) = HitType::hit_tile(tile, tile_pos, pos) {
                    return Some((win, hit));
                }
            }
        }

        None
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        if self.sections.is_empty() {
            return;
        }

        if self.interactive_resize.is_some() {
            return;
        }

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
        };
        self.view_offset = ViewOffset::Gesture(gesture);
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if let ViewOffset::Gesture(ViewGesture {
            dnd_last_event_time: Some(_),
            ..
        }) = &self.view_offset
        {
            // Already active.
            return;
        }

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
        };
        self.view_offset = ViewOffset::Gesture(gesture);

        self.interactive_resize = None;
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        gesture.tracker.push(delta_x, timestamp);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let pos = gesture.tracker.pos() * norm_factor;
        let view_offset = pos + gesture.delta_from_tracker;
        gesture.current_view_offset = view_offset;

        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, delta: f64) -> bool {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_view_scroll;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let view_offset = gesture.tracker.pos() + gesture.delta_from_tracker;

        // Clamp it so that it doesn't go too much out of bounds.
        let (startmost_offset, endmost_offset) = if self.sections.is_empty() {
            (0., 0.)
        } else {
            let gaps = self.options.layout.gaps;

            let mut startmost_offset = -self.working_area.size.w;

            let last_col_idx = self.sections.len() - 1;
            let last_section_main = self
                .sections
                .iter()
                .take(last_col_idx)
                .fold(0., |section_main, col| section_main + col.width() + gaps);
            let last_section_span = self.sections[last_col_idx].width();
            let mut endmost_offset = last_section_main + last_section_span - self.working_area.loc.x;

            let active_section_main = self
                .sections
                .iter()
                .take(self.active_section_idx)
                .fold(0., |section_main, col| section_main + col.width() + gaps);
            startmost_offset -= active_section_main;
            endmost_offset -= active_section_main;

            (startmost_offset, endmost_offset)
        };
        let min_offset = f64::min(startmost_offset, endmost_offset);
        let max_offset = f64::max(startmost_offset, endmost_offset);
        let clamped_offset = view_offset.clamp(min_offset, max_offset);

        gesture.delta_from_tracker += clamped_offset - view_offset;
        gesture.current_view_offset = clamped_offset;
        true
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        // We do not handle cancelling, just like GNOME Shell doesn't. For this gesture, proper
        // cancelling would require keeping track of the original active section, and then updating
        // it in all the right places (adding sections, removing sections, etc.) -- quite a bit of
        // effort and bug potential.

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let velocity = gesture.tracker.velocity() * norm_factor;
        let pos = gesture.tracker.pos() * norm_factor;
        let current_view_offset = pos + gesture.delta_from_tracker;

        if self.sections.is_empty() {
            self.view_offset = ViewOffset::Static(current_view_offset);
            return true;
        }

        // Figure out where the gesture would stop after deceleration.
        let end_pos = gesture.tracker.projected_end_pos() * norm_factor;
        let target_view_offset = end_pos + gesture.delta_from_tracker;

        let snapping_points = self.collect_view_snaps();

        let active_section_main = self.section_main_pos(self.active_section_idx);
        let target_view_main = active_section_main + target_view_offset;
        let target_snap = self.closest_view_snap(&snapping_points, target_view_main);
        let new_col_idx = self.furthest_visible_section_from_snap(
            target_snap,
            target_view_offset,
            current_view_offset,
        );

        let new_section_main = self.section_main_pos(new_col_idx);
        let main_delta = active_section_main - new_section_main;

        if self.active_section_idx != new_col_idx {
            self.view_offset_to_restore = None;
        }

        self.active_section_idx = new_col_idx;

        let target_view_offset = target_snap.view_main_pos - new_section_main;

        self.view_offset = ViewOffset::Animation(Animation::new(
            self.clock.clone(),
            current_view_offset + main_delta,
            target_view_offset,
            velocity,
            self.options.animations.horizontal_view_movement.0,
        ));

        // HACK: deal with things like snapping to the right edge of a larger-than-view window.
        self.animate_view_offset_to_section(None, new_col_idx, None);

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return;
        };

        if gesture.dnd_last_event_time.is_some() && gesture.tracker.pos() == 0. {
            // DnD didn't scroll anything, so preserve the current view position (rather than
            // snapping the window).

            // If there's an ongoing animation within the gesture (e.g. from a window being removed
            // during DnD), preserve it.
            if let Some(mut anim) = gesture.animation.take() {
                anim.offset(gesture.current_view_offset);
                self.view_offset = ViewOffset::Animation(anim);
            } else {
                self.view_offset = ViewOffset::Static(gesture.delta_from_tracker);
            }

            if !self.sections.is_empty() {
                // Just in case, make sure the active window remains on screen.
                self.animate_view_offset_to_section(None, self.active_section_idx, None);
            }
            return;
        }

        self.view_offset_gesture_end(None);
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let axis = self.axis();

        let col = self
            .sections
            .iter_mut()
            .find(|col| col.contains(&window))
            .unwrap();

        if !col.pending_sizing_mode().is_normal() {
            return false;
        }

        let tile = col
            .tiles_enumerated_mut()
            .find(|(_, tile)| tile.window().id() == &window)
            .map(|(_, tile)| tile)
            .unwrap();

        let original_window_size = axis.size_in(tile.window_size());
        let edges = axis.resize_edges_in(edges);

        let resize = InteractiveResize {
            window,
            original_window_size,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);

        self.view_offset.stop_anim_and_gesture();

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let delta = self.axis().point_in(delta);
        let is_centering = self.is_centering_focused_section();

        let col_idx = self.sections.iter().position(|col| col.contains(window)).unwrap();
        let col = &mut self.sections[col_idx];

        let tile_idx = col
            .tiles_enumerated()
            .find(|(_, tile)| tile.window().id() == window)
            .map(|(idx, _)| idx)
            .unwrap();

        if resize.data.edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if resize.data.edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            };

            if is_centering {
                dx *= 2.;
            }

            let window_width = (resize.original_window_size.w + dx).round() as i32;
            // Resize the boundary with the sibling in the tile's nearest Main-axis split (at any
            // nesting depth); with no such split ancestor this falls back to the whole section
            // width. `tile_idx` is already a flat leaf index.
            col.set_split_child_width(SizeChange::SetFixed(window_width), tile_idx, false);
        }

        if resize.data.edges.intersects(ResizeEdge::TOP_BOTTOM) {
            // Prevent the simplest case of weird resizing (top edge when this is the topmost
            // window).
            if !(resize.data.edges.contains(ResizeEdge::TOP) && tile_idx == 0) {
                let mut dy = delta.y;
                if resize.data.edges.contains(ResizeEdge::TOP) {
                    dy = -dy;
                };

                // FIXME: some smarter height distribution would be nice here so that vertical
                // resizes work as expected in more cases.

                let window_height = (resize.original_window_size.h + dy).round() as i32;
                col.set_window_height(SizeChange::SetFixed(window_height), Some(tile_idx), false);
            }
        }

        // Resizing changed the section width; keep the cached section data in sync.

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }

            // Animate the active window into view right away.
            if self.sections[self.active_section_idx].contains(window) {
                self.animate_view_offset_to_section(None, self.active_section_idx, None);
            }
        }

        self.interactive_resize = None;
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        for (col_idx, col) in self.sections.iter_mut().enumerate() {
            let mut col_resize_data = None;
            if let Some(resize) = &self.interactive_resize {
                if col.contains(&resize.window) {
                    col_resize_data = Some(resize.data);
                }
            }

            let is_tabbed = col.is_tabbed();
            let extra_size = col.extra_size();

            // If transactions are disabled, also disable combined throttling, for more intuitive
            // behavior. In tabbed display mode, only one window is visible, so individual
            // throttling makes more sense.
            let individual_throttling = self.options.disable_transactions || is_tabbed;

            let intent = if self.options.disable_resize_throttling {
                ConfigureIntent::CanSend
            } else if individual_throttling {
                // In this case, we don't use combined throttling, but rather compute throttling
                // individually below.
                ConfigureIntent::CanSend
            } else {
                col.tiles_enumerated()
                    .map(|(_, tile)| tile)
                    .fold(ConfigureIntent::NotNeeded, |intent, tile| {
                        match (intent, tile.window().configure_intent()) {
                            (_, ConfigureIntent::ShouldSend) => ConfigureIntent::ShouldSend,
                            (ConfigureIntent::NotNeeded, tile_intent) => tile_intent,
                            (ConfigureIntent::CanSend, ConfigureIntent::Throttled) => {
                                ConfigureIntent::Throttled
                            }
                            (intent, _) => intent,
                        }
                    })
            };

            let active_leaf_idx = col.active_leaf_idx();
            for (tile_idx, tile) in col.tiles_enumerated_mut() {
                let win = tile.window_mut();

                // Compare against the *flat* active leaf index, not the root-child `active_tile_idx`
                // (they differ once the section is nested), so the actually-focused leaf is the one
                // marked active — otherwise a sibling gets activated and the focused window
                // deactivated under `deactivate_unfocused_windows`.
                let active_in_section = active_leaf_idx == tile_idx;
                win.set_active_in_section(active_in_section);
                win.set_floating(false);

                let mut active = is_active && self.active_section_idx == col_idx;
                if self.options.deactivate_unfocused_windows {
                    active &= active_in_section && is_focused;
                } else {
                    // In tabbed mode, all tabs have activated state to reduce unnecessary
                    // animations when switching tabs.
                    active &= active_in_section || is_tabbed;
                }
                win.set_activated(active);

                win.set_interactive_resize(col_resize_data);

                let border_config = self.options.layout.border.merged_with(&win.rules().border);
                let bounds = compute_toplevel_bounds(
                    border_config,
                    self.working_area.size,
                    extra_size,
                    self.options.layout.gaps,
                );
                win.set_bounds(bounds);

                let intent = if individual_throttling {
                    win.configure_intent()
                } else {
                    intent
                };

                if matches!(
                    intent,
                    ConfigureIntent::CanSend | ConfigureIntent::ShouldSend
                ) {
                    win.send_pending_configure();
                }

                win.refresh();
            }
        }
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.parent_area
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn active_section_idx(&self) -> usize {
        self.active_section_idx
    }

    /// Test introspection: per-tab `(union geometry, label)` for the tabbing node at `path` in the
    /// active section (`path` empty = the root header).
    #[cfg(test)]
    pub(super) fn tab_child_infos(
        &self,
        path: &[usize],
    ) -> Vec<(Rectangle<f64, Logical>, String)> {
        let col = &self.sections[self.active_section_idx];
        col.tab_children(path)
            .into_iter()
            .map(|c| (c.geometry, c.title))
            .collect()
    }

    #[cfg(test)]
    pub(super) fn view_offset(&self) -> &ViewOffset {
        &self.view_offset
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(
            self.working_area,
            compute_working_area(self.parent_area, self.scale, self.options.layout.struts)
        );

        if !self.sections.is_empty() {
            assert!(self.active_section_idx < self.sections.len());

            for section in &self.sections {
                assert!(Rc::ptr_eq(&self.options, &section.options));
                assert_eq!(self.clock, section.clock);
                assert_eq!(self.scale, section.scale);
                section.verify_invariants();
            }

            let col = &self.sections[self.active_section_idx];

            if self.view_offset_to_restore.is_some() {
                assert!(
                    !col.sizing_mode().is_normal(),
                    "when view_offset_to_restore is set, \
                     the active section must be fullscreen or maximized"
                );
            }
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.sections
                    .iter()
                    .flat_map(|col| col.tiles_enumerated().map(|(_, tile)| tile))
                    .any(|tile| tile.window().id() == &resize.window),
                "interactive resize window must be present in the layout"
            );
        }
    }
}

impl ViewOffset {
    /// Returns the current view offset.
    pub fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.value(),
            ViewOffset::Gesture(gesture) => {
                gesture.current_view_offset
                    + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    /// Returns the target view offset suitable for computing the new view offset.
    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.to(),
            // This can be used for example if a gesture is interrupted.
            ViewOffset::Gesture(gesture) => gesture.current_view_offset,
        }
    }

    /// Returns a view offset value suitable for saving and later restoration.
    ///
    /// This means that it shouldn't return an in-progress animation or gesture value.
    fn stationary(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            // For animations we can return the final value.
            ViewOffset::Animation(anim) => anim.to(),
            ViewOffset::Gesture(gesture) => gesture.stationary_view_offset,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static(_))
    }

    pub fn is_gesture(&self) -> bool {
        matches!(self, Self::Gesture(_))
    }

    pub fn is_dnd_scroll(&self) -> bool {
        matches!(&self, ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_some())
    }

    pub fn is_animation_ongoing(&self) -> bool {
        match self {
            ViewOffset::Static(_) => false,
            ViewOffset::Animation(_) => true,
            ViewOffset::Gesture(gesture) => gesture.animation.is_some(),
        }
    }

    pub fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(offset) => *offset += delta,
            ViewOffset::Animation(anim) => anim.offset(delta),
            ViewOffset::Gesture(gesture) => {
                gesture.stationary_view_offset += delta;
                gesture.delta_from_tracker += delta;
                gesture.current_view_offset += delta;
            }
        }
    }

    pub fn cancel_gesture(&mut self) {
        if let ViewOffset::Gesture(gesture) = self {
            *self = ViewOffset::Static(gesture.current_view_offset);
        }
    }

    pub fn stop_anim_and_gesture(&mut self) {
        *self = ViewOffset::Static(self.current());
    }
}

impl ViewGesture {
    fn animate_from(&mut self, from: f64, clock: Clock, config: niri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl From<PresetSize> for SectionWidth {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(p) => Self::Proportion(p.clamp(0., 10000.)),
            PresetSize::Fixed(f) => Self::Fixed(f64::from(f.clamp(1, 100000))),
        }
    }
}

impl<W: LayoutElement> Section<W> {
    // --- Tree accessors ---

    /// Returns the display mode (Normal or Tabbed).
    fn display_mode(&self) -> SectionDisplay {
        self.root.display_mode()
    }

    /// Returns whether the section is in tabbed display mode.
    fn is_tabbed(&self) -> bool {
        self.root.is_tabbed()
    }

    /// Returns the number of tiles (leaves) in this section (recursive).
    fn tiles_len(&self) -> usize {
        self.root.leaf_count()
    }

    /// Returns the active tile index (index into the root's children).
    fn active_tile_idx(&self) -> usize {
        self.root.active_idx()
    }

    /// The flat-leaf index of the active leaf (following `active_idx` down the tree). Use this when
    /// passing a target to a flat-leaf-indexed operation (remove, split, resize); `active_tile_idx`
    /// is only the root child index and they differ once the section is nested.
    fn active_leaf_idx(&self) -> usize {
        self.root.path_for_leaf_index_from_active().unwrap_or(0)
    }

    /// Maps a flat-leaf index to the root child whose subtree contains it (used when inserting a
    /// new top-level row at a flat position). Returns `child_count` for an out-of-range / past-end
    /// index.
    fn leaf_idx_to_root_child(&self, leaf_idx: usize) -> usize {
        match self.root.path_for_leaf_index(leaf_idx) {
            Some(path) if !path.is_empty() => path[0],
            _ => self.root.child_count(),
        }
    }

    /// The flat-leaf index of the first leaf under root child `root_child`.
    fn root_child_first_leaf_idx(&self, root_child: usize) -> usize {
        match &self.root {
            TileNode::Leaf(_) => 0,
            TileNode::Internal { children, .. } => {
                children.iter().take(root_child).map(TileNode::leaf_count).sum()
            }
        }
    }

    /// Returns the active tile (immutable).
    fn active_tile(&self) -> &Tile<W> {
        self.root.active_leaf()
    }

    /// Returns the active tile (mutable).
    fn active_tile_mut(&mut self) -> &mut Tile<W> {
        self.root.active_leaf_mut()
    }

    /// Returns the render offset of the active leaf (recursive).
    fn active_tile_offset(&self) -> Point<f64, Logical> {
        let origin = self.tiles_origin();
        self.root
            .active_leaf_offset(origin, self.options.layout.gaps, self.scale, self.axis(), true)
    }

    /// Returns the last leaf (mutable).
    fn last_tile_mut(&mut self) -> &mut Tile<W> {
        self.root.last_leaf_mut()
    }

    /// Returns the child data slice (for Split/Tabbed roots).
    fn data(&self) -> &[SplitChildData] {
        match &self.root {
            TileNode::Leaf(_) => &[],
            TileNode::Internal { data, .. } => data,
        }
    }

    /// Returns the child data slice (mutable, for Split/Tabbed roots).
    fn data_mut(&mut self) -> &mut [SplitChildData] {
        match &mut self.root {
            TileNode::Leaf(_) => &mut [],
            TileNode::Internal { data, .. } => data,
        }
    }

    /// Returns a reference to the tile at the given flat index (recursive — counts all leaves).
    fn tile(&self, idx: usize) -> &Tile<W> {
        self.root
            .leaves()
            .nth(idx)
            .map(|(tile, _)| tile)
            .unwrap_or_else(|| panic!("tile index {idx} out of bounds (leaves: {})", self.root.leaf_count()))
    }

    /// Returns a mutable reference to the tile at the given flat index (recursive).
    fn tile_mut(&mut self, idx: usize) -> &mut Tile<W> {
        let count = self.root.leaf_count();
        self.root
            .leaves_mut()
            .nth(idx)
            .map(|(tile, _)| tile)
            .unwrap_or_else(|| panic!("tile index {idx} out of bounds (leaves: {count})"))
    }

    /// Returns the tab header (if tabbed).
    fn tab_header(&self) -> Option<&TabHeader> {
        self.root.tab_header()
    }

    /// Returns the tab header (mutable, if tabbed).
    fn tab_header_mut(&mut self) -> Option<&mut TabHeader> {
        self.root.tab_header_mut()
    }

    /// Returns an iterator over all tiles (leaves) with their flat indices (recursive).
    fn tiles_enumerated(&self) -> impl Iterator<Item = (usize, &Tile<W>)> + '_ {
        self.root.leaves().enumerate().map(|(idx, (tile, _))| (idx, tile))
    }

    /// Returns a mutable iterator over all tiles (leaves) with their flat indices (recursive).
    fn tiles_enumerated_mut(&mut self) -> impl Iterator<Item = (usize, &mut Tile<W>)> + '_ {
        self.root.leaves_mut().enumerate().map(|(idx, (tile, _))| (idx, tile))
    }

    // --- Tree mutators ---

    /// Sets the active tile index (root-level active child).
    fn set_active_tile_idx(&mut self, idx: usize) {
        self.root.set_active_idx(idx);
    }

    /// Sets the display mode (Normal/Tabbed), toggling the root node type.
    fn set_display_mode(&mut self, display: SectionDisplay) {
        self.root
            .set_display(display, self.options.layout.tab_header.clone());
    }

    /// Whether the section root is a tabbing container whose *active tab* is a nested split (not a
    /// single leaf). Fullscreening or maximizing such a section would size every leaf to the full
    /// area while the split tab renders its leaves at their tree positions — several full-size
    /// windows overlapping — so the caller expels the target window into its own section instead.
    fn active_tab_is_nested(&self) -> bool {
        match &self.root {
            TileNode::Internal { layout, children, active_idx, .. } if layout.is_tabbing() => {
                children
                    .get(*active_idx)
                    .is_some_and(|c| !matches!(c, TileNode::Leaf(_)))
            }
            _ => false,
        }
    }

    /// Inserts a tile at the given index, creating a new leaf child with auto span.
    fn insert_tile(&mut self, idx: usize, tile: Tile<W>) {
        let mut data = SplitChildData::new_auto();
        data.update(&tile, self.axis());
        self.root.insert_leaf(idx, tile, data);
    }

    /// Removes the tile at the given flat index and returns it.
    fn remove_tile(&mut self, idx: usize) -> Tile<W> {
        // Convert flat leaf index to a path, then use path-based removal.
        let path = self
            .root
            .path_for_leaf_index(idx)
            .unwrap_or_else(|| panic!("tile index {idx} out of bounds"));
        let tile = self
            .root
            .remove_leaf(&path)
            .unwrap_or_else(|| panic!("failed to remove leaf at path {path:?}"));
        // Canonicalize the tree (reap empties, flatten single-child nodes, merge same-family
        // splits), then drop a redundant single-child root wrapper (keeping a lone-leaf root, the
        // single-window section).
        self.root.simplify();
        self.collapse_redundant_root_wrapper();
        tile
    }

    /// If the root is a Split/Tabbed with a single non-leaf child, replace the root with that child
    /// (repeatedly), so the meaningful split/tabs become the root. Keeps a lone-leaf root wrapper
    /// (the canonical single-window section) intact.
    fn collapse_redundant_root_wrapper(&mut self) {
        loop {
            let collapse = matches!(
                &self.root,
                TileNode::Internal { children, .. }
                    if children.len() == 1 && !matches!(children[0], TileNode::Leaf(_))
            );
            if !collapse {
                break;
            }
            let child = match &mut self.root {
                TileNode::Internal { children, .. } => {
                    children.remove(0)
                }
                TileNode::Leaf(_) => unreachable!(),
            };
            self.root = child;
        }
    }

    /// Resizes the main-axis span of the leaf at flat index `tile_idx` by resizing the appropriate
    /// child slot of the nearest Main-axis (`SplitH`) ancestor split — the width analogue of
    /// [`set_window_height`]'s cross-axis walk. If the leaf has no `SplitH` ancestor, its "width" is
    /// the whole section's main extent, so the section width is resized instead.
    fn set_split_child_width(&mut self, change: SizeChange, tile_idx: usize, animate: bool) {
        let Some(path) = self.root.path_for_leaf_index(tile_idx) else {
            return;
        };

        // Walk from the root down the leaf's path to the *nearest* (deepest) `SplitH` ancestor. The
        // slot we resize is that split's child on the path. With no such ancestor a horizontal
        // resize is meaningless at the tile level: the leaf already spans the section's main extent,
        // so resize the whole section instead.
        let child_path = {
            let mut target_len = None;
            for k in 0..path.len() {
                if matches!(
                    self.root.node_at(&path[..k]),
                    TileNode::Internal { layout: Layout::SplitH, .. }
                ) {
                    target_len = Some(k + 1);
                }
            }
            match target_len {
                Some(len) => path[..len].to_vec(),
                None => {
                    self.set_section_width(change, Some(tile_idx), animate);
                    return;
                }
            }
        };

        let (child_idx, parent_path) = child_path.split_last().unwrap();
        let child_idx = *child_idx;

        let current_span = self.root.leaf_data(&child_path).map(|d| d.size.w).unwrap_or(0.);

        let new_span = match change {
            SizeChange::SetFixed(fixed) => f64::from(fixed).clamp(1., 100000.),
            SizeChange::SetProportion(proportion) => {
                let available = self.working_area.size.w - self.options.layout.gaps;
                available * (proportion / 100.)
            }
            SizeChange::AdjustFixed(delta) => (current_span + f64::from(delta)).clamp(1., 100000.),
            SizeChange::AdjustProportion(delta) => {
                let available = self.working_area.size.w - self.options.layout.gaps;
                let current_proportion = if available == 0. { 1. } else { current_span / available };
                available * (current_proportion + delta / 100.)
            }
        };

        // Set the resized child to the new fixed span, and convert its siblings within the same
        // split to Auto with a weight proportional to their current span so they keep their relative
        // proportions when the remaining space is redistributed. (Weights are relative, so the raw
        // span works as the weight directly.)
        if let TileNode::Internal { layout, data, .. } = self.root.node_at_mut(parent_path) {
            if *layout == Layout::SplitH {
                data[child_idx].span = ChildSpan::Fixed(new_span);
                for (i, d) in data.iter_mut().enumerate() {
                    if i != child_idx {
                        let weight = if d.size.w > 0. { d.size.w } else { 1. };
                        d.span = ChildSpan::Auto { weight };
                    }
                }
            }
        }

        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    /// Adds a tile as a split child of the tile at `target_idx`.
    ///
    /// `place_after` controls which side of the target the new tile lands on (the half the user
    /// hovered when dragging): `true` inserts after/right/below, `false` before/left/above.
    ///
    /// If the root is already a `Split` with the matching axis, the new tile is inserted into it.
    /// Otherwise the target leaf is replaced with a nested `Split { axis, [target, new] }` (ordered
    /// by `place_after`). If `activate` is true, the new tile becomes the active tile.
    fn add_tile_to_split(
        &mut self,
        target_idx: usize,
        mut tile: Tile<W>,
        axis: SplitAxis,
        place_after: bool,
        activate: bool,
    ) {
        tile.update_config(
            self.map_size_out(self.view_size),
            self.scale,
            self.options.clone(),
        );

        // Capture previous on-screen positions keyed by window id, so existing tiles animate to
        // their new positions after the structure changes (id keys survive Vec reordering).
        let prev = self.leaf_positions_by_id();

        let mut new_data = SplitChildData::new_auto();
        new_data.update(&tile, self.axis());

        // Builds the two-element child/data vecs for a fresh nested split, ordered by `place_after`,
        // and returns them along with the index the new tile ends up at.
        let make_pair = |old_tile: Tile<W>, old_data: SplitChildData, new_tile: Tile<W>| {
            if place_after {
                (
                    vec![TileNode::Leaf(old_tile), TileNode::Leaf(new_tile)],
                    vec![old_data, new_data],
                    1usize,
                )
            } else {
                (
                    vec![TileNode::Leaf(new_tile), TileNode::Leaf(old_tile)],
                    vec![new_data, old_data],
                    0usize,
                )
            }
        };

        let placeholder = || TileNode::internal(Layout::SplitV, Vec::new(), 0, Vec::new(), None);
        let split_layout = Layout::split_for_axis(axis);

        // Resolve the target leaf's position in the tree, then split *there* (at any depth). The
        // path to the new leaf is recorded so it can be activated afterwards.
        let path = self.root.path_for_leaf_index(target_idx);
        let new_leaf_path: TilePath;

        if matches!(&self.root, TileNode::Leaf(_)) {
            // Single-tile section: replace the root leaf with a split.
            let TileNode::Leaf(old_tile) = std::mem::replace(&mut self.root, placeholder()) else {
                unreachable!()
            };
            let (children, data, new_idx) = make_pair(old_tile, SplitChildData::new_auto(), tile);
            let internal_active = if activate { new_idx } else { 1 - new_idx };
            new_leaf_path = vec![new_idx];
            self.root = TileNode::internal(split_layout, children, internal_active, data, None);
        } else if let Some(path) = path.filter(|p| !p.is_empty()) {
            let child = *path.last().unwrap();
            let parent_path = path[..path.len() - 1].to_vec();

            // If the target leaf's parent already arranges its children along `axis`, insert the
            // new tile as a sibling next to the target (on the requested side). Otherwise wrap the
            // target leaf in a fresh nested split.
            let parent_matches = matches!(
                self.root.node_at(&parent_path),
                TileNode::Internal { layout, .. } if layout.is_split() && layout.axis() == axis
            );

            let mut leaf_path = parent_path.clone();
            {
                let TileNode::Internal { children, data, active_idx, .. } =
                    self.root.node_at_mut(&parent_path)
                else {
                    unreachable!("parent path points to a leaf")
                };

                if parent_matches {
                    let insert_idx = if place_after { child + 1 } else { child };
                    children.insert(insert_idx, TileNode::Leaf(tile));
                    data.insert(insert_idx, new_data);
                    if !activate && *active_idx >= insert_idx {
                        *active_idx += 1;
                    }
                    leaf_path.push(insert_idx);
                } else {
                    let TileNode::Leaf(old_tile) =
                        std::mem::replace(&mut children[child], placeholder())
                    else {
                        unreachable!("leaf path did not point to a leaf")
                    };
                    let (sub_children, sub_data, new_idx) =
                        make_pair(old_tile, SplitChildData::new_auto(), tile);
                    let internal_active = if activate { new_idx } else { 1 - new_idx };
                    children[child] =
                        TileNode::internal(split_layout, sub_children, internal_active, sub_data, None);
                    leaf_path.push(child);
                    leaf_path.push(new_idx);
                }
            }
            new_leaf_path = leaf_path;
        } else {
            // Defensive: the target index didn't resolve to a leaf in a non-leaf root. Append at
            // root level so the tile isn't lost.
            let idx = self.root.child_count();
            self.insert_tile(idx, tile);
            new_leaf_path = self
                .root
                .path_for_leaf_index(self.root.leaf_count().saturating_sub(1))
                .unwrap_or_default();
        }

        self.finish_add_tile_to_split(new_leaf_path, activate, prev);
    }

    /// Groups `tile` into a tabbed container with the leaf at `target_idx` (drag centre-drop): if
    /// the target's parent is already a tabbing container the tile joins it as a new tab next to
    /// the target, otherwise the target leaf is wrapped in a fresh `Tabbed` container `[target,
    /// tile]`. The new tile becomes the active tab.
    fn add_tile_as_tab(&mut self, target_idx: usize, mut tile: Tile<W>, activate: bool) {
        tile.update_config(
            self.map_size_out(self.view_size),
            self.scale,
            self.options.clone(),
        );
        let prev = self.leaf_positions_by_id();
        let cfg = self.options.layout.tab_header.clone();

        let mut new_data = SplitChildData::new_auto();
        new_data.update(&tile, self.axis());

        // Wraps the target leaf and the new tile into a fresh tabbed container, new tile last/active.
        let wrap = |old_tile: Tile<W>, new_tile: Tile<W>, new_data: SplitChildData| {
            TileNode::internal(
                Layout::Tabbed,
                vec![TileNode::Leaf(old_tile), TileNode::Leaf(new_tile)],
                if activate { 1 } else { 0 },
                vec![SplitChildData::new_auto(), new_data],
                Some(TabHeader::new(cfg.clone())),
            )
        };

        let path = self.root.path_for_leaf_index(target_idx);
        let new_leaf_path: TilePath = if matches!(&self.root, TileNode::Leaf(_)) {
            let TileNode::Leaf(old) = std::mem::replace(
                &mut self.root,
                TileNode::internal(Layout::SplitV, Vec::new(), 0, Vec::new(), None),
            ) else {
                unreachable!()
            };
            self.root = wrap(old, tile, new_data);
            vec![1]
        } else if let Some(path) = path.filter(|p| !p.is_empty()) {
            let child = *path.last().unwrap();
            let parent_path = path[..path.len() - 1].to_vec();
            let parent_is_tabbing = self
                .root
                .node_at(&parent_path)
                .layout()
                .is_some_and(|l| l.is_tabbing());

            let mut leaf_path = parent_path.clone();
            let TileNode::Internal { children, data, active_idx, .. } =
                self.root.node_at_mut(&parent_path)
            else {
                unreachable!("parent path points to a leaf")
            };
            if parent_is_tabbing {
                // Join the existing tab/stack container as a new tab after the target.
                let insert_idx = child + 1;
                children.insert(insert_idx, TileNode::Leaf(tile));
                data.insert(insert_idx, new_data);
                if activate {
                    *active_idx = insert_idx;
                } else if *active_idx >= insert_idx {
                    *active_idx += 1;
                }
                leaf_path.push(insert_idx);
            } else {
                // Wrap the target leaf in a fresh tabbed container `[target, tile]`.
                let placeholder = TileNode::internal(Layout::SplitV, Vec::new(), 0, Vec::new(), None);
                let TileNode::Leaf(old) = std::mem::replace(&mut children[child], placeholder)
                else {
                    unreachable!("leaf path did not point to a leaf")
                };
                children[child] = wrap(old, tile, new_data);
                leaf_path.push(child);
                // The new tile is always index 1 in the wrapped `[target, tile]`; activation is
                // handled separately via the wrapper's active_idx.
                leaf_path.push(1);
            }
            leaf_path
        } else {
            let idx = self.root.child_count();
            self.insert_tile(idx, tile);
            self.root
                .path_for_leaf_index(self.root.leaf_count().saturating_sub(1))
                .unwrap_or_default()
        };

        self.finish_add_tile_to_split(new_leaf_path, activate, prev);
    }

    /// Shared tail of `add_tile_to_split`: activate the new leaf, collapse redundant wrappers,
    /// relayout, and animate.
    fn finish_add_tile_to_split(
        &mut self,
        new_leaf_path: TilePath,
        activate: bool,
        prev: Vec<(W::Id, Point<f64, Logical>)>,
    ) {
        if activate && !new_leaf_path.is_empty() {
            self.root.activate_path(&new_leaf_path);
            self.root
                .leaf_at_mut(&new_leaf_path)
                .ensure_alpha_animates_to_1();
        }

        // Canonicalize (merge same-family splits, flatten single-child nodes) then drop a redundant
        // single-child root wrapper, so toggle-tabbed / swapping / render all operate on real tabs.
        self.root.simplify();
        self.collapse_redundant_root_wrapper();

        self.update_tile_sizes(true);

        // Animate existing tiles according to their position changes.
        self.animate_leaves_if_moved(&prev);
    }

    /// Captures on-screen leaf positions keyed by window id (stable across tree reordering).
    fn leaf_positions_by_id(&self) -> Vec<(W::Id, Point<f64, Logical>)> {
        let positions = self.leaf_positions();
        self.root
            .leaves()
            .map(|(t, _)| t.window().id().clone())
            .zip(positions.into_iter().map(|(_, p)| p))
            .collect()
    }

    /// Animates any leaf whose on-screen position changed from `prev` (keyed by window id).
    fn animate_leaves_if_moved(&mut self, prev: &[(W::Id, Point<f64, Logical>)]) {
        let new = self.leaf_positions_by_id();
        for (tile, _) in self.root.leaves_mut() {
            let id = tile.window().id();
            let new_pos = new.iter().find(|(i, _)| i == id).map(|(_, p)| *p);
            let prev_pos = prev.iter().find(|(i, _)| i == id).map(|(_, p)| *p);
            if let (Some(new_pos), Some(prev_pos)) = (new_pos, prev_pos) {
                if new_pos != prev_pos {
                    tile.animate_move_from(prev_pos - new_pos);
                }
            }
        }
    }

    /// Returns an iterator over (tile, data) pairs for the root's children (immutable).
    fn tiles_and_data(&self) -> impl Iterator<Item = (&Tile<W>, &SplitChildData)> {
        match &self.root {
            TileNode::Leaf(_) => panic!("tiles_and_data called on a Leaf root"),
            TileNode::Internal { children, data, .. } => {
                children.iter().zip(data.iter()).filter_map(|(child, data)| {
                    match child {
                        TileNode::Leaf(tile) => Some((tile, data)),
                        // Skip nested splits/tabs — they don't have direct tile references.
                        _ => None,
                    }
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tile(
        tile: Tile<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        width: SectionWidth,
        is_full_width: bool,
    ) -> Self {
        let options = tile.options.clone();

        let display_mode = tile
            .window()
            .rules()
            .default_section_display
            .unwrap_or(options.layout.default_section_display);

        // Try to match width to a preset width. Consider the following case: a terminal (foot)
        // sizes itself to the terminal grid. We open it with default-section-width 0.5. It shrinks
        // by a few pixels to evenly match the terminal grid. Then we press
        // switch-preset-section-width intending to go to proportion 0.667, but the preset width
        // matching code picks the proportion 0.5 preset because it's the next smallest width after
        // the current foot's window width. Effectively, this makes the first
        // switch-preset-section-width press ignored.
        //
        // However, here, we do know that width = proportion 0.5 (regardless of what the window
        // opened with), and we can match it to a preset right away, if one exists.
        let preset_width_idx = options
            .layout
            .preset_section_widths
            .iter()
            .position(|preset| width == SectionWidth::from(*preset));

        let clock = tile.clock.clone();
        let options_clone = options.clone();
        let tab_indicator_config = options.layout.tab_indicator;
        let anim_config = options.animations.window_movement.0;
        let _anim_open_config = options.animations.window_open.anim;
        let hide_when_single_tab = options.layout.tab_indicator.hide_when_single_tab;

        // Create the root as a cross-axis split with one child (the initial tile).
        // This matches the existing section behavior where tiles stack along the cross axis.
        // For tabbed display, we use a Tabbed node instead.
        let tab_header = TabHeader::new_indicator(tab_indicator_config);
        let root = if display_mode == SectionDisplay::Tabbed {
            TileNode::tabbed(vec![tile], 0, tab_header)
        } else {
            TileNode::cross_split(vec![tile], 0)
        };

        let mut rv = Self {
            root,
            width,
            preset_width_idx,
            is_full_width,
            is_pending_maximized: false,
            is_pending_fullscreen: false,
            move_animation: None,
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
            pending_split_direction: None,
        };

        // Update the tile's config and create its data.
        let axis = rv.axis();
        let tile_view_size = rv.map_size_out(view_size);
        rv.tile_mut(0).update_config(tile_view_size, scale, options_clone);
        // Update data for the first child.
        let tile_size = rv.tile(0).tile_size();
        rv.data_mut()[0].size = axis.size_in(tile_size);
        rv.data_mut()[0].interactively_resizing_by_start_edge = rv.tile(0)
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(crate::utils::ResizeEdge::LEFT));

        let pending_sizing_mode = rv.tile(0).window().pending_sizing_mode();

        rv.update_tile_sizes(false);

        match pending_sizing_mode {
            SizingMode::Normal => (),
            SizingMode::Maximized => rv.set_maximized(true),
            SizingMode::Fullscreen => rv.set_fullscreen(true),
        }

        // Animate the tab indicator for new sections.
        if display_mode == SectionDisplay::Tabbed
            && !hide_when_single_tab
            && rv.sizing_mode().is_normal()
        {
            // Usually new sections are created together with window movement actions. For new
            // windows, we handle that in start_open_animation().
            let clock_clone = rv.clock.clone();
            if let Some(tab_indicator) = rv.tab_header_mut() {
                tab_indicator.start_open_animation(clock_clone, anim_config);
            }
        }

        rv
    }

    fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let axis = AxisMap::new(options.layout.main_axis);
        let tile_view_size = axis.size_out(view_size);

        let mut update_sizes = false;

        if self.view_size != view_size
            || self.working_area != working_area
            || self.parent_area != parent_area
        {
            update_sizes = true;
        }

        // If preset widths changed, clear our stored preset index.
        if self.options.layout.preset_section_widths != options.layout.preset_section_widths {
            self.preset_width_idx = None;
        }

        // If preset heights changed, make our heights non-preset.
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.convert_heights_to_auto();
            update_sizes = true;
        }

        if self.options.layout.main_axis != options.layout.main_axis {
            update_sizes = true;
        }

        if self.options.layout.gaps != options.layout.gaps {
            update_sizes = true;
        }

        if self.options.layout.border.off != options.layout.border.off
            || self.options.layout.border.width != options.layout.border.width
        {
            update_sizes = true;
        }

        if self.options.layout.tab_indicator != options.layout.tab_indicator {
            update_sizes = true;
        }

        // Update config for all tiles recursively (including nested splits).
        self.root.update_config_tiles(tile_view_size, scale, options.clone(), axis);

        // Re-derive the Stacked flag: update_config can recreate the header (on a tab-style flip),
        // which would otherwise reset it to a single-row Tabbed.
        let root_stacked = self.root.layout() == Some(Layout::Stacked);
        if let Some(tab_header) = self.tab_header_mut() {
            tab_header.update_config(options.layout.tab_header.clone());
            tab_header.set_stacked(root_stacked);
        }
        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        if update_sizes {
            self.update_tile_sizes(false);
        }
    }

    pub fn update_shaders(&mut self) {
        for (_, tile) in self.tiles_enumerated_mut() {
            tile.update_shaders();
        }

        if let Some(tab_indicator) = self.tab_header_mut() {
            tab_indicator.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let Some(move_) = &mut self.move_animation {
            if move_.anim.is_done() {
                self.move_animation = None;
            }
        }

        for (_, tile) in self.tiles_enumerated_mut() {
            tile.advance_animations();
        }

        if let Some(tab_indicator) = self.tab_header_mut() {
            tab_indicator.advance_animations();
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_header().is_some_and(|ti| ti.are_animations_ongoing())
            || self.tiles_enumerated().any(|(_, tile)| tile.are_animations_ongoing())
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_header().is_some_and(|ti| ti.are_animations_ongoing())
            || self
                .tiles_enumerated()
                .any(|(_, tile)| tile.are_transitions_ongoing())
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        // `tiles_mut()` yields leaves in flat-leaf order, so compare against the active *leaf*
        // index (which follows active_idx down the tree), not the root-level active child index.
        let active_idx = self.root.path_for_leaf_index_from_active().unwrap_or(0);
        for (tile_idx, (tile, tile_off)) in self.tiles_mut().enumerate() {
            let is_active = is_active && tile_idx == active_idx;

            let mut tile_view_rect = view_rect;
            tile_view_rect.loc -= tile_off + tile.render_offset();
            tile.update_render_elements(is_active, tile_view_rect);
        }

        // Only update tab indicator render elements when in tabbed mode.
        // Extract all needed data before the mutable borrow of tab_header_mut().
        let config = self.tab_header().map(|ti| ti.config());
        if let Some(config) = config {
            // One tab per *direct child* of the root (not per leaf): a tab whose child is a nested
            // container reports the union extent of its whole subtree, not one descendant leaf's.
            let active_child_idx = self.root.active_idx();
            let tabs: Vec<_> = self
                .tab_children(&[])
                .into_iter()
                .enumerate()
                .map(|(i, child)| {
                    let tile = self.tile(child.rep_leaf_idx);
                    let is_active = i == active_child_idx;
                    let is_urgent = tile.window().is_urgent();
                    TabInfo::from_tile_with_geometry(
                        tile,
                        child.geometry,
                        is_active,
                        is_urgent,
                        &config,
                    )
                })
                .collect();

            // Hide the tab indicator in fullscreen. If you have it configured to overlap the window,
            // you don't want that to happen in fullscreen. Also, laying things out correctly when the
            // tab indicator is within the section and the section goes fullscreen, would require too
            // many changes to the code for too little benefit (it's mostly invisible anyway).
            let enabled = self.is_tabbed() && self.sizing_mode().is_normal();
            let tab_indicator_area = self.tab_indicator_area();
            // One tab/row per direct child of the root (a Stacked root reserves a row each).
            let tab_count = self.root.child_count();
            let scale = self.scale;

            if let Some(tab_indicator) = self.tab_header_mut() {
                tab_indicator.update_render_elements(
                    enabled,
                    tab_indicator_area,
                    view_rect,
                    tab_count,
                    tabs.into_iter(),
                    is_active,
                    scale,
                );
            }
        }

        // Nested tabbed containers (generalized tabs deeper in the tree).
        let scale = self.scale;
        let sizing_normal = self.sizing_mode().is_normal();
        for unit in self.collect_nested_tabbed() {
            let Some(config) = self
                .root
                .node_at(&unit.path)
                .tab_header()
                .map(|h| h.config())
            else {
                continue;
            };
            let tabs: Vec<TabInfo> = self
                .tab_children(&unit.path)
                .into_iter()
                .enumerate()
                .map(|(i, child)| {
                    let tile = self.tile(child.rep_leaf_idx);
                    let is_tab_active = i == unit.active_idx;
                    let is_urgent = tile.window().is_urgent();
                    TabInfo::from_tile_with_geometry(
                        tile,
                        child.geometry,
                        is_tab_active,
                        is_urgent,
                        &config,
                    )
                })
                .collect();
            let enabled = unit.visible && sizing_normal;
            let header_active = is_active && unit.active_on_path;
            let content_area = unit.content_area;
            let tab_count = unit.tab_count;
            if let Some(header) = self.root.node_at_mut(&unit.path).tab_header_mut() {
                header.update_render_elements(
                    enabled,
                    content_area,
                    view_rect,
                    tab_count,
                    tabs.into_iter(),
                    header_active,
                    scale,
                );
            }
        }
    }

    pub fn is_pending_fullscreen(&self) -> bool {
        self.is_pending_fullscreen
    }

    pub fn is_pending_maximized(&self) -> bool {
        self.is_pending_maximized
    }

    pub fn pending_sizing_mode(&self) -> SizingMode {
        if self.is_pending_fullscreen {
            SizingMode::Fullscreen
        } else if self.is_pending_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    fn axis(&self) -> AxisMap {
        AxisMap::new(self.options.layout.main_axis)
    }

    fn map_size_in(&self, size: Size<f64, Logical>) -> Size<f64, Logical> {
        self.axis().size_in(size)
    }

    fn map_size_out(&self, size: Size<f64, Logical>) -> Size<f64, Logical> {
        self.axis().size_out(size)
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_animation {
            offset.x += move_.from * move_.anim.value();
        }

        offset
    }

    pub fn animate_move_from(&mut self, from_x_offset: f64) {
        self.animate_move_from_with_config(
            from_x_offset,
            self.options.animations.window_movement.0,
        );
    }

    pub fn animate_move_from_with_config(
        &mut self,
        from_x_offset: f64,
        config: niri_config::Animation,
    ) {
        let current_offset = self
            .move_animation
            .as_ref()
            .map_or(0., |move_| move_.from * move_.anim.value());

        let anim = Animation::new(self.clock.clone(), 1., 0., 0., config);
        self.move_animation = Some(MoveAnimation {
            anim,
            from: from_x_offset + current_offset,
        });
    }

    pub fn offset_move_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.anim.value();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    /// Returns whether this section is currently fullscreen.
    ///
    /// As in, if it contains one currently-fullscreen tile, or in tabbed mode, if it contains at
    /// least one currently-fullscreen tile.
    ///
    /// This will lag behind is_pending_fullscreen, depending on when the tiles actually respond to
    /// the un/fullscreen request. But, it's possible for is_fullscreen() to flip instantly, for
    /// example when consuming a fullscreen tile into a non-pending-fullscreen section.
    ///
    /// This controls things like:
    ///
    /// - whether the section draws at the top of the screen or at the start of the working area
    /// - whether the section draws above the top layer-shell layer
    /// - whether the tab indicator is shown
    /// - restoring view_offset_before_fullscreen
    ///
    /// Edge cases to watch out for:
    ///
    /// - Consuming a fullscreen tile into a non-tabbed section will keep that tile fullscreen until
    ///   it responds to the unfullscreen request. This tile may be anywhere in the section,
    ///   including at the active position.
    ///
    /// - Changing a fullscreen tabbed section into normal mode is an easy way to get randomly
    ///   delayed unfullscreening tiles in a normal section.
    ///
    /// - is_fullscreen() can suddenly change when consuming/expelling a fullscreen tile into/from a
    ///   non-fullscreen section. This can influence the code that saves/restores the unfullscreen
    ///   view offset.
    fn sizing_mode(&self) -> SizingMode {
        // Behaviors that we want:
        //
        // 1. The common case: single tile in a section. Assume no animations. Fullscreening the tile
        //    should make it jump to the top-left of the screen only when the tile finishes
        //    fullscreening. Similarly, unfullscreening should keep it at the top-left until the
        //    tile had unfullscreened.
        //
        // 2. Unfullscreening a tabbed section with multiple tiles should restore the view offset
        //    correctly. This means waiting for *all* tiles to unfullscreen, because otherwise the
        //    restored view offset will immediately get overwritten by the still screen-wide section
        //    (it uses the largest tile's width).
        //
        // 3. Changing a fullscreen tabbed section to normal should probably also restore the view
        //    offset correctly. Same problem as above, but now for normal sections (since display
        //    mode change applies instantly).
        //
        // The logic that satisfies these behaviors is to check if *any* tile is fullscreen.
        let mut any_fullscreen = false;
        let mut any_maximized = false;
        for (_, tile) in self.tiles_enumerated() {
            match tile.sizing_mode() {
                SizingMode::Normal => (),
                SizingMode::Maximized => any_maximized = true,
                SizingMode::Fullscreen => any_fullscreen = true,
            }
        }

        if any_fullscreen {
            SizingMode::Fullscreen
        } else if any_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn contains(&self, window: &W::Id) -> bool {
        self.tiles_enumerated()
            .map(|(_, tile)| tile.window())
            .any(|win| win.id() == window)
    }

    pub fn position(&self, window: &W::Id) -> Option<usize> {
        self.tiles_enumerated()
            .find_map(|(idx, tile)| (tile.window().id() == window).then_some(idx))
    }

    /// Activates the leaf at flat-leaf index `idx`, setting `active_idx` at every level along its
    /// path (so it works through nested splits/tabs). Returns whether anything changed.
    fn activate_idx(&mut self, idx: usize) -> bool {
        let Some(path) = self.root.path_for_leaf_index(idx) else {
            return false;
        };
        let changed = self.root.activate_path(&path);
        self.root.leaf_at_mut(&path).ensure_alpha_animates_to_1();
        changed
    }

    fn activate_window(&mut self, window: &W::Id) {
        let idx = self.position(window).unwrap();
        self.activate_idx(idx);
    }

    /// Inserts `tile` as a new top-level row of the section at flat-leaf position `leaf_idx`
    /// (a vertical / cross-axis insertion). If the section root is itself a horizontal Main split
    /// (the whole section is a single row), it is wrapped in a Cross split so the new tile becomes a
    /// row above (`leaf_idx == 0`) or below it. Activates the new tile if `activate`. Returns the
    /// new leaf's flat-leaf index.
    fn add_tile_at(&mut self, leaf_idx: usize, mut tile: Tile<W>, activate: bool) -> usize {
        tile.update_config(
            self.map_size_out(self.view_size),
            self.scale,
            self.options.clone(),
        );

        // Inserting a tile pushes other tiles over; capture their positions (by id) to animate.
        let prev = self.leaf_positions_by_id();

        if !self.is_tabbed() {
            self.is_pending_fullscreen = false;
            self.is_pending_maximized = false;
        }

        let mut new_data = SplitChildData::new_auto();
        new_data.update(&tile, self.axis());

        let new_root_idx;
        if matches!(&self.root, TileNode::Internal { layout: Layout::SplitH, .. }) {
            // The section is a single horizontal row; wrap it in a Cross split so the new tile lands
            // above or below the whole row rather than beside its tiles.
            let above = leaf_idx == 0;
            let old_root = std::mem::replace(
                &mut self.root,
                TileNode::internal(Layout::SplitV, Vec::new(), 0, Vec::new(), None),
            );
            let old_data = SplitChildData::new_auto();
            let (children, data, new_idx) = if above {
                (vec![TileNode::Leaf(tile), old_root], vec![new_data, old_data], 0)
            } else {
                (vec![old_root, TileNode::Leaf(tile)], vec![old_data, new_data], 1)
            };
            self.root = TileNode::internal(
                Layout::SplitV,
                children,
                if activate { new_idx } else { 1 - new_idx },
                data,
                None,
            );
            new_root_idx = new_idx;
        } else {
            // Cross stack / tabbed / lone leaf: insert at the matching root-child boundary (a new
            // row, or a new tab for a tabbed section).
            let root_idx = self.leaf_idx_to_root_child(leaf_idx);
            let old_active = self.root.active_idx();
            self.insert_tile(root_idx, tile);
            let new_active = if activate {
                root_idx
            } else if old_active >= root_idx {
                old_active + 1
            } else {
                old_active
            };
            self.root.set_active_idx(new_active);
            new_root_idx = root_idx;
        }

        if activate {
            self.root
                .leaf_at_mut(&[new_root_idx])
                .ensure_alpha_animates_to_1();
        }

        self.update_tile_sizes(true);
        self.animate_leaves_if_moved(&prev);

        // The new tile is a direct root-child leaf; return its flat-leaf index.
        self.root_child_first_leaf_idx(new_root_idx)
    }

    fn update_window(&mut self, window: &W::Id) {
        let axis = self.axis();

        let tile_idx = self
            .tiles_enumerated()
            .find(|(_, tile)| tile.window().id() == window)
            .map(|(idx, _)| idx)
            .unwrap();

        // Find the path to this leaf for correct data access in nested splits.
        let path = self
            .root
            .path_for_leaf_index(tile_idx)
            .unwrap_or_else(|| panic!("update_window: tile index {tile_idx} out of bounds"));

        // Get the previous height and update the tile's data at the correct tree level.
        let prev_height = self.root.leaf_data(&path).map(|d| d.size.h).unwrap_or(0.);

        self.tile_mut(tile_idx).update_window();
        // Update data for this leaf at the correct tree level.
        let tile_size = axis.size_in(self.tile(tile_idx).tile_size());
        let resizing_by_start = self
            .tile(tile_idx)
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(crate::utils::ResizeEdge::LEFT));
        self.root.update_leaf_data(&path, tile_size, resizing_by_start);

        let new_height = self.root.leaf_data(&path).map(|d| d.size.h).unwrap_or(0.);
        let offset = prev_height - new_height;

        let is_tabbed = self.is_tabbed();

        // Move windows below in tandem with resizing.
        //
        // FIXME: in always-centering mode, window resizing will affect the offsets of all other
        // windows in the section, so they should all be animated. How should this interact with
        // animated vs. non-animated resizes? For example, an animated +20 resize followed by two
        // non-animated -10 resizes.
        if !is_tabbed && offset != 0. {
            let has_resize_anim = self.tile(tile_idx).resize_animation().is_some();
            let resize_anim_config = self.options.animations.window_resize.anim;
            let tiles_len = self.tiles_len();
            if has_resize_anim {
                // If there's a resize animation (that may have just started in
                // tile.update_window()), then the apparent size change is smooth with no sudden
                // jumps. This corresponds to adding a cross-axis animation to tiles below.
                for i in (tile_idx + 1)..tiles_len {
                    self.tile_mut(i).animate_move_y_from_with_config(
                        offset,
                        resize_anim_config,
                    );
                }
            } else {
                // There's no resize animation, but the offset is nonzero. This could happen for
                // example:
                // - if the window resized on its own, which we don't animate
                // - if the window resized by less than 10 px (the resize threshold)
                //
                // The latter case could also cancel an ongoing resize animation.
                //
                // Now, stationary tiles below shouldn't react to this offset change in any way,
                // i.e. their apparent cross-axis position should jump together with the resize.
                // However, tiles below that are already animating a cross-axis movement should
                // offset their animations to avoid the jump.
                //
                // Notably, this is necessary to fix the animation jump when resizing height back
                // and forth in quick succession (in a way that cancels the resize animation).
                for i in (tile_idx + 1)..self.tiles_len() {
                    self.tile_mut(i).offset_move_y_anim_current(offset);
                }
            }
        }
    }

    /// Extra size taken up by elements in the section such as the tab indicator.
    fn extra_size(&self) -> Size<f64, Logical> {
        if self.is_tabbed() {
            // A tabbing header has one tab/row per *direct child* of the root, not per leaf (they
            // differ when the root has a nested child); Stacked reserves a row per tab.
            self.tab_header().unwrap().extra_size(self.root.child_count(), self.scale)
        } else {
            Size::from((0., 0.))
        }
    }

    fn resolve_preset_main_span(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.w, extra.w)
    }

    fn resolve_preset_cross_span(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.h, extra.h)
    }

    fn resolve_section_main_span(&self, width: SectionWidth) -> f64 {
        let working_size = self.working_area.size;
        let gaps = self.options.layout.gaps;
        let extra = self.extra_size();

        match width {
            SectionWidth::Proportion(proportion) => {
                (working_size.w - gaps) * proportion - gaps - extra.w
            }
            SectionWidth::Fixed(width) => width,
        }
    }

    fn tile_main_span_for_window_main_span(&self, tile: &Tile<W>, window_main_span: f64) -> f64 {
        tile.tile_width_for_window_width(window_main_span)
    }

    fn tile_cross_span_for_window_cross_span(&self, tile: &Tile<W>, window_cross_span: f64) -> f64 {
        tile.tile_height_for_window_height(window_cross_span)
    }

    fn window_cross_span_for_tile_cross_span(&self, tile: &Tile<W>, tile_cross_span: f64) -> f64 {
        tile.window_height_for_tile_height(tile_cross_span)
    }

    fn update_tile_sizes(&mut self, animate: bool) {
        self.update_tile_sizes_with_transaction(animate, Transaction::new());
    }

    fn update_tile_sizes_with_transaction(&mut self, animate: bool, transaction: Transaction) {
        let axis = self.axis();
        let sizing_mode = self.pending_sizing_mode();
        if matches!(sizing_mode, SizingMode::Fullscreen | SizingMode::Maximized) {
            // Flat active leaf index: `tiles_enumerated_mut` yields flat indices, so the root-child
            // `active_tile_idx` would pick the wrong leaf in a nested (tabbed) fullscreen section.
            let active_leaf_idx = self.active_leaf_idx();
            let is_tabbed = self.is_tabbed();
            let parent_area_size = axis.size_out(self.parent_area.size);
            for (tile_idx, tile) in self.tiles_enumerated_mut() {
                // In tabbed mode, only the visible window participates in the transaction.
                let is_active = tile_idx == active_leaf_idx;
                let transaction = if is_tabbed && !is_active {
                    None
                } else {
                    Some(transaction.clone())
                };

                if matches!(sizing_mode, SizingMode::Fullscreen) {
                    tile.request_fullscreen(animate, transaction);
                } else {
                    tile.request_maximized(
                        parent_area_size,
                        animate,
                        transaction,
                    );
                }
            }
            return;
        }

        let is_tabbed = self.is_tabbed();

        // The flat cross-split path below has important features (max_non_auto clamping, preset
        // handling, window/tile span conversion) that only apply to a normal vertically-stacked
        // section or a tabbed section. Any main-axis split or any nested structure goes through the
        // recursive path, which distributes space recursively and honors per-child minimums.
        let has_nested = self.root.has_nested_children();
        let root_is_main_split = matches!(&self.root, TileNode::Internal { layout: Layout::SplitH, .. });
        if has_nested || root_is_main_split {
            self.update_tile_sizes_recursive(animate, transaction, axis);
            return;
        }

        let min_size: Vec<_> = self
            .tiles_enumerated()
            .map(|(_, tile)| tile.min_size_nonfullscreen())
            .map(|size| axis.size_in(size))
            .map(|mut size| {
                size.w = size.w.max(1.);
                size.h = size.h.max(1.);
                size
            })
            .collect();
        let max_size: Vec<_> = self
            .tiles_enumerated()
            .map(|(_, tile)| tile.max_size_nonfullscreen())
            .map(|size| axis.size_in(size))
            .collect();

        // Compute the section main-axis span.
        let min_main_span = min_size
            .iter()
            .map(|size| NotNan::new(size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();
        let max_main_span = max_size
            .iter()
            .filter_map(|size| {
                let main_span = size.w;
                if main_span == 0. {
                    None
                } else {
                    Some(NotNan::new(main_span).unwrap())
                }
            })
            .min()
            .map(NotNan::into_inner)
            .unwrap_or(f64::from(i32::MAX));
        let max_main_span = f64::max(max_main_span, min_main_span);

        let desired_width = if self.is_full_width {
            SectionWidth::Proportion(1.)
        } else {
            self.width
        };

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();

        let section_main_span = self.resolve_section_main_span(desired_width);
        let section_main_span = f64::max(f64::min(section_main_span, max_main_span), min_main_span);
        let max_tile_cross_span = working_size.h - self.options.layout.gaps * 2. - extra_size.h;

        // If there are multiple windows in a section, clamp the non-auto window's cross span
        // according to other windows' min spans.
        let mut max_non_auto_window_cross_span = None;
        if self.tiles_len() > 1 && !is_tabbed {
            if let Some(non_auto_idx) = self
                .data()
                .iter()
                .position(|data| !matches!(data.span, ChildSpan::Auto { .. }))
            {
                let min_cross_span_taken = min_size
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != non_auto_idx)
                    .map(|(_, min_size)| min_size.h + self.options.layout.gaps)
                    .sum::<f64>();

                let tile = self.tile(non_auto_idx);
                let cross_span_left = max_tile_cross_span - min_cross_span_taken;
                max_non_auto_window_cross_span = Some(f64::max(
                    1.,
                    self.window_cross_span_for_tile_cross_span(tile, cross_span_left)
                        .round(),
                ));
            }
        }

        // Compute the tile cross spans. Start by converting window cross spans to tile cross spans.
        let mut cross_spans = self
            .tiles_and_data()
            .map(|(tile, data)| match data.span {
                auto @ ChildSpan::Auto { .. } => auto,
                ChildSpan::Fixed(tile_cross_span) => {
                    // `Fixed` stores a *tile* cross span (SplitChildData's contract). Clamp it to the
                    // available cross space: either the shared cap when another window is non-auto
                    // (computed as a window span above, converted here into tile space) or, failing
                    // that, the working-area cross span.
                    let mut tile_cross_span = tile_cross_span.round().max(1.);
                    let cap = if let Some(max_cross_span) = max_non_auto_window_cross_span {
                        self.tile_cross_span_for_window_cross_span(tile, max_cross_span)
                    } else {
                        max_tile_cross_span
                    };
                    tile_cross_span = f64::min(tile_cross_span, cap);

                    ChildSpan::Fixed(tile_cross_span)
                }
                ChildSpan::Preset(idx) => {
                    let preset = self.options.layout.preset_window_heights[idx];
                    let window_cross_span = match self.resolve_preset_cross_span(preset) {
                        ResolvedSize::Tile(cross_span) => {
                            self.window_cross_span_for_tile_cross_span(tile, cross_span)
                        }
                        ResolvedSize::Window(cross_span) => cross_span,
                    };

                    let mut window_cross_span = window_cross_span.round().clamp(1., 100000.);
                    if let Some(max_cross_span) = max_non_auto_window_cross_span {
                        window_cross_span = f64::min(window_cross_span, max_cross_span);
                    }

                    let tile_cross_span =
                        self.tile_cross_span_for_window_cross_span(tile, window_cross_span);
                    ChildSpan::Fixed(tile_cross_span)
                }
            })
            .collect::<Vec<_>>();

        // In tabbed display mode, fill fixed cross spans right away.
        if is_tabbed {
            // All tiles have the same cross span, equal to the cross span of the only fixed tile
            // (if any).
            let tabbed_cross_span = cross_spans
                .iter()
                .find_map(|h| {
                    if let ChildSpan::Fixed(h) = h {
                        Some(*h)
                    } else {
                        None
                    }
                })
                .unwrap_or(max_tile_cross_span);

            // We also take min cross span of all tabs into account.
            let min_cross_span = min_size
                .iter()
                .map(|size| NotNan::new(size.h).unwrap())
                .max()
                .map(NotNan::into_inner)
                .unwrap();
            // But, if there's a larger-than-workspace tab, we don't want to force all tabs to that
            // span.
            let min_cross_span = f64::min(max_tile_cross_span, min_cross_span);
            let tabbed_cross_span = f64::max(tabbed_cross_span, min_cross_span);

            cross_spans.fill(ChildSpan::Fixed(tabbed_cross_span));

            // The following logic will apply individual min/max cross span, etc.
        }

        let gap_span_left = self.options.layout.gaps * (self.tiles_len() + 1) as f64;
        let mut cross_span_left = working_size.h - gap_span_left;
        let mut auto_tiles_left = self.tiles_len();

        // Subtract all fixed cross-span tiles.
        for (h, (min_size, max_size)) in zip(&mut cross_spans, zip(&min_size, &max_size)) {
            // Check if the tile has an exact cross-span constraint.
            if min_size.h == max_size.h {
                *h = ChildSpan::Fixed(min_size.h);
            }

            if let ChildSpan::Fixed(ref mut h) = h {
                if max_size.h > 0. {
                    *h = f64::min(*h, max_size.h);
                }
                *h = f64::max(*h, min_size.h);

                cross_span_left -= *h;
                auto_tiles_left -= 1;
            }
        }

        let mut total_weight: f64 = cross_spans
            .iter()
            .filter_map(|h| {
                if let ChildSpan::Auto { weight } = *h {
                    Some(weight)
                } else {
                    None
                }
            })
            .sum();

        // Iteratively try to distribute the remaining cross span, checking against tile min cross
        // spans. Pick an auto cross span according to the current sizes, then check if it
        // satisfies all remaining minimums. If not, allocate fixed cross span to those tiles and
        // repeat the loop. On each iteration the auto cross span will get smaller.
        //
        // NOTE: we do not respect max cross span here. Doing so would complicate things: if the
        // current auto cross span is above some tile's max span, then the auto cross span can
        // become larger. Combining this with the min cross-span loop is where the complexity
        // appears.
        //
        // However, most max cross-span uses are for fixed-size dialogs, where min cross span ==
        // max cross span. This case is separately handled above.
        'outer: while auto_tiles_left > 0 {
            // Wayland requires us to round the requested size for a window to integer logical
            // pixels, therefore we compute the remaining auto cross span dynamically.
            let mut remaining_cross_span = cross_span_left;
            let mut remaining_weight = total_weight;
            for ((h, tile), min_size) in cross_spans
                .iter_mut()
                .zip(self.tiles_enumerated().map(|(_, t)| t))
                .zip(&min_size)
            {
                let weight = match *h {
                    ChildSpan::Auto { weight } => weight,
                    ChildSpan::Fixed(_) => continue,
                    ChildSpan::Preset(_) => unreachable!(),
                };
                let factor = weight / remaining_weight;

                // Compute the current auto cross span.
                let mut auto_cross_span = remaining_cross_span * factor;

                // Check if the auto cross span satisfies the min cross span.
                if min_size.h > auto_cross_span {
                    auto_cross_span = min_size.h;
                    *h = ChildSpan::Fixed(auto_cross_span);
                    cross_span_left -= auto_cross_span;
                    total_weight -= weight;
                    auto_tiles_left -= 1;

                    // If a min cross span was unsatisfied, then we allocate the tile more than the
                    // auto cross span, which means that the remaining auto tiles now have less
                    // cross span to work with, and the loop must run again.
                    continue 'outer;
                }

                auto_cross_span = self.tile_cross_span_for_window_cross_span(
                    tile,
                    self.window_cross_span_for_tile_cross_span(tile, auto_cross_span)
                        .round()
                        .max(1.),
                );

                remaining_cross_span -= auto_cross_span;
                remaining_weight -= weight;
            }

            // All min cross spans were satisfied, fill them in.
            for (h, tile) in cross_spans
                .iter_mut()
                .zip(self.tiles_enumerated().map(|(_, t)| t))
            {
                let weight = match *h {
                    ChildSpan::Auto { weight } => weight,
                    ChildSpan::Fixed(_) => continue,
                    ChildSpan::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight;

                // Compute the current auto cross span.
                let auto_cross_span = cross_span_left * factor;
                let auto_cross_span = self.tile_cross_span_for_window_cross_span(
                    tile,
                    self.window_cross_span_for_tile_cross_span(tile, auto_cross_span)
                        .round()
                        .max(1.),
                );

                *h = ChildSpan::Fixed(auto_cross_span);
                cross_span_left -= auto_cross_span;
                total_weight -= weight;
                auto_tiles_left -= 1;
            }

            assert_eq!(auto_tiles_left, 0);
        }

        let active_tile_idx = self.active_tile_idx();
        let is_tabbed = self.is_tabbed();
        for ((tile_idx, tile), h) in self.tiles_enumerated_mut().zip(cross_spans) {
            let ChildSpan::Fixed(tile_cross_span) = h else {
                unreachable!()
            };

            let size = axis.size_out(Size::from((section_main_span, tile_cross_span)));

            // In tabbed mode, only the visible window participates in the transaction.
            let is_active = tile_idx == active_tile_idx;
            let transaction = if is_tabbed && !is_active {
                None
            } else {
                Some(transaction.clone())
            };

            tile.request_tile_size(size, animate, transaction);
        }

        // Keep each leaf's cached data in sync with its tile. Unlike the recursive path (which
        // stores intended spans for positioning), the flat path's `data` mirrors the tiles, so
        // refresh it here — otherwise a section that just switched out of the recursive path (e.g.
        // tabbing a Main split) would keep stale sizes.
        self.root.update_data(axis);
    }

    /// Recursive layout for sections with nested splits/tabs.
    ///
    /// Computes the section main span and available size, then delegates to
    /// `root.request_sizes()` which recursively distributes space among all
    /// leaves, handling nested Split and Tabbed nodes.
    fn update_tile_sizes_recursive(
        &mut self,
        animate: bool,
        transaction: Transaction,
        axis: AxisMap,
    ) {
        let working_size = self.working_area.size;
        let gaps = self.options.layout.gaps;

        // Compute the section main-axis span from the root's aggregate min/max.
        let (min_main_span, max_main_span) = self.root.aggregate_min_max_main_span(axis);
        let desired_width = if self.is_full_width {
            SectionWidth::Proportion(1.)
        } else {
            self.width
        };
        let section_main_span = self.resolve_section_main_span(desired_width);
        let section_main_span = f64::max(f64::min(section_main_span, max_main_span), min_main_span);

        // The available size for the root: main span = section width, cross span = working height.
        //
        // We do NOT subtract the root header band here: a tabbing root reserves its own header band
        // internally in `request_sizes` (`child_cross = available.h - extra.h`), and `tiles_origin`
        // separately offsets the content past that band. Subtracting it here too would double-count
        // the band — shrinking the content from the bottom while the offset already shrinks it from
        // the top. For a non-tabbing root the band is zero anyway, so this is a no-op there.
        let available = Size::from((
            section_main_span.max(1.),
            (working_size.h - gaps * 2.).max(1.),
        ));

        self.root
            .request_sizes(available, gaps, axis, self.scale, animate, Some(&transaction));
    }

    fn width(&self) -> f64 {
        let gaps = self.options.layout.gaps;
        // The section's main-axis extent depends on how its root arranges children:
        // a Main split lays children side by side (sum + inter-child gaps), while a Cross
        // split, a Tabbed node, or a lone Leaf all share the main axis (max).
        let mut main_span = match &self.root {
            TileNode::Leaf(tile) => self.axis().size_in(tile.tile_size()).w,
            TileNode::Internal { layout: Layout::SplitH, data, .. } => {
                let sum: f64 = data.iter().map(|d| d.size.w).sum();
                sum + gaps * data.len().saturating_sub(1) as f64
            }
            TileNode::Internal { data, .. } => data
                .iter()
                .map(|data| NotNan::new(data.size.w).unwrap())
                .max()
                .map(NotNan::into_inner)
                .unwrap_or(0.),
        };

        if self.is_tabbed() && self.sizing_mode().is_normal() {
            let extra_size = self
                .tab_header()
                .unwrap()
                .extra_size(self.root.child_count(), self.scale);
            main_span += extra_size.w;
        }

        main_span
    }

    fn focus_index(&mut self, index: u8) {
        let idx = min(usize::from(index.saturating_sub(1)), self.tiles_len() - 1);
        self.activate_idx(idx);
    }

    /// Moves focus to the adjacent leaf along `axis` (i3/sway-style directional focus).
    ///
    /// Walks up from the active leaf to the nearest ancestor that arranges its children along
    /// `axis` (a `Split` of that axis, or a `Tabbed` node for the cross axis) and that has a
    /// sibling in the requested direction (`delta` = -1 / +1). Focus then descends into that
    /// sibling's most-recently-focused leaf. Returns false if no such move exists within this
    /// section (the caller may then fall through to inter-section navigation).
    fn focus_in_axis(&mut self, axis: SplitAxis, delta: isize) -> bool {
        // Resolve the target leaf path using only immutable borrows first.
        let target = {
            let active_path = self.root.active_leaf_path();
            let mut found = None;
            for k in (1..=active_path.len()).rev() {
                let parent_path = &active_path[..k - 1];
                let child_idx = active_path[k - 1];
                let parent = self.root.node_at(parent_path);
                let is_match = match parent {
                    TileNode::Internal { layout, .. } if layout.is_split() => layout.axis() == axis,
                    // A tabbing container navigates along the cross axis (up/down switches tabs),
                    // matching niri's existing tabbed-section navigation.
                    TileNode::Internal { layout, .. } if layout.is_tabbing() => {
                        axis == SplitAxis::Cross
                    }
                    _ => false,
                };
                if is_match {
                    let new_child = child_idx as isize + delta;
                    if new_child >= 0 && (new_child as usize) < parent.child_count() {
                        let mut path = parent_path.to_vec();
                        path.push(new_child as usize);
                        path.extend(self.root.node_at(&path).active_leaf_path());
                        found = Some(path);
                        break;
                    }
                }
            }
            found
        };

        if let Some(path) = target {
            self.root.activate_path(&path);
            self.root.leaf_at_mut(&path).ensure_alpha_animates_to_1();
            true
        } else {
            false
        }
    }

    /// Swaps the active leaf's subtree with its adjacent sibling along `axis` (the move counterpart
    /// of [`focus_in_axis`]). Returns false if there is no sibling in that direction within this
    /// section, so the caller can fall through to inter-section movement.
    /// Swaps the two leaves at the given flat indices within this section by exchanging their tile
    /// payloads, leaving every slot (split structure, `data` sizes, `active_idx`) fixed — so the two
    /// windows trade places but each adopts the other's slot geometry. This matches sway's swap
    /// (containers stay put, occupants exchange) and is uniform whether or not the two leaves share
    /// a parent. No-op for equal/OOB indices.
    fn swap_leaves_by_flat_idx(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        let count = self.root.leaf_count();
        if a >= count || b >= count {
            return;
        }
        let (Some(pa), Some(pb)) = (
            self.root.path_for_leaf_index(a),
            self.root.path_for_leaf_index(b),
        ) else {
            return;
        };

        let prev = self.leaf_positions_by_id();
        self.root.swap_leaf_contents(&pa, &pb);
        self.update_tile_sizes(true);
        self.animate_leaves_if_moved(&prev);
    }

    fn swap_in_axis(&mut self, axis: SplitAxis, delta: isize) -> bool {
        let plan = {
            let active_path = self.root.active_leaf_path();
            let mut found = None;
            for k in (1..=active_path.len()).rev() {
                let parent_path = &active_path[..k - 1];
                let child_idx = active_path[k - 1];
                let parent = self.root.node_at(parent_path);
                let is_match = match parent {
                    TileNode::Internal { layout, .. } if layout.is_split() => layout.axis() == axis,
                    TileNode::Internal { layout, .. } if layout.is_tabbing() => {
                        axis == SplitAxis::Cross
                    }
                    _ => false,
                };
                if is_match {
                    let new_child = child_idx as isize + delta;
                    if new_child >= 0 && (new_child as usize) < parent.child_count() {
                        found = Some((parent_path.to_vec(), child_idx, new_child as usize));
                        break;
                    }
                }
            }
            found
        };

        if let Some((parent_path, a, b)) = plan {
            let prev = self.leaf_positions_by_id();
            let parent = self.root.node_at_mut(&parent_path);
            parent.swap_leaves(a, b);
            // Follow the moved subtree (it is now at index b).
            parent.set_active_idx(b);
            self.root.active_leaf_mut().ensure_alpha_animates_to_1();
            self.update_tile_sizes(true);
            self.animate_leaves_if_moved(&prev);
            true
        } else {
            false
        }
    }

    fn focus_up(&mut self) -> bool {
        self.focus_in_axis(SplitAxis::Cross, -1)
    }

    fn focus_down(&mut self) -> bool {
        self.focus_in_axis(SplitAxis::Cross, 1)
    }

    fn focus_top(&mut self) {
        self.activate_idx(0);
    }

    fn focus_bottom(&mut self) {
        self.activate_idx(self.tiles_len().saturating_sub(1));
    }

    fn move_up(&mut self) -> bool {
        self.swap_in_axis(SplitAxis::Cross, -1)
    }

    fn move_down(&mut self) -> bool {
        self.swap_in_axis(SplitAxis::Cross, 1)
    }

    fn toggle_width(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_leaf_idx());

        let preset_idx = if self.is_full_width || self.is_pending_maximized {
            None
        } else {
            self.preset_width_idx
        };

        let len = self.options.layout.preset_section_widths.len();
        let preset_idx = if let Some(idx) = preset_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let tile = self.tile(tile_idx);
            let current_window = self.map_size_in(tile.window_expected_or_current_size()).w;
            let current_tile = self.map_size_in(tile.tile_expected_or_current_size()).w;

            let mut it = self
                .options
                .layout
                .preset_section_widths
                .iter()
                .map(|preset| self.resolve_preset_main_span(*preset));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        };

        let preset = self.options.layout.preset_section_widths[preset_idx];
        self.set_section_width(SizeChange::from(preset), Some(tile_idx), true);

        self.preset_width_idx = Some(preset_idx);
    }

    fn toggle_full_width(&mut self) {
        if self.is_pending_maximized {
            // Treat it as unmaximize.
            self.is_pending_maximized = false;
            self.is_full_width = false;
        } else {
            self.is_full_width = !self.is_full_width;
        }

        self.update_tile_sizes(true);
    }

    fn set_section_width(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let current_width = if self.is_full_width || self.is_pending_maximized {
            SectionWidth::Proportion(1.)
        } else {
            self.width
        };

        let current_main_span = self.resolve_section_main_span(current_width);

        // FIXME: fix overflows then remove limits.
        const MAX_MAIN_SPAN: f64 = 100000.;
        const MAX_PROPORTION: f64 = 10000.;

        let new_width = match (current_width, change) {
            (_, SizeChange::SetFixed(fixed)) => {
                // As a special case, setting a fixed section width will compute it in such a way
                // that the specified (usually active) window gets that width. This is the
                // intention behind the ability to set a fixed size.
                let tile_idx = tile_idx.unwrap_or(self.active_leaf_idx());
                let tile = self.tile(tile_idx);
                SectionWidth::Fixed(
                    self.tile_main_span_for_window_main_span(tile, f64::from(fixed))
                        .clamp(1., MAX_MAIN_SPAN),
                )
            }
            (_, SizeChange::SetProportion(proportion)) => {
                SectionWidth::Proportion((proportion / 100.).clamp(0., MAX_PROPORTION))
            }
            (_, SizeChange::AdjustFixed(delta)) => {
                let new_main_span = (current_main_span + f64::from(delta)).clamp(1., MAX_MAIN_SPAN);
                SectionWidth::Fixed(new_main_span)
            }
            (SectionWidth::Proportion(current_proportion), SizeChange::AdjustProportion(delta)) => {
                let new_proportion = (current_proportion + delta / 100.).clamp(0., MAX_PROPORTION);
                SectionWidth::Proportion(new_proportion)
            }
            (SectionWidth::Fixed(_), SizeChange::AdjustProportion(delta)) => {
                let available_main_span = self.working_area.size.w - self.options.layout.gaps;
                let current_proportion = if available_main_span == 0. {
                    1.
                } else {
                    (current_main_span + self.options.layout.gaps + self.extra_size().w)
                        / available_main_span
                };
                let new_proportion = (current_proportion + delta / 100.).clamp(0., MAX_PROPORTION);
                SectionWidth::Proportion(new_proportion)
            }
        };

        self.width = new_width;
        self.preset_width_idx = None;
        self.is_full_width = false;
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn set_window_height(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_leaf_idx());

        // Use path-based data access for nested split support.
        let path = self
            .root
            .path_for_leaf_index(tile_idx)
            .unwrap_or_else(|| panic!("set_window_height: tile index {tile_idx} out of bounds"));

        // A "height" is a span along the cross axis, so resize the child of the nearest
        // cross-arranging ancestor (Cross split or Tabbed node) that contains this leaf — not the
        // leaf's own span, which for a Main-split parent would be a *width*. If there is no such
        // ancestor (e.g. a bare child of a Main split), a vertical resize is meaningless: the tile
        // already fills the section's cross extent.
        let path = {
            let mut target_len = None;
            for k in 0..path.len() {
                if matches!(
                    self.root.node_at(&path[..k]),
                    TileNode::Internal { layout, .. }
                        if *layout == Layout::SplitV || layout.is_tabbing()
                ) {
                    target_len = Some(k + 1);
                }
            }
            match target_len {
                Some(len) => path[..len].to_vec(),
                None => return,
            }
        };

        // Start by converting all heights to automatic, since only one window in the section can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        let is_auto = self.root.leaf_data(&path).map(|d| matches!(d.span, ChildSpan::Auto { .. })).unwrap_or(false);
        if is_auto {
            self.convert_heights_to_auto();
        }

        let current_height = self.root.leaf_data(&path).map(|d| d.span).unwrap_or(ChildSpan::Auto { weight: 1. });
        let tile = self.tile(tile_idx);
        // `ChildSpan::Fixed` stores a *tile* cross span (SplitChildData's contract), so recover the
        // window span from it rather than treating the stored value as a window span.
        let (current_window_cross_span, current_tile_cross_span) = match current_height {
            ChildSpan::Auto { .. } | ChildSpan::Preset(_) => {
                let w = self.map_size_in(tile.window_size()).h;
                (w, self.tile_cross_span_for_window_cross_span(tile, w))
            }
            ChildSpan::Fixed(tile_cross_span) => (
                self.window_cross_span_for_tile_cross_span(tile, tile_cross_span),
                tile_cross_span,
            ),
        };

        let work_area_cross_span = self.working_area.size.h;
        let gaps = self.options.layout.gaps;
        let extra_cross_span = self.extra_size().h;
        let available_cross_span = work_area_cross_span - gaps;
        let current_proportion = if available_cross_span == 0. {
            1.
        } else {
            (current_tile_cross_span + gaps) / available_cross_span
        };

        // FIXME: fix overflows then remove limits.
        const MAX_CROSS_SPAN: f64 = 100000.;

        let mut new_window_cross_span = match change {
            SizeChange::SetFixed(fixed) => f64::from(fixed),
            SizeChange::SetProportion(proportion) => {
                let tile_cross_span =
                    (work_area_cross_span - gaps) * (proportion / 100.) - gaps - extra_cross_span;
                self.window_cross_span_for_tile_cross_span(tile, tile_cross_span)
            }
            SizeChange::AdjustFixed(delta) => current_window_cross_span + f64::from(delta),
            SizeChange::AdjustProportion(delta) => {
                let new_proportion = current_proportion + delta / 100.;
                let tile_cross_span =
                    (work_area_cross_span - gaps) * new_proportion - gaps - extra_cross_span;
                self.window_cross_span_for_tile_cross_span(tile, tile_cross_span)
            }
        };

        // Clamp the height according to the min sizes of the windows that actually compete with the
        // resized slot for cross-axis space — its siblings within the nearest cross-arranging
        // ancestor, which is the same node the resize routes to (`path`'s parent). Summing every leaf
        // in the section flat over-restricts nested trees, since horizontal siblings in other
        // subtrees don't share this cross axis at all.
        let min_cross_span_taken = if self.is_tabbed() {
            0.
        } else {
            let (child_idx, parent_path) = path.split_last().unwrap();
            match self.root.node_at(parent_path) {
                // A tabbing ancestor overlaps its children (one shown at a time), so they don't
                // compete for cross-axis space.
                TileNode::Internal { layout, .. } if layout.is_tabbing() => 0.,
                TileNode::Internal { children, .. } => children
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| i != child_idx)
                    .map(|(_, c)| f64::max(1., c.min_cross_span_subtree(self.axis())) + gaps)
                    .sum::<f64>(),
                TileNode::Leaf(_) => 0.,
            }
        };
        let cross_span_left =
            work_area_cross_span - extra_cross_span - gaps - min_cross_span_taken - gaps;
        let cross_span_left = f64::max(
            1.,
            self.window_cross_span_for_tile_cross_span(tile, cross_span_left),
        );
        new_window_cross_span = f64::min(cross_span_left, new_window_cross_span);

        // Clamp it against the window height constraints.
        let win = self.tile(tile_idx).window();
        let min_h = win.min_size().h;
        let max_h = win.max_size().h;

        if max_h > 0 {
            new_window_cross_span = f64::min(new_window_cross_span, f64::from(max_h));
        }
        if min_h > 0 {
            new_window_cross_span = f64::max(new_window_cross_span, f64::from(min_h));
        }

        // Store a *tile* cross span (SplitChildData's contract), converting the resolved window span
        // here at the storage site where this leaf's decoration delta is known. The recursive path
        // (`request_sizes`) and the flat path both consume `Fixed` as a tile span, so a nested
        // (recursive-path) section now converges to the same window size as a flat one.
        let new_window_cross_span = new_window_cross_span.clamp(1., MAX_CROSS_SPAN);
        let new_tile_cross_span =
            self.tile_cross_span_for_window_cross_span(self.tile(tile_idx), new_window_cross_span);
        self.root.update_leaf_data(
            &path,
            self.tile(tile_idx).tile_size(),
            false, // resizing_by_start not relevant here
        );
        // Update the span specifically.
        self.root.update_leaf_span(&path, ChildSpan::Fixed(new_tile_cross_span));
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn reset_window_height(&mut self, tile_idx: Option<usize>) {
        if self.is_tabbed() {
            // When tabbed, reset window height should work on any window, not just the fixed-size
            // one.
            for data in self.data_mut() {
                data.span = ChildSpan::auto_1();
            }
        } else {
            let tile_idx = tile_idx.unwrap_or(self.active_leaf_idx());
            let path = self.root.path_for_leaf_index(tile_idx)
                .unwrap_or_else(|| panic!("convert_heights_to_auto: tile index {tile_idx} out of bounds"));
            self.root.update_leaf_span(&path, ChildSpan::auto_1());
        }

        self.update_tile_sizes(true);
    }

    fn toggle_window_height(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_leaf_idx());

        // Start by converting all heights to automatic, since only one window in the section can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        // Use path-based data access for nested split support.
        let path = self
            .root
            .path_for_leaf_index(tile_idx)
            .unwrap_or_else(|| panic!("toggle_height: tile index {tile_idx} out of bounds"));

        if matches!(self.root.leaf_data(&path).map(|d| d.span), Some(ChildSpan::Auto { .. })) {
            self.convert_heights_to_auto();
        }

        let len = self.options.layout.preset_window_heights.len();
        let preset_idx = match self.root.leaf_data(&path).map(|d| d.span) {
            Some(ChildSpan::Preset(idx)) if !self.is_pending_maximized => {
                (idx + if forwards { 1 } else { len - 1 }) % len
            }
            _ => {
                let current_tile_cross_span = self.root.leaf_data(&path).map(|d| d.size.h).unwrap_or(0.);
                let tile = self.tile(tile_idx);

                let mut it = self
                    .options
                    .layout
                    .preset_window_heights
                    .iter()
                    .copied()
                    .map(|preset| {
                        let window_cross_span = match self.resolve_preset_cross_span(preset) {
                            ResolvedSize::Tile(h) => {
                                self.window_cross_span_for_tile_cross_span(tile, h)
                            }
                            ResolvedSize::Window(h) => h,
                        };
                        self.tile_cross_span_for_window_cross_span(
                            tile,
                            window_cross_span.round().clamp(1., 100000.),
                        )
                    });

                if forwards {
                    it.position(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        current_tile_cross_span + 1. < resolved
                    })
                    .unwrap_or(0)
                } else {
                    it.rposition(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        resolved + 1. < current_tile_cross_span
                    })
                    .unwrap_or(len - 1)
                }
            }
        };
        let path = self.root.path_for_leaf_index(tile_idx)
            .unwrap_or_else(|| panic!("toggle_height preset: tile index {tile_idx} out of bounds"));
        self.root.update_leaf_span(&path, ChildSpan::Preset(preset_idx));
        self.is_pending_maximized = false;
        self.update_tile_sizes(true);
    }

    /// Converts all heights in the section to automatic, preserving the apparent heights.
    ///
    /// All weights are recomputed to preserve the current tile heights while "centering" the
    /// weights at the median window height (it gets weight = 1).
    ///
    /// One case where apparent heights will not be preserved is when the section is taller than the
    /// working area.
    fn convert_heights_to_auto(&mut self) {
        let heights: Vec<_> = self.data().iter().map(|data| data.size.h).collect();

        // Weights are invariant to multiplication: a section with weights 2, 2, 1 is equivalent to
        // a section with weights 4, 4, 2. So we find the median window height and use that as 1.
        let mut sorted = heights.clone();
        // NaN heights should never reach here, but a `partial_cmp().unwrap()` would panic if one did;
        // treat them as equal rather than crash.
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];

        for (data, height) in self.data_mut().iter_mut().zip(heights) {
            // A freshly-wrapped subtree slot starts at zero size before its first relayout, so the
            // median (or an individual height) can be zero — `0. / 0.` is NaN, and a NaN weight
            // cached here can later panic a `partial_cmp().unwrap()` elsewhere. Fall back to a
            // neutral weight in any degenerate/non-finite case.
            let weight = if median > 0. && height.is_finite() {
                height / median
            } else {
                1.
            };
            let weight = if weight.is_finite() { weight } else { 1. };
            data.span = ChildSpan::Auto { weight };
        }
    }

    fn set_fullscreen(&mut self, is_fullscreen: bool) {
        if self.is_pending_fullscreen == is_fullscreen {
            return;
        }

        if is_fullscreen {
            assert!(self.tiles_len() == 1 || self.is_tabbed());
        }

        self.is_pending_fullscreen = is_fullscreen;
        self.update_tile_sizes(true);
    }

    fn set_maximized(&mut self, maximize: bool) {
        if self.is_pending_maximized == maximize {
            return;
        }

        if maximize {
            assert!(self.tiles_len() == 1 || self.is_tabbed());
        }

        self.is_pending_maximized = maximize;
        self.update_tile_sizes(true);
    }

    fn set_section_display(&mut self, display: SectionDisplay) {
        if self.display_mode() == display {
            return;
        }

        // Animate the movement.
        //
        // We're doing some shortcuts here because we know that currently normal vs. tabbed can
        // only cause a cross-axis shift + a shift to the origin.
        //
        // Doing it this way to avoid storing all tile positions in a vector. If more display modes
        // are added it might be simpler to just collect everything into a smallvec.
        let prev_origin = self.tiles_origin();
        self.set_display_mode(display);
        let new_origin = self.tiles_origin();
        let origin_delta = prev_origin - new_origin;

        // Determine which leaves are hidden in tabbed mode (everything outside the active tab's
        // subtree). These are exactly the tiles whose opacity changes on the transition.
        self.set_display_mode(SectionDisplay::Tabbed);
        let hidden_in_tabbed: Vec<bool> =
            self.root.leaf_visibility().into_iter().map(|v| !v).collect();

        // We need to walk the tiles in the normal display mode to get the right offsets.
        self.set_display_mode(SectionDisplay::Normal);
        let anim_config = self.options.animations.window_movement.0;
        for (tile, pos) in self.tiles_mut() {
            let mut cross_delta = pos.y - prev_origin.y;

            // Invert the cross-axis motion when transitioning *to* normal display mode.
            if display == SectionDisplay::Normal {
                cross_delta *= -1.;
            }

            let delta = origin_delta + cross_space_vec(cross_delta);
            tile.animate_move_from(delta);
        }

        // Animate the opacity: tabs outside the active subtree fade out when entering tabbed mode,
        // and fade back in when leaving it.
        for ((_, tile), &hidden) in self.tiles_enumerated_mut().zip(&hidden_in_tabbed) {
            if hidden {
                let (from, to) = if display == SectionDisplay::Tabbed {
                    (1., 0.)
                } else {
                    (0., 1.)
                };
                tile.animate_alpha(from, to, anim_config);
            }
        }

        // Now switch the display mode for real.
        self.set_display_mode(display);

        // Un-tabbing the root can expose a same-family split directly inside another (e.g.
        // Stacked[a, V[b,c]] → SplitV[a, V[b,c]]); merge it away to keep the tree canonical. The
        // animation passes above already captured leaf order/positions, and simplify preserves
        // depth-first leaf order, so doing this after them is safe.
        self.root.simplify();
        self.collapse_redundant_root_wrapper();

        // Animate the appearance of the tab indicator.
        if display == SectionDisplay::Tabbed {
            let clock = self.clock.clone();
            if let Some(tab_indicator) = self.tab_header_mut() {
                tab_indicator.start_open_animation(clock, anim_config);
            }
        }

        self.update_tile_sizes(true);
    }

    fn tiles_origin(&self) -> Point<f64, Logical> {
        let mut origin = Point::from((0., 0.));

        match self.sizing_mode() {
            SizingMode::Normal => (),
            SizingMode::Maximized => {
                origin += cross_space_vec(self.parent_area.loc.y);
                return origin;
            }
            SizingMode::Fullscreen => return origin,
        }

        origin += cross_space_vec(self.working_area.loc.y + self.options.layout.gaps);

        if self.is_tabbed() {
            origin += self
                .tab_header()
                .unwrap()
                .content_offset(self.root.child_count(), self.scale);
        }

        origin
    }

    /// The single source of truth for on-screen leaf positions.
    ///
    /// Recursive tree geometry (from `TileNode::leaf_layout`) with main-axis centering and
    /// interactive start-edge resize shift applied at the section level. Returns one
    /// `(tile_ptr, position)` per leaf, in tree (flat-leaf) order. All rendering, hit-testing and
    /// animation goes through this so they can never disagree.
    fn leaf_positions(&self) -> Vec<(*const Tile<W>, Point<f64, Logical>)> {
        let origin = self.tiles_origin();
        let raw = self.root.leaf_layout(origin, self.options.layout.gaps, self.scale);

        // Centering / start-edge shift only applies to leaves that share the section's main-axis
        // origin (no Main-split ancestor); the max main span is taken over just those leaves.
        let center = self.options.layout.center_focused_section == CenterFocusedSection::Always;
        let max_main = raw
            .iter()
            .filter(|l| l.aligned)
            .map(|l| l.main_size)
            .fold(0., f64::max);

        raw.into_iter()
            .map(|l| {
                let mut pos = l.pos;
                if l.aligned {
                    if center {
                        pos.x = origin.x + (max_main - l.main_size) / 2.;
                    } else if l.resizing_by_start {
                        pos.x = origin.x + max_main - l.main_size;
                    }
                }
                (l.tile, pos)
            })
            .collect()
    }

    fn tile_offsets(&self) -> impl Iterator<Item = Point<f64, Logical>> + '_ {
        self.leaf_positions().into_iter().map(|(_, pos)| pos)
    }

    fn tile_offset(&self, tile_idx: usize) -> Point<f64, Logical> {
        let offsets = self.leaf_positions();
        if let Some((_, pos)) = offsets.get(tile_idx) {
            return *pos;
        }
        // Handle the "one past the end" case: the position just below the last tile (used by the
        // insert hint for "below the section" and by remove for gap deltas). Include the last tile's
        // cross size so it lands at the section's bottom, not back at the last tile's top.
        if tile_idx == offsets.len() && !offsets.is_empty() {
            let last = offsets.len() - 1;
            let (_, last_pos) = offsets[last];
            let last_h = self
                .root
                .path_for_leaf_index(last)
                .and_then(|p| self.root.leaf_data(&p))
                .map_or(0., |d| d.size.h);
            return Point::from((last_pos.x, last_pos.y + last_h + self.options.layout.gaps));
        }
        panic!("tile_offset: index {tile_idx} out of bounds (leaves: {})", offsets.len())
    }

    pub fn tiles(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let positions = self.leaf_positions();
        self.root
            .leaves()
            .map(|(t, _)| t)
            .zip(positions.into_iter().map(|(_, pos)| pos))
    }

    fn tiles_mut(&mut self) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let positions: Vec<_> = self.leaf_positions().into_iter().map(|(_, pos)| pos).collect();
        self.root.leaves_mut().map(|(t, _)| t).zip(positions)
    }

    /// Builds the active-first render order over leaf indices.
    fn render_order(&self, n: usize) -> (usize, Vec<usize>) {
        let active = self.root.path_for_leaf_index_from_active().unwrap_or(0);
        let mut order = Vec::with_capacity(n);
        if active < n {
            order.push(active);
        }
        for i in 0..n {
            if i != active {
                order.push(i);
            }
        }
        (active, order)
    }

    fn tiles_in_render_order(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        // A leaf is visible unless a Tabbed ancestor on its path hides it (only that tab's active
        // child shows). This correctly reveals *all* leaves of a split that is itself a tab.
        let visibility = self.root.leaf_visibility();
        let positions = self.leaf_positions();
        let (_active, order) = self.render_order(positions.len());
        let leaves: Vec<&Tile<W>> = self.root.leaves().map(|(t, _)| t).collect();

        order.into_iter().map(move |idx| {
            (leaves[idx], positions[idx].1, visibility[idx])
        })
    }

    fn tiles_in_render_order_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let positions: Vec<_> = self.leaf_positions().into_iter().map(|(_, pos)| pos).collect();
        let (_active, order) = self.render_order(positions.len());

        let leaf_ptrs: Vec<*mut Tile<W>> =
            self.root.leaves_mut().map(|(t, _)| t as *mut _).collect();

        order.into_iter().map(move |idx| {
            // SAFETY: `order` is a permutation of distinct leaf indices, so each pointer is
            // dereferenced exactly once and they never alias. All were derived from &mut self.root.
            let ptr = leaf_ptrs[idx];
            let tile = unsafe { &mut *ptr };
            (tile, positions[idx])
        })
    }

    /// Per-tab render info for a tabbing container, computed per *direct child* (one tab each).
    ///
    /// Each tab's `geometry` is the union (bounding box) of every leaf under that child subtree, so
    /// a tab over a multi-window group reports the group's full extent rather than a single
    /// descendant leaf's size. `title` is the window title for a leaf child, or a synthesized group
    /// label (e.g. `H[2]`) for a nested container child. `rep_leaf_idx` is the flat index of the
    /// child subtree's active leaf (used for the gradient/urgency source).
    fn tab_children(&self, path: &[usize]) -> Vec<TabChild> {
        let node = self.root.node_at(path);
        let count = node.child_count();

        let positions = self.leaf_positions();
        let leaf_paths: Vec<TilePath> = self.root.leaves().map(|(_, p)| p).collect();
        let flat_of = |target: &[usize]| -> usize {
            leaf_paths.iter().position(|p| p.as_slice() == target).unwrap_or(0)
        };

        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let mut child_path = path.to_vec();
            child_path.push(i);
            let child = self.root.node_at(&child_path);

            // Union bounding box of every leaf under this child (section-local coords).
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for (idx, lp) in leaf_paths.iter().enumerate() {
                if !lp.starts_with(&child_path) {
                    continue;
                }
                let pos = positions[idx].1;
                let sz = self.root.leaf_data(lp).map(|d| d.size).unwrap_or_default();
                x0 = x0.min(pos.x);
                y0 = y0.min(pos.y);
                x1 = x1.max(pos.x + sz.w);
                y1 = y1.max(pos.y + sz.h);
            }
            let geometry = if x0 > x1 || y0 > y1 {
                Rectangle::default()
            } else {
                Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)))
            };

            // Representative leaf = the child subtree's active leaf.
            let mut rep_path = child_path.clone();
            rep_path.extend(child.active_path());
            let rep_leaf_idx = flat_of(&rep_path);

            // Title: the window title for a leaf, or the sway/i3-style tree representation
            // (`H[a b]`, `V[a H[b c]]`, …) for a nested container.
            let title = child.tree_repr();

            out.push(TabChild { rep_leaf_idx, geometry, title });
        }
        out
    }

    /// Collects render data for every *nested* (non-root) tabbed container in this section. The
    /// root section header is handled by the existing dedicated path; this generalizes headers to
    /// tabbed nodes deeper in the tree (e.g. a tabbed row). Returns an empty vec for the common
    /// case of no nested tabs, doing only a cheap tree walk.
    fn collect_nested_tabbed(&self) -> Vec<NestedTabbed> {
        let mut paths = Vec::new();
        let mut prefix = Vec::new();
        self.root.collect_tabbed_paths(&mut prefix, &mut paths);
        paths.retain(|p| !p.is_empty());
        if paths.is_empty() {
            return Vec::new();
        }

        let visibility = self.root.leaf_visibility();
        let positions = self.leaf_positions();
        let leaf_paths: Vec<TilePath> = self.root.leaves().map(|(_, p)| p).collect();
        let active_path = self.root.active_path();

        let flat_of = |target: &[usize]| -> usize {
            leaf_paths.iter().position(|p| p.as_slice() == target).unwrap_or(0)
        };

        let mut out = Vec::new();
        for path in paths {
            let node = self.root.node_at(&path);
            let tab_count = node.child_count();
            let active_idx = node.active_idx();

            // Content area = bounding box of every leaf under the node (section-local). All tabs are
            // sized to the same content rectangle, so this is exactly that rectangle; the header
            // draws in the band just above it.
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for (idx, lp) in leaf_paths.iter().enumerate() {
                if !lp.starts_with(&path) {
                    continue;
                }
                let pos = positions[idx].1;
                let sz = self.root.leaf_data(lp).map(|d| d.size).unwrap_or_default();
                x0 = x0.min(pos.x);
                y0 = y0.min(pos.y);
                x1 = x1.max(pos.x + sz.w);
                y1 = y1.max(pos.y + sz.h);
            }
            if x0 > x1 || y0 > y1 {
                continue;
            }
            let content_area =
                Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)));

            // Representative leaf (flat index) of each tab = the active leaf of that child subtree.
            let mut rep_leaf_idx = Vec::with_capacity(tab_count);
            for i in 0..tab_count {
                let mut child_path = path.clone();
                child_path.push(i);
                let child = self.root.node_at(&child_path);
                child_path.extend(child.active_path());
                rep_leaf_idx.push(flat_of(&child_path));
            }

            // The node is visible if its own active leaf is visible (not hidden by an ancestor tab).
            let mut node_active = path.clone();
            node_active.extend(node.active_path());
            let visible = visibility.get(flat_of(&node_active)).copied().unwrap_or(true);
            let active_on_path = active_path.starts_with(&path);

            out.push(NestedTabbed {
                path,
                content_area,
                tab_count,
                active_idx,
                visible,
                active_on_path,
                rep_leaf_idx,
            });
        }
        out
    }

    fn tab_indicator_area(&self) -> Rectangle<f64, Logical> {
        // We'd like to use the active tile's animated size for the tab indicator, however we need
        // to be mindful of the case where the active tile is smaller than some other tile in the
        // section. The section assumes the size of the largest tile.
        //
        // We expect users to mainly resize tabbed sections by their main-axis span, so matching the
        // animated size is more important here. Besides, we always try to resize all windows in a
        // section to the same main-axis span when possible, and also the animation for going into
        // tabbed mode doesn't move tiles along the main axis as much.
        //
        // For cross span though, it's a different story. First, users probably aren't resizing a
        // tabbed section by cross span. Second, we don't match windows by cross span, so it's easy
        // to have a smaller active tile than the rest of the section, e.g. by adding a fixed-size
        // dialog. Then, switching to that dialog and back should ideally keep the tab indicator
        // position fixed. Third, the animation for making a section tabbed moves tiles along the
        // cross axis, and using the active tile's animated size in this case only works for the
        // topmost tile, and looks broken otherwise.
        let mut max_tile_cross_span = 0.;
        let mut max_tile_main_span = 0.;
        for data in self.data() {
            max_tile_cross_span = f64::max(max_tile_cross_span, data.size.h);
            max_tile_main_span = f64::max(max_tile_main_span, data.size.w);
        }

        // The header band spans the section's content width. When a direct child is a Main split
        // (e.g. a tabbed/stacked container holding a horizontal pair), its leaves subdivide the
        // width, so the active *leaf* is narrower than the section; use the child's full extent
        // (`data.size.w`, which is the per-tab content width) so the header spans the whole group,
        // not one descendant. Fall back to the active tile's animated width when wider (mid-resize
        // animation), preserving the smooth main-axis resize the old code aimed for.
        let tile = self.active_tile();
        let active_size = self.map_size_in(tile.animated_tile_size());
        let main_span = f64::max(active_size.w, max_tile_main_span);
        let indicator_size = Size::from((main_span, max_tile_cross_span));

        Rectangle::new(self.tiles_origin(), indicator_size)
    }

    /// If `local` (section-local logical coords, same space as `leaf_positions`) is over the header
    /// band that a tabbing container reserves at its edge, returns the flat-leaf index of a
    /// representative leaf in that container — dropping there adds the dragged window as a new tab.
    ///
    /// The band is `extra_size` worth of space at the container's header side: the full container
    /// rect (content + header) minus the content rect. This reuses the exact geometry the renderer
    /// and the tab-switching hit-test use (`tab_indicator_area`/`collect_nested_tabbed` for the
    /// content rect, `TabHeader::extra_size`/`content_offset` for the reserved band), so the drop
    /// region lines up with the drawn header. Handles the root tabbed section and every nested
    /// Tabbed/Stacked node; the innermost (deepest) matching container wins.
    fn header_band_target(&self, local: Point<f64, Logical>) -> Option<usize> {
        let scale = self.scale;

        // A container's header band = full rect (content shifted back by content_offset, grown by
        // extra_size) minus the content rect. Returns whether `local` is in that band.
        let in_band = |content: Rectangle<f64, Logical>,
                       header: &TabHeader,
                       tab_count: usize|
         -> bool {
            let extra = header.extra_size(tab_count, scale);
            if extra.w <= 0. && extra.h <= 0. {
                return false;
            }
            let offset = header.content_offset(tab_count, scale);
            let full = Rectangle::new(content.loc - offset, content.size + extra);
            full.contains(local) && !content.contains(local)
        };

        let mut best: Option<(usize, usize)> = None; // (path depth, target leaf)

        // Root tabbed section.
        if self.is_tabbed() {
            if let Some(header) = self.tab_header() {
                if in_band(self.tab_indicator_area(), header, self.root.child_count()) {
                    best = Some((0, self.active_leaf_idx()));
                }
            }
        }

        // Nested tabbing containers (a tabbed row, a stacked column, etc.).
        for unit in self.collect_nested_tabbed() {
            if !unit.visible {
                continue;
            }
            let Some(header) = self.root.node_at(&unit.path).tab_header() else {
                continue;
            };
            if !in_band(unit.content_area, header, unit.tab_count) {
                continue;
            }
            let target = unit
                .rep_leaf_idx
                .get(unit.active_idx)
                .or_else(|| unit.rep_leaf_idx.first())
                .copied()
                .unwrap_or(0);
            let depth = unit.path.len();
            if best.is_none_or(|(d, _)| depth >= d) {
                best = Some((depth, target));
            }
        }

        best.map(|(_, leaf)| leaf)
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        let is_tabbed = self.is_tabbed();
        let sizing_normal = self.sizing_mode().is_normal();
        let tiles_len = self.tiles_len();
        let hide_when_single_tab = self.tab_header().is_none_or(|ti| ti.config().hide_when_single_tab);
        let clock = self.clock.clone();
        let open_anim = self.options.animations.window_open.anim;

        // Find the tile index first, then do the work outside the iterator borrow.
        let found_idx = self.tiles_enumerated().find(|(_, tile)| tile.window().id() == id).map(|(idx, _)| idx);

        if let Some(idx) = found_idx {
            self.tile_mut(idx).start_open_animation();

            // Animate the appearance of the tab indicator.
            if is_tabbed
                && sizing_normal
                && tiles_len == 1
                && !hide_when_single_tab
            {
                self.tab_header_mut().unwrap().start_open_animation(
                    clock,
                    open_anim,
                );
            }

            return true;
        }

        false
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        assert!(self.tiles_len() != 0, "sections can't be empty");
        assert!(self.active_tile_idx() < self.root.child_count());
        // data().len() matches the root's direct child count, not the recursive leaf count.
        assert_eq!(self.root.child_count(), self.data().len());
        // Recursively validate the whole tree (lengths, active_idx, no empty/redundant nodes).
        self.root.verify_structure();

        if !self.pending_sizing_mode().is_normal() {
            assert!(self.root.child_count() == 1 || self.is_tabbed());
        }

        if let Some(idx) = self.preset_width_idx {
            assert!(idx < self.options.layout.preset_section_widths.len());
        }

        let is_tabbed = self.is_tabbed();

        let child_count = self.root.child_count();
        if child_count == 1 {
            if let ChildSpan::Auto { weight } = self.data()[0].span {
                assert_eq!(
                    weight, 1.,
                    "auto height weight must reset to 1 for a single window"
                );
            }
        }

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();
        let gaps = self.options.layout.gaps;

        // The cross-axis sum invariant only applies to Cross-axis splits (where tiles
        // stack along the cross axis). For Main-axis splits, tiles are side-by-side
        // and each gets the full cross span, so the sum is not constrained.
        let is_main_split = matches!(&self.root, TileNode::Internal { layout: Layout::SplitH, .. });

        let mut found_fixed = false;
        let mut total_height = 0.;
        let mut total_min_height = 0.;
        let has_nested = self.root.has_nested_children();

        for (tile, data) in self.tiles_and_data() {
            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            assert_eq!(
                self.pending_sizing_mode(),
                tile.window().pending_sizing_mode()
            );
            assert_eq!(self.map_size_out(self.view_size), tile.view_size());
            tile.verify_invariants();

            // Skip the data consistency check for sections laid out by the recursive path
            // (any nested structure, or a Main-axis split root). There, `request_sizes` stores
            // the *intended* per-child span in `data.size` — which is what positioning needs —
            // rather than the tile's currently-committed size, so the two legitimately differ
            // until the window commits its configure. The flat cross-split path instead derives
            // `data` from the tiles, so the check is meaningful there. It's also only meaningful in
            // normal sizing mode: fullscreen/maximized layout bypasses the flat path (and `data`),
            // sizing tiles directly.
            let root_is_main_split =
                matches!(&self.root, TileNode::Internal { layout: Layout::SplitH, .. });
            if !has_nested && !root_is_main_split && self.pending_sizing_mode().is_normal() {
                let mut data2 = *data;
                data2.update(tile, self.axis());
                assert_eq!(data, &data2, "tile data must be up to date");
            }

            if matches!(data.span, ChildSpan::Fixed(_)) {
                assert!(
                    !found_fixed,
                    "there can only be one fixed-height window in a section"
                );
                found_fixed = true;
            }

            if let ChildSpan::Preset(idx) = data.span {
                assert!(self.options.layout.preset_window_heights.len() > idx);
            }

            let requested_size = tile.window().requested_size().unwrap();
            let requested_size = self.axis().size_in(requested_size);
            let requested_tile_height =
                self.tile_cross_span_for_window_cross_span(tile, f64::from(requested_size.h));
            let min_tile_height = f64::max(1., self.map_size_in(tile.min_size_nonfullscreen()).h);

            if !is_tabbed
                && self.pending_sizing_mode().is_normal()
                && self.scale.round() == self.scale
                && working_size.h.round() == working_size.h
                && gaps.round() == gaps
            {
                let total_height = requested_tile_height + gaps * 2. + extra_size.h;
                let total_min_height = min_tile_height + gaps * 2. + extra_size.h;
                let max_height = f64::max(total_min_height, working_size.h);
                assert!(
                    total_height <= max_height,
                    "each tile in a section mustn't go beyond working area height \
                     (tile height {total_height} > max height {max_height})"
                );
            }

            total_height += requested_tile_height;
            total_min_height += min_tile_height;
        }

        if !is_tabbed
            && !is_main_split
            && child_count > 1
            && self.scale.round() == self.scale
            && working_size.h.round() == working_size.h
            && gaps.round() == gaps
        {
            total_height += gaps * (child_count + 1) as f64 + extra_size.h;
            total_min_height += gaps * (child_count + 1) as f64 + extra_size.h;
            let max_height = f64::max(total_min_height, working_size.h);
            assert!(
                total_height <= max_height,
                "multiple tiles in a section mustn't go beyond working area height \
                 (total height {total_height} > max height {max_height})"
            );
        }
    }
}

fn compute_new_view_offset(
    current_view_main: f64,
    view_main_span: f64,
    new_section_main: f64,
    new_section_span: f64,
    gaps: f64,
) -> f64 {
    // If the section is wider than the view, always align it to the start of the main axis.
    if view_main_span <= new_section_span {
        return 0.;
    }

    // Compute the padding in case it needs to be smaller due to large section span.
    let padding = ((view_main_span - new_section_span) / 2.).clamp(0., gaps);

    // Compute the desired start/end positions with padding.
    let desired_start = new_section_main - padding;
    let desired_end = new_section_main + new_section_span + padding;

    // If the section is already fully visible, leave the view as is.
    if current_view_main <= desired_start && desired_end <= current_view_main + view_main_span {
        return -(new_section_main - current_view_main);
    }

    // Otherwise, prefer the alignment that results in less motion from the current position.
    let dist_to_start = (current_view_main - desired_start).abs();
    let dist_to_end = ((current_view_main + view_main_span) - desired_end).abs();
    if dist_to_start <= dist_to_end {
        -padding
    } else {
        -(view_main_span - padding - new_section_span)
    }
}

fn compute_working_area(
    parent_area: Rectangle<f64, Logical>,
    scale: f64,
    struts: Struts,
) -> Rectangle<f64, Logical> {
    let mut working_area = parent_area;

    // Add struts.
    working_area.size.w = f64::max(0., working_area.size.w - struts.left.0 - struts.right.0);
    working_area.loc.x += struts.left.0;

    working_area.size.h = f64::max(0., working_area.size.h - struts.top.0 - struts.bottom.0);
    working_area.loc.y += struts.top.0;

    // Round location to start at a physical pixel.
    let loc = working_area
        .loc
        .to_physical_precise_ceil(scale)
        .to_logical(scale);

    let mut size_diff = (loc - working_area.loc).to_size();
    size_diff.w = f64::min(working_area.size.w, size_diff.w);
    size_diff.h = f64::min(working_area.size.h, size_diff.h);

    working_area.size -= size_diff;
    working_area.loc = loc;

    working_area
}

fn compute_toplevel_bounds(
    border_config: niri_config::Border,
    working_area_size: Size<f64, Logical>,
    extra_size: Size<f64, Logical>,
    gaps: f64,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - gaps * 2. - extra_size.w - border, 1.),
        f64::max(working_area_size.h - gaps * 2. - extra_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn cancel_resize_for_section<W: LayoutElement>(
    interactive_resize: &mut Option<InteractiveResize<W>>,
    section: &mut Section<W>,
) {
    if let Some(resize) = interactive_resize {
        if section.contains(&resize.window) {
            *interactive_resize = None;
        }
    }

    for (_, tile) in section.tiles_enumerated_mut() {
        tile.window_mut().cancel_interactive_resize();
    }
}

fn resolve_preset_size(
    preset: PresetSize,
    options: &Options,
    view_size: f64,
    extra_size: f64,
) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(
            (view_size - options.layout.gaps) * proportion - options.layout.gaps - extra_size,
        ),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}

#[cfg(test)]
mod tests {
    use niri_config::FloatOrInt;

    use super::*;
    use crate::utils::round_logical_in_physical;

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        let area = compute_working_area(parent_area, 1., struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        compute_working_area(parent_area, 1., struts);
    }
}
