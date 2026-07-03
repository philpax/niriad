use std::cell::{Cell, OnceCell, RefCell};

use niri_config::utils::{Flag, MergeWith as _};
use niri_config::workspace::WorkspaceName;
use niri_config::{
    CenterFocusedSection, FloatOrInt, MainAxis, OutputName, Struts, TabIndicatorLength,
    TabIndicatorPosition, WorkspaceReference,
};
use proptest::prelude::*;
use proptest_derive::Arbitrary;
use smithay::output::{Mode, PhysicalProperties, Subpixel};
use smithay::utils::Rectangle;

use super::*;

mod animations;
mod fullscreen;

impl<W: LayoutElement> Default for Layout<W> {
    fn default() -> Self {
        Self::with_options(Clock::with_time(Duration::ZERO), Default::default())
    }
}

#[derive(Debug)]
struct TestWindowInner {
    id: usize,
    parent_id: Cell<Option<usize>>,
    bbox: Cell<Rectangle<i32, Logical>>,
    initial_bbox: Rectangle<i32, Logical>,
    requested_size: Cell<Option<Size<i32, Logical>>>,
    // Emulates the window ignoring the compositor-provided size.
    forced_size: Cell<Option<Size<i32, Logical>>>,
    min_size: Size<i32, Logical>,
    max_size: Size<i32, Logical>,
    pending_sizing_mode: Cell<SizingMode>,
    pending_activated: Cell<bool>,
    sizing_mode: Cell<SizingMode>,
    is_windowed_fullscreen: Cell<bool>,
    is_pending_windowed_fullscreen: Cell<bool>,
    animate_next_configure: Cell<bool>,
    animation_snapshot: RefCell<Option<LayoutElementRenderSnapshot>>,
    rules: ResolvedWindowRules,
}

#[derive(Debug, Clone)]
struct TestWindow(Rc<TestWindowInner>);

#[derive(Debug, Clone, Arbitrary)]
struct TestWindowParams {
    #[proptest(strategy = "1..=5usize")]
    id: usize,
    #[proptest(strategy = "arbitrary_parent_id()")]
    parent_id: Option<usize>,
    is_floating: bool,
    #[proptest(strategy = "arbitrary_bbox()")]
    bbox: Rectangle<i32, Logical>,
    #[proptest(strategy = "arbitrary_min_max_size()")]
    min_max_size: (Size<i32, Logical>, Size<i32, Logical>),
    #[proptest(strategy = "prop::option::of(arbitrary_rules())")]
    rules: Option<ResolvedWindowRules>,
}

impl TestWindowParams {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            parent_id: None,
            is_floating: false,
            bbox: Rectangle::from_size(Size::from((100, 200))),
            min_max_size: Default::default(),
            rules: None,
        }
    }
}

impl TestWindow {
    fn new(params: TestWindowParams) -> Self {
        Self(Rc::new(TestWindowInner {
            id: params.id,
            parent_id: Cell::new(params.parent_id),
            bbox: Cell::new(params.bbox),
            initial_bbox: params.bbox,
            requested_size: Cell::new(None),
            forced_size: Cell::new(None),
            min_size: params.min_max_size.0,
            max_size: params.min_max_size.1,
            pending_sizing_mode: Cell::new(SizingMode::Normal),
            pending_activated: Cell::new(false),
            sizing_mode: Cell::new(SizingMode::Normal),
            is_windowed_fullscreen: Cell::new(false),
            is_pending_windowed_fullscreen: Cell::new(false),
            animate_next_configure: Cell::new(false),
            animation_snapshot: RefCell::new(None),
            rules: params.rules.unwrap_or_default(),
        }))
    }

    fn communicate(&self) -> bool {
        let mut changed = false;

        let size = self.0.forced_size.get().or(self.0.requested_size.get());
        if let Some(size) = size {
            assert!(size.w >= 0);
            assert!(size.h >= 0);

            let mut new_bbox = self.0.initial_bbox;
            if size.w != 0 {
                new_bbox.size.w = size.w;
            }
            if size.h != 0 {
                new_bbox.size.h = size.h;
            }

            if self.0.bbox.get() != new_bbox {
                if self.0.animate_next_configure.get() {
                    self.0.animation_snapshot.replace(Some(RenderSnapshot {
                        contents: Vec::new(),
                        contents_with_blocked_out_bg: None,
                        blocked_out_contents: Vec::new(),
                        block_out_from: None,
                        size: self.0.bbox.get().size.to_f64(),
                        texture: OnceCell::new(),
                        texture_with_blocked_out_bg: Default::default(),
                        blocked_out_texture: OnceCell::new(),
                    }));
                }

                self.0.bbox.set(new_bbox);
                changed = true;
            }
        }

        self.0.animate_next_configure.set(false);

        if self.0.sizing_mode.get() != self.0.pending_sizing_mode.get() {
            self.0.sizing_mode.set(self.0.pending_sizing_mode.get());
            changed = true;
        }

        if self.0.is_windowed_fullscreen.get() != self.0.is_pending_windowed_fullscreen.get() {
            self.0
                .is_windowed_fullscreen
                .set(self.0.is_pending_windowed_fullscreen.get());
            changed = true;
        }

        changed
    }
}

impl LayoutElement for TestWindow {
    type Id = usize;

    fn id(&self) -> &Self::Id {
        &self.0.id
    }

    fn title(&self) -> Option<String> {
        Some(format!("win{}", self.0.id))
    }

    fn app_id(&self) -> Option<String> {
        Some(format!("app{}", self.0.id))
    }

    fn size(&self) -> Size<i32, Logical> {
        self.0.bbox.get().size
    }

    fn buf_loc(&self) -> Point<i32, Logical> {
        (0, 0).into()
    }

    fn is_in_input_region(&self, _point: Point<f64, Logical>) -> bool {
        false
    }

    fn request_size(
        &mut self,
        size: Size<i32, Logical>,
        mode: SizingMode,
        _animate: bool,
        _transaction: Option<Transaction>,
    ) {
        if self.0.requested_size.get() != Some(size) {
            self.0.requested_size.set(Some(size));
            self.0.animate_next_configure.set(true);
        }

        self.0.pending_sizing_mode.set(mode);

        if mode.is_fullscreen() {
            self.0.is_pending_windowed_fullscreen.set(false);
        }
    }

    fn min_size(&self) -> Size<i32, Logical> {
        self.0.min_size
    }

    fn max_size(&self) -> Size<i32, Logical> {
        self.0.max_size
    }

    fn is_wl_surface(&self, _wl_surface: &WlSurface) -> bool {
        false
    }

    fn set_preferred_scale_transform(&self, _scale: output::Scale, _transform: Transform) {}

    fn has_ssd(&self) -> bool {
        false
    }

    fn output_enter(&self, _output: &Output) {}

    fn output_leave(&self, _output: &Output) {}

    fn set_offscreen_data(&self, _data: Option<OffscreenData>) {}

    fn set_activated(&mut self, active: bool) {
        self.0.pending_activated.set(active);
    }

    fn set_bounds(&self, _bounds: Size<i32, Logical>) {}

    fn is_ignoring_opacity_window_rule(&self) -> bool {
        false
    }

    fn configure_intent(&self) -> ConfigureIntent {
        ConfigureIntent::CanSend
    }

    fn send_pending_configure(&mut self) {}

    fn set_active_in_section(&mut self, _active: bool) {}

    fn set_floating(&mut self, _floating: bool) {}

    fn sizing_mode(&self) -> SizingMode {
        self.0.sizing_mode.get()
    }

    fn pending_sizing_mode(&self) -> SizingMode {
        self.0.pending_sizing_mode.get()
    }

    fn requested_size(&self) -> Option<Size<i32, Logical>> {
        self.0.requested_size.get()
    }

    fn is_windowed_fullscreen(&self) -> bool {
        self.0.is_windowed_fullscreen.get()
    }

    fn is_pending_windowed_fullscreen(&self) -> bool {
        self.0.is_pending_windowed_fullscreen.get()
    }

    fn request_windowed_fullscreen(&mut self, value: bool) {
        self.0.is_pending_windowed_fullscreen.set(value);
    }

    fn is_child_of(&self, parent: &Self) -> bool {
        self.0.parent_id.get() == Some(parent.0.id)
    }

    fn refresh(&self) {}

    fn rules(&self) -> &ResolvedWindowRules {
        &self.0.rules
    }

    fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot> {
        self.0.animation_snapshot.take()
    }

    fn set_interactive_resize(&mut self, _data: Option<InteractiveResizeData>) {}

    fn cancel_interactive_resize(&mut self) {}

    fn on_commit(&mut self, _serial: Serial) {}

    fn interactive_resize_data(&self) -> Option<InteractiveResizeData> {
        None
    }

    fn is_urgent(&self) -> bool {
        false
    }
}

fn arbitrary_size() -> impl Strategy<Value = Size<i32, Logical>> {
    any::<(u16, u16)>().prop_map(|(w, h)| Size::from((w.max(1).into(), h.max(1).into())))
}

fn arbitrary_bbox() -> impl Strategy<Value = Rectangle<i32, Logical>> {
    any::<(i16, i16, u16, u16)>().prop_map(|(x, y, w, h)| {
        let loc: Point<i32, _> = Point::from((x.into(), y.into()));
        let size: Size<i32, _> = Size::from((w.max(1).into(), h.max(1).into()));
        Rectangle::new(loc, size)
    })
}

fn arbitrary_size_change() -> impl Strategy<Value = SizeChange> {
    prop_oneof![
        (0..).prop_map(SizeChange::SetFixed),
        (0f64..).prop_map(SizeChange::SetProportion),
        any::<i32>().prop_map(SizeChange::AdjustFixed),
        any::<f64>().prop_map(SizeChange::AdjustProportion),
        // Interactive resize can have negative values here.
        Just(SizeChange::SetFixed(-100)),
    ]
}

fn arbitrary_position_change() -> impl Strategy<Value = PositionChange> {
    prop_oneof![
        (-1000f64..1000f64).prop_map(PositionChange::SetFixed),
        any::<f64>().prop_map(PositionChange::SetProportion),
        (-1000f64..1000f64).prop_map(PositionChange::AdjustFixed),
        any::<f64>().prop_map(PositionChange::AdjustProportion),
        any::<f64>().prop_map(PositionChange::SetFixed),
        any::<f64>().prop_map(PositionChange::AdjustFixed),
    ]
}

fn arbitrary_min_max() -> impl Strategy<Value = (i32, i32)> {
    prop_oneof![
        Just((0, 0)),
        (1..65536).prop_map(|n| (n, n)),
        (1..65536).prop_map(|min| (min, 0)),
        (1..).prop_map(|max| (0, max)),
        (1..65536, 1..).prop_map(|(min, max): (i32, i32)| (min, max.max(min))),
    ]
}

fn arbitrary_min_max_size() -> impl Strategy<Value = (Size<i32, Logical>, Size<i32, Logical>)> {
    prop_oneof![
        5 => (arbitrary_min_max(), arbitrary_min_max()).prop_map(
            |((min_w, max_w), (min_h, max_h))| {
                let min_size = Size::from((min_w, min_h));
                let max_size = Size::from((max_w, max_h));
                (min_size, max_size)
            },
        ),
        1 => arbitrary_min_max().prop_map(|(w, h)| {
            let size = Size::from((w, h));
            (size, size)
        }),
    ]
}

prop_compose! {
    fn arbitrary_rules()(
        focus_ring in arbitrary_focus_ring(),
        border in arbitrary_border(),
    ) -> ResolvedWindowRules {
        ResolvedWindowRules {
            focus_ring,
            border,
            ..ResolvedWindowRules::default()
        }
    }
}

fn arbitrary_view_offset_gesture_delta() -> impl Strategy<Value = f64> {
    prop_oneof![(-10f64..10f64), (-50000f64..50000f64),]
}

fn arbitrary_resize_edge() -> impl Strategy<Value = ResizeEdge> {
    prop_oneof![
        Just(ResizeEdge::RIGHT),
        Just(ResizeEdge::BOTTOM),
        Just(ResizeEdge::LEFT),
        Just(ResizeEdge::TOP),
        Just(ResizeEdge::BOTTOM_RIGHT),
        Just(ResizeEdge::BOTTOM_LEFT),
        Just(ResizeEdge::TOP_RIGHT),
        Just(ResizeEdge::TOP_LEFT),
        Just(ResizeEdge::empty()),
    ]
}

fn arbitrary_scale() -> impl Strategy<Value = f64> {
    prop_oneof![Just(1.), Just(1.5), Just(2.),]
}

fn arbitrary_msec_delta() -> impl Strategy<Value = i32> {
    prop_oneof![
        1 => Just(-1000),
        2 => Just(-10),
        1 => Just(0),
        2 => Just(10),
        6 => Just(1000),
    ]
}

fn arbitrary_parent_id() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![
        5 => Just(None),
        1 => prop::option::of(1..=5usize),
    ]
}

fn arbitrary_scroll_direction() -> impl Strategy<Value = ScrollDirection> {
    prop_oneof![Just(ScrollDirection::Left), Just(ScrollDirection::Right)]
}

fn arbitrary_node_layout() -> impl Strategy<Value = super::tile_node::Layout> {
    use super::tile_node::Layout;
    prop_oneof![
        Just(Layout::SplitH),
        Just(Layout::SplitV),
        Just(Layout::Tabbed),
        Just(Layout::Stacked),
    ]
}

fn arbitrary_section_display() -> impl Strategy<Value = SectionDisplay> {
    prop_oneof![Just(SectionDisplay::Normal), Just(SectionDisplay::Tabbed)]
}

fn arbitrary_split_direction() -> impl Strategy<Value = niri_ipc::SplitDirection> {
    prop_oneof![
        Just(niri_ipc::SplitDirection::Main),
        Just(niri_ipc::SplitDirection::Cross),
    ]
}

fn arbitrary_tab_direction() -> impl Strategy<Value = niri_ipc::TabDirection> {
    prop_oneof![
        Just(niri_ipc::TabDirection::Left),
        Just(niri_ipc::TabDirection::Right),
    ]
}

#[derive(Debug, Clone, Arbitrary)]
enum Op {
    AddOutput(#[proptest(strategy = "1..=5usize")] usize),
    AddScaledOutput {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "arbitrary_scale()")]
        scale: f64,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    RemoveOutput(#[proptest(strategy = "1..=5usize")] usize),
    FocusOutput(#[proptest(strategy = "1..=5usize")] usize),
    UpdateOutputLayoutConfig {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    AddNamedWorkspace {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
        #[proptest(strategy = "prop::option::of(1..=5usize)")]
        output_name: Option<usize>,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    UnnameWorkspace {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
    },
    UpdateWorkspaceLayoutConfig {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    AddWindow {
        params: TestWindowParams,
    },
    AddWindowNextTo {
        params: TestWindowParams,
        #[proptest(strategy = "1..=5usize")]
        next_to_id: usize,
    },
    AddWindowToNamedWorkspace {
        params: TestWindowParams,
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
    },
    CloseWindow(#[proptest(strategy = "1..=5usize")] usize),
    FullscreenWindow(#[proptest(strategy = "1..=5usize")] usize),
    SetFullscreenWindow {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        is_fullscreen: bool,
    },
    ToggleWindowedFullscreen(#[proptest(strategy = "1..=5usize")] usize),
    FocusSectionLeft,
    FocusSectionRight,
    FocusSectionFirst,
    FocusSectionLast,
    FocusSectionRightOrFirst,
    FocusSectionLeftOrLast,
    FocusSection(#[proptest(strategy = "1..=5usize")] usize),
    FocusWindowOrMonitorUp(#[proptest(strategy = "1..=2u8")] u8),
    FocusWindowOrMonitorDown(#[proptest(strategy = "1..=2u8")] u8),
    FocusSectionOrMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
    FocusSectionOrMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
    FocusWindowDown,
    FocusWindowUp,
    FocusWindowDownOrSectionLeft,
    FocusWindowDownOrSectionRight,
    FocusWindowUpOrSectionLeft,
    FocusWindowUpOrSectionRight,
    FocusWindowOrWorkspaceDown,
    FocusWindowOrWorkspaceUp,
    FocusWindow(#[proptest(strategy = "1..=5usize")] usize),
    FocusWindowInSection(#[proptest(strategy = "1..=5u8")] u8),
    FocusWindowTop,
    FocusWindowBottom,
    FocusWindowDownOrTop,
    FocusWindowUpOrBottom,
    MoveSectionLeft,
    MoveSectionRight,
    MoveSectionToFirst,
    MoveSectionToLast,
    MoveSectionLeftOrToMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
    MoveSectionRightOrToMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
    MoveSectionToIndex(#[proptest(strategy = "1..=5usize")] usize),
    MoveWindowDown,
    MoveWindowUp,
    MoveWindowDownOrToWorkspaceDown,
    MoveWindowUpOrToWorkspaceUp,
    ConsumeOrExpelWindowLeft {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ConsumeOrExpelWindowRight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ConsumeWindowIntoSection,
    ExpelWindowFromSection,
    SplitWindow(#[proptest(strategy = "arbitrary_split_direction()")] niri_ipc::SplitDirection),
    ConsumeWindowIntoSplit,
    SwapWindowInDirection(#[proptest(strategy = "arbitrary_scroll_direction()")] ScrollDirection),
    ToggleSectionTabbedDisplay,
    ToggleTabbed,
    MoveTab(#[proptest(strategy = "arbitrary_tab_direction()")] niri_ipc::TabDirection),
    SetSectionDisplay(#[proptest(strategy = "arbitrary_section_display()")] SectionDisplay),
    SetLayout(#[proptest(strategy = "arbitrary_node_layout()")] super::tile_node::Layout),
    CenterSection,
    CenterWindow {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    CenterVisibleSections,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    FocusWorkspace(#[proptest(strategy = "0..=4usize")] usize),
    FocusWorkspaceAutoBackAndForth(#[proptest(strategy = "0..=4usize")] usize),
    FocusWorkspacePrevious,
    MoveWindowToWorkspaceDown(bool),
    MoveWindowToWorkspaceUp(bool),
    MoveWindowToWorkspace {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        window_id: Option<usize>,
        #[proptest(strategy = "0..=4usize")]
        workspace_idx: usize,
    },
    MoveSectionToWorkspaceDown(bool),
    MoveSectionToWorkspaceUp(bool),
    MoveSectionToWorkspace(#[proptest(strategy = "0..=4usize")] usize, bool),
    MoveWorkspaceDown,
    MoveWorkspaceUp,
    MoveWorkspaceToIndex {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
        #[proptest(strategy = "0..=4usize")]
        target_idx: usize,
    },
    MoveWorkspaceToMonitor {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
        #[proptest(strategy = "0..=5usize")]
        output_id: usize,
    },
    SetWorkspaceName {
        #[proptest(strategy = "1..=5usize")]
        new_ws_name: usize,
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
    },
    UnsetWorkspaceName {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
    },
    MoveWindowToOutput {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        window_id: Option<usize>,
        #[proptest(strategy = "1..=5usize")]
        output_id: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        target_ws_idx: Option<usize>,
    },
    MoveSectionToOutput {
        #[proptest(strategy = "1..=5usize")]
        output_id: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        target_ws_idx: Option<usize>,
        activate: bool,
    },
    SwitchPresetSectionWidth,
    SwitchPresetSectionWidthBack,
    SwitchPresetWindowWidth {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowWidthBack {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowHeightBack {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    MaximizeSection,
    MaximizeWindowToEdges {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SetSectionWidth(#[proptest(strategy = "arbitrary_size_change()")] SizeChange),
    SetWindowWidth {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_size_change()")]
        change: SizeChange,
    },
    SetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_size_change()")]
        change: SizeChange,
    },
    ResetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ExpandSectionToAvailableWidth,
    ToggleWindowFloating {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SetWindowFloating {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        floating: bool,
    },
    FocusFloating,
    FocusTiling,
    SwitchFocusFloatingTiling,
    MoveFloatingWindow {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_position_change()")]
        x: PositionChange,
        #[proptest(strategy = "arbitrary_position_change()")]
        y: PositionChange,
        animate: bool,
    },
    SetParent {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "prop::option::of(1..=5usize)")]
        new_parent_id: Option<usize>,
    },
    SetForcedSize {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "proptest::option::of(arbitrary_size())")]
        size: Option<Size<i32, Logical>>,
    },
    Communicate(#[proptest(strategy = "1..=5usize")] usize),
    Refresh {
        is_active: bool,
    },
    AdvanceAnimations {
        #[proptest(strategy = "arbitrary_msec_delta()")]
        msec_delta: i32,
    },
    CompleteAnimations,
    MoveWorkspaceToOutput(#[proptest(strategy = "1..=5usize")] usize),
    ViewOffsetGestureBegin {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        workspace_idx: Option<usize>,
        is_touchpad: bool,
    },
    ViewOffsetGestureUpdate {
        #[proptest(strategy = "arbitrary_view_offset_gesture_delta()")]
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    },
    ViewOffsetGestureEnd {
        is_touchpad: Option<bool>,
    },
    WorkspaceSwitchGestureBegin {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        is_touchpad: bool,
    },
    WorkspaceSwitchGestureUpdate {
        #[proptest(strategy = "-400f64..400f64")]
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    },
    WorkspaceSwitchGestureEnd {
        is_touchpad: Option<bool>,
    },
    OverviewGestureBegin,
    OverviewGestureUpdate {
        #[proptest(strategy = "-400f64..400f64")]
        delta: f64,
        timestamp: Duration,
    },
    OverviewGestureEnd,
    InteractiveMoveBegin {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    InteractiveMoveUpdate {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        dx: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        dy: f64,
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    InteractiveMoveEnd {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
    },
    DndUpdate {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    DndEnd,
    InteractiveResizeBegin {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "arbitrary_resize_edge()")]
        edges: ResizeEdge,
    },
    InteractiveResizeUpdate {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        dx: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        dy: f64,
    },
    InteractiveResizeEnd {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
    },
    ToggleOverview,
    UpdateConfig {
        #[proptest(strategy = "arbitrary_layout_part().prop_map(Box::new)")]
        layout_config: Box<niri_config::LayoutPart>,
    },
}

impl Op {
    fn apply(self, layout: &mut Layout<TestWindow>) {
        match self {
            Op::AddOutput(id) => {
                let name = format!("output{id}");
                if layout.outputs().any(|o| o.name() == name) {
                    return;
                }

                let output = Output::new(
                    name.clone(),
                    PhysicalProperties {
                        size: Size::from((1280, 720)),
                        subpixel: Subpixel::Unknown,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                    },
                );
                output.change_current_state(
                    Some(Mode {
                        size: Size::from((1280, 720)),
                        refresh: 60000,
                    }),
                    None,
                    None,
                    None,
                );
                output.user_data().insert_if_missing(|| OutputName {
                    connector: name,
                    make: None,
                    model: None,
                    serial: None,
                });
                layout.add_output(output.clone(), None);
            }
            Op::AddScaledOutput {
                id,
                scale,
                layout_config,
            } => {
                let name = format!("output{id}");
                if layout.outputs().any(|o| o.name() == name) {
                    return;
                }

                let output = Output::new(
                    name.clone(),
                    PhysicalProperties {
                        size: Size::from((1280, 720)),
                        subpixel: Subpixel::Unknown,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                    },
                );
                output.change_current_state(
                    Some(Mode {
                        size: Size::from((1280, 720)),
                        refresh: 60000,
                    }),
                    None,
                    Some(smithay::output::Scale::Fractional(scale)),
                    None,
                );
                output.user_data().insert_if_missing(|| OutputName {
                    connector: name,
                    make: None,
                    model: None,
                    serial: None,
                });
                layout.add_output(output.clone(), layout_config.map(|x| *x));
            }
            Op::RemoveOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.remove_output(&output);
            }
            Op::FocusOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_output(&output);
            }
            Op::UpdateOutputLayoutConfig { id, layout_config } => {
                let name = format!("output{id}");
                let Some(mon) = layout.monitors_mut().find(|m| m.output_name() == &name) else {
                    return;
                };

                mon.update_layout_config(layout_config.map(|x| *x));
            }
            Op::AddNamedWorkspace {
                ws_name,
                output_name,
                layout_config,
            } => {
                layout.ensure_named_workspace(&WorkspaceConfig {
                    name: WorkspaceName(format!("ws{ws_name}")),
                    open_on_output: output_name.map(|name| format!("output{name}")),
                    layout: layout_config.map(|x| niri_config::WorkspaceLayoutPart(*x)),
                });
            }
            Op::UnnameWorkspace { ws_name } => {
                layout.unname_workspace(&format!("ws{ws_name}"));
            }
            Op::UpdateWorkspaceLayoutConfig {
                ws_name,
                layout_config,
            } => {
                let ws_name = format!("ws{ws_name}");
                let Some(ws) = layout
                    .workspaces_mut()
                    .find(|ws| ws.name() == Some(&ws_name))
                else {
                    return;
                };

                ws.update_layout_config(layout_config.map(|x| *x));
            }
            Op::SetWorkspaceName {
                new_ws_name,
                ws_name,
            } => {
                let ws_ref =
                    ws_name.map(|ws_name| WorkspaceReference::Name(format!("ws{ws_name}")));
                layout.set_workspace_name(format!("ws{new_ws_name}"), ws_ref);
            }
            Op::UnsetWorkspaceName { ws_name } => {
                let ws_ref =
                    ws_name.map(|ws_name| WorkspaceReference::Name(format!("ws{ws_name}")));
                layout.unset_workspace_name(ws_ref);
            }
            Op::AddWindow { mut params } => {
                if layout.has_window(&params.id) {
                    return;
                }
                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::Auto,
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::AddWindowNextTo {
                mut params,
                next_to_id,
            } => {
                let mut found_next_to = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    let win_id = move_.tile.window().0.id;
                    if win_id == params.id {
                        return;
                    }
                    if win_id == next_to_id {
                        found_next_to = true;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == params.id {
                                        return;
                                    }

                                    if win.0.id == next_to_id {
                                        found_next_to = true;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == params.id {
                                    return;
                                }

                                if win.0.id == next_to_id {
                                    found_next_to = true;
                                }
                            }
                        }
                    }
                }

                if !found_next_to {
                    return;
                }

                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::NextTo(&next_to_id),
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::AddWindowToNamedWorkspace {
                mut params,
                ws_name,
            } => {
                let ws_name = format!("ws{ws_name}");
                let mut ws_id = None;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == params.id {
                        return;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == params.id {
                                        return;
                                    }
                                }

                                if ws
                                    .name
                                    .as_ref()
                                    .is_some_and(|name| name.eq_ignore_ascii_case(&ws_name))
                                {
                                    ws_id = Some(ws.id());
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == params.id {
                                    return;
                                }
                            }

                            if ws
                                .name
                                .as_ref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(&ws_name))
                            {
                                ws_id = Some(ws.id());
                            }
                        }
                    }
                }

                let Some(ws_id) = ws_id else {
                    return;
                };

                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::Workspace(ws_id),
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::CloseWindow(id) => {
                layout.remove_window(&id, Transaction::new());
            }
            Op::FullscreenWindow(id) => {
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_fullscreen(&id);
            }
            Op::SetFullscreenWindow {
                window,
                is_fullscreen,
            } => {
                if !layout.has_window(&window) {
                    return;
                }
                layout.set_fullscreen(&window, is_fullscreen);
            }
            Op::ToggleWindowedFullscreen(id) => {
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_windowed_fullscreen(&id);
            }
            Op::FocusSectionLeft => layout.focus_left(),
            Op::FocusSectionRight => layout.focus_right(),
            Op::FocusSectionFirst => layout.focus_section_first(),
            Op::FocusSectionLast => layout.focus_section_last(),
            Op::FocusSectionRightOrFirst => layout.focus_section_right_or_first(),
            Op::FocusSectionLeftOrLast => layout.focus_section_left_or_last(),
            Op::FocusSection(index) => layout.focus_section(index),
            Op::FocusWindowOrMonitorUp(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_window_up_or_output(&output);
            }
            Op::FocusWindowOrMonitorDown(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_window_down_or_output(&output);
            }
            Op::FocusSectionOrMonitorLeft(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_section_left_or_output(&output);
            }
            Op::FocusSectionOrMonitorRight(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_section_right_or_output(&output);
            }
            Op::FocusWindowDown => layout.focus_down(),
            Op::FocusWindowUp => layout.focus_up(),
            Op::FocusWindowDownOrSectionLeft => layout.focus_down_or_left(),
            Op::FocusWindowDownOrSectionRight => layout.focus_down_or_right(),
            Op::FocusWindowUpOrSectionLeft => layout.focus_up_or_left(),
            Op::FocusWindowUpOrSectionRight => layout.focus_up_or_right(),
            Op::FocusWindowOrWorkspaceDown => layout.focus_window_or_workspace_down(),
            Op::FocusWindowOrWorkspaceUp => layout.focus_window_or_workspace_up(),
            Op::FocusWindow(id) => layout.activate_window(&id),
            Op::FocusWindowInSection(index) => layout.focus_window_in_section(index),
            Op::FocusWindowTop => layout.focus_window_top(),
            Op::FocusWindowBottom => layout.focus_window_bottom(),
            Op::FocusWindowDownOrTop => layout.focus_window_down_or_top(),
            Op::FocusWindowUpOrBottom => layout.focus_window_up_or_bottom(),
            Op::MoveSectionLeft => layout.move_left(),
            Op::MoveSectionRight => layout.move_right(),
            Op::MoveSectionToFirst => layout.move_section_to_first(),
            Op::MoveSectionToLast => layout.move_section_to_last(),
            Op::MoveSectionLeftOrToMonitorLeft(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_section_left_or_to_output(&output);
            }
            Op::MoveSectionRightOrToMonitorRight(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_section_right_or_to_output(&output);
            }
            Op::MoveSectionToIndex(index) => layout.move_section_to_index(index),
            Op::MoveWindowDown => layout.move_down(),
            Op::MoveWindowUp => layout.move_up(),
            Op::MoveWindowDownOrToWorkspaceDown => layout.move_down_or_to_workspace_down(),
            Op::MoveWindowUpOrToWorkspaceUp => layout.move_up_or_to_workspace_up(),
            Op::ConsumeOrExpelWindowLeft { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.consume_or_expel_window_left(id.as_ref());
            }
            Op::ConsumeOrExpelWindowRight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.consume_or_expel_window_right(id.as_ref());
            }
            Op::ConsumeWindowIntoSection => layout.consume_into_section(),
            Op::ExpelWindowFromSection => layout.expel_from_section(),
            Op::SplitWindow(direction) => {
                let dir = Some(match direction {
                    niri_ipc::SplitDirection::Main => SplitAxis::Main,
                    niri_ipc::SplitDirection::Cross => SplitAxis::Cross,
                });
                layout.split_window(dir);
            }
            Op::ConsumeWindowIntoSplit => {
                layout.consume_window_into_split(None, None);
            }
            Op::SwapWindowInDirection(direction) => layout.swap_window_in_direction(direction),
            Op::ToggleSectionTabbedDisplay => layout.toggle_section_tabbed_display(),
            Op::ToggleTabbed => layout.toggle_tabbed(),
            Op::MoveTab(direction) => {
                let dir = match direction {
                    niri_ipc::TabDirection::Left => ScrollDirection::Left,
                    niri_ipc::TabDirection::Right => ScrollDirection::Right,
                };
                layout.move_tab(dir);
            }
            Op::SetSectionDisplay(display) => layout.set_section_display(display),
            Op::SetLayout(node_layout) => layout.set_active_layout(node_layout),
            Op::CenterSection => layout.center_section(),
            Op::CenterWindow { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.center_window(id.as_ref());
            }
            Op::CenterVisibleSections => layout.center_visible_sections(),
            Op::FocusWorkspaceDown => layout.switch_workspace_down(),
            Op::FocusWorkspaceUp => layout.switch_workspace_up(),
            Op::FocusWorkspace(idx) => layout.switch_workspace(idx),
            Op::FocusWorkspaceAutoBackAndForth(idx) => {
                layout.switch_workspace_auto_back_and_forth(idx)
            }
            Op::FocusWorkspacePrevious => layout.switch_workspace_previous(),
            Op::MoveWindowToWorkspaceDown(focus) => layout.move_to_workspace_down(focus),
            Op::MoveWindowToWorkspaceUp(focus) => layout.move_to_workspace_up(focus),
            Op::MoveWindowToWorkspace {
                window_id,
                workspace_idx,
            } => {
                let window_id = window_id.filter(|id| layout.has_window(id));
                layout.move_to_workspace(window_id.as_ref(), workspace_idx, ActivateWindow::Smart);
            }
            Op::MoveSectionToWorkspaceDown(focus) => layout.move_section_to_workspace_down(focus),
            Op::MoveSectionToWorkspaceUp(focus) => layout.move_section_to_workspace_up(focus),
            Op::MoveSectionToWorkspace(idx, focus) => layout.move_section_to_workspace(idx, focus),
            Op::MoveWindowToOutput {
                window_id,
                output_id: id,
                target_ws_idx,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                let mon = layout.monitor_for_output(&output).unwrap();

                let window_id = window_id.filter(|id| layout.has_window(id));
                let target_ws_idx = target_ws_idx.filter(|idx| mon.workspaces.len() > *idx);
                layout.move_to_output(
                    window_id.as_ref(),
                    &output,
                    target_ws_idx,
                    ActivateWindow::Smart,
                );
            }
            Op::MoveSectionToOutput {
                output_id: id,
                target_ws_idx,
                activate,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_section_to_output(&output, target_ws_idx, activate);
            }
            Op::MoveWorkspaceDown => layout.move_workspace_down(),
            Op::MoveWorkspaceUp => layout.move_workspace_up(),
            Op::MoveWorkspaceToIndex {
                ws_name: Some(ws_name),
                target_idx,
            } => {
                let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
                    return;
                };

                let Some((old_idx, old_output)) = monitors.iter().find_map(|monitor| {
                    monitor
                        .workspaces
                        .iter()
                        .enumerate()
                        .find_map(|(i, ws)| {
                            if ws.name == Some(format!("ws{ws_name}")) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .map(|i| (i, monitor.output.clone()))
                }) else {
                    return;
                };

                layout.move_workspace_to_idx(Some((Some(old_output), old_idx)), target_idx)
            }
            Op::MoveWorkspaceToIndex {
                ws_name: None,
                target_idx,
            } => layout.move_workspace_to_idx(None, target_idx),
            Op::MoveWorkspaceToMonitor {
                ws_name: None,
                output_id: id,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.move_workspace_to_output(&output);
            }
            Op::MoveWorkspaceToMonitor {
                ws_name: Some(ws_name),
                output_id: id,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
                    return;
                };

                let Some((old_idx, old_output)) = monitors.iter().find_map(|monitor| {
                    monitor
                        .workspaces
                        .iter()
                        .enumerate()
                        .find_map(|(i, ws)| {
                            if ws.name == Some(format!("ws{ws_name}")) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .map(|i| (i, monitor.output.clone()))
                }) else {
                    return;
                };

                layout.move_workspace_to_output_by_id(old_idx, Some(old_output), &output);
            }
            Op::SwitchPresetSectionWidth => layout.toggle_width(true),
            Op::SwitchPresetSectionWidthBack => layout.toggle_width(false),
            Op::SwitchPresetWindowWidth { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_width(id.as_ref(), true);
            }
            Op::SwitchPresetWindowWidthBack { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_width(id.as_ref(), false);
            }
            Op::SwitchPresetWindowHeight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_height(id.as_ref(), true);
            }
            Op::SwitchPresetWindowHeightBack { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_height(id.as_ref(), false);
            }
            Op::MaximizeSection => layout.toggle_full_width(),
            Op::MaximizeWindowToEdges { id } => {
                let id = id.or_else(|| layout.focus().map(|win| *win.id()));
                let Some(id) = id else {
                    return;
                };
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_maximized(&id);
            }
            Op::SetSectionWidth(change) => layout.set_section_width(change),
            Op::SetWindowWidth { id, change } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_width(id.as_ref(), change);
            }
            Op::SetWindowHeight { id, change } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_height(id.as_ref(), change);
            }
            Op::ResetWindowHeight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.reset_window_height(id.as_ref());
            }
            Op::ExpandSectionToAvailableWidth => layout.expand_section_to_available_width(),
            Op::ToggleWindowFloating { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_floating(id.as_ref());
            }
            Op::SetWindowFloating { id, floating } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_floating(id.as_ref(), floating);
            }
            Op::FocusFloating => {
                layout.focus_floating();
            }
            Op::FocusTiling => {
                layout.focus_tiling();
            }
            Op::SwitchFocusFloatingTiling => {
                layout.switch_focus_floating_tiling();
            }
            Op::MoveFloatingWindow { id, x, y, animate } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.move_floating_window(id.as_ref(), x, y, animate);
            }
            Op::SetParent {
                id,
                mut new_parent_id,
            } => {
                if !layout.has_window(&id) {
                    return;
                }

                if let Some(parent_id) = new_parent_id {
                    if parent_id_causes_loop(layout, id, parent_id) {
                        new_parent_id = None;
                    }
                }

                let mut update = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == id {
                        move_.tile.window().0.parent_id.set(new_parent_id);
                        update = true;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        'outer: for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        win.0.parent_id.set(new_parent_id);
                                        update = true;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        'outer: for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == id {
                                    win.0.parent_id.set(new_parent_id);
                                    update = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                if update {
                    if let Some(new_parent_id) = new_parent_id {
                        layout.descendants_added(&new_parent_id);
                    }
                }
            }
            Op::SetForcedSize { id, size } => {
                for (_mon, win) in layout.windows() {
                    if win.0.id == id {
                        win.0.forced_size.set(size);
                        return;
                    }
                }
            }
            Op::Communicate(id) => {
                let mut update = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == id {
                        if move_.tile.window().communicate() {
                            update = true;
                        }

                        if update {
                            // FIXME: serial.
                            layout.update_window(&id, None);
                        }
                        return;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        'outer: for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        if win.communicate() {
                                            update = true;
                                        }
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        'outer: for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == id {
                                    if win.communicate() {
                                        update = true;
                                    }
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                if update {
                    // FIXME: serial.
                    layout.update_window(&id, None);
                }
            }
            Op::Refresh { is_active } => {
                layout.refresh(is_active);
            }
            Op::AdvanceAnimations { msec_delta } => {
                let mut now = layout.clock.now_unadjusted();
                if msec_delta >= 0 {
                    now = now.saturating_add(Duration::from_millis(msec_delta as u64));
                } else {
                    now = now.saturating_sub(Duration::from_millis(-msec_delta as u64));
                }
                layout.clock.set_unadjusted(now);
                layout.advance_animations();
            }
            Op::CompleteAnimations => {
                layout.clock.set_complete_instantly(true);
                layout.advance_animations();
                layout.clock.set_complete_instantly(false);
            }
            Op::MoveWorkspaceToOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_workspace_to_output(&output);
            }
            Op::ViewOffsetGestureBegin {
                output_idx: id,
                workspace_idx,
                is_touchpad: normalize,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.view_offset_gesture_begin(&output, workspace_idx, normalize);
            }
            Op::ViewOffsetGestureUpdate {
                delta,
                timestamp,
                is_touchpad,
            } => {
                layout.view_offset_gesture_update(delta, timestamp, is_touchpad);
            }
            Op::ViewOffsetGestureEnd { is_touchpad } => {
                layout.view_offset_gesture_end(is_touchpad);
            }
            Op::WorkspaceSwitchGestureBegin {
                output_idx: id,
                is_touchpad,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.workspace_switch_gesture_begin(&output, is_touchpad);
            }
            Op::WorkspaceSwitchGestureUpdate {
                delta,
                timestamp,
                is_touchpad,
            } => {
                layout.workspace_switch_gesture_update(delta, timestamp, is_touchpad);
            }
            Op::WorkspaceSwitchGestureEnd { is_touchpad } => {
                layout.workspace_switch_gesture_end(is_touchpad);
            }
            Op::OverviewGestureBegin => {
                layout.overview_gesture_begin();
            }
            Op::OverviewGestureUpdate { delta, timestamp } => {
                layout.overview_gesture_update(delta, timestamp);
            }
            Op::OverviewGestureEnd => {
                layout.overview_gesture_end();
            }
            Op::InteractiveMoveBegin {
                window,
                output_idx,
                px,
                py,
            } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.interactive_move_begin(window, &output, Point::from((px, py)));
            }
            Op::InteractiveMoveUpdate {
                window,
                dx,
                dy,
                output_idx,
                px,
                py,
            } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.interactive_move_update(
                    &window,
                    Point::from((dx, dy)),
                    output,
                    Point::from((px, py)),
                );
            }
            Op::InteractiveMoveEnd { window } => {
                layout.interactive_move_end(&window);
            }
            Op::DndUpdate { output_idx, px, py } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.dnd_update(output, Point::from((px, py)));
            }
            Op::DndEnd => {
                layout.dnd_end();
            }
            Op::InteractiveResizeBegin { window, edges } => {
                layout.interactive_resize_begin(window, edges);
            }
            Op::InteractiveResizeUpdate { window, dx, dy } => {
                layout.interactive_resize_update(&window, Point::from((dx, dy)));
            }
            Op::InteractiveResizeEnd { window } => {
                layout.interactive_resize_end(&window);
            }
            Op::ToggleOverview => {
                layout.toggle_overview();
            }
            Op::UpdateConfig { layout_config } => {
                let options = Options {
                    layout: niri_config::Layout::from_part(&layout_config),
                    ..Default::default()
                };

                layout.update_options(options);
            }
        }
    }
}

#[track_caller]
fn check_ops_on_layout(layout: &mut Layout<TestWindow>, ops: impl IntoIterator<Item = Op>) {
    for op in ops {
        op.apply(layout);
        layout.verify_invariants();
    }
}

#[track_caller]
fn check_ops(ops: impl IntoIterator<Item = Op>) -> Layout<TestWindow> {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, ops);
    layout
}

#[track_caller]
fn check_ops_with_options(
    options: Options,
    ops: impl IntoIterator<Item = Op>,
) -> Layout<TestWindow> {
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);
    check_ops_on_layout(&mut layout, ops);
    layout
}

#[test]
fn vertical_main_axis_places_sections_vertically() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
        ],
    );

    let ws = layout.active_workspace().unwrap();
    let positions: Vec<_> = ws
        .tiles_with_render_positions()
        .map(|(_, pos, _)| pos)
        .collect();

    assert_eq!(positions.len(), 2);

    let dx = (positions[0].x - positions[1].x).abs();
    let dy = (positions[0].y - positions[1].y).abs();
    assert!(
        dy > dx,
        "expected vertical separation, got dx={dx}, dy={dy}"
    );
}

#[test]
fn vertical_main_axis_insert_position_follows_y() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
        ],
    );

    let ws = layout.active_workspace().unwrap();
    let mut centers: Vec<_> = ws
        .tiles_with_render_positions()
        .map(|(tile, pos, _)| {
            let size = tile.window().size().to_f64();
            Point::from((pos.x + size.w / 2., pos.y + size.h / 2.))
        })
        .collect();
    centers.sort_by(|a, b| a.y.total_cmp(&b.y));

    assert_eq!(centers.len(), 2);

    let insert_col_idx = |center| match ws.scrolling_insert_position(center) {
        super::monitor::InsertPosition::NewSection(idx)
        | super::monitor::InsertPosition::InSection(idx, _)
        | super::monitor::InsertPosition::InSplit(idx, _, _, _)
        | super::monitor::InsertPosition::InsertTab(idx, _)
        | super::monitor::InsertPosition::Swap(idx, _) => idx,
        super::monitor::InsertPosition::Floating => unreachable!(),
    };

    let upper_idx = insert_col_idx(centers[0]);
    let lower_idx = insert_col_idx(centers[1]);

    assert!(
        lower_idx > upper_idx,
        "expected insert position to progress with y, got upper={upper_idx}, lower={lower_idx}"
    );
}

#[test]
fn vertical_main_axis_dnd_edge_scroll_uses_vertical_edges() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
        ],
    );

    let ws = layout.active_workspace_mut().unwrap();
    let area = ws.working_area();

    ws.dnd_scroll_gesture_begin();

    let center =
        Point::<f64, Logical>::from((area.loc.x + area.size.w / 2., area.loc.y + area.size.h / 2.));
    let left = Point::from((area.loc.x + 1., center.y));
    let top = Point::from((center.x, area.loc.y + 1.));

    assert!(!ws.dnd_scroll_gesture_scroll(left, 1.));
    assert!(ws.dnd_scroll_gesture_scroll(top, 1.));
}

#[test]
fn vertical_main_axis_overview_places_workspaces_horizontally() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::FocusWorkspaceDown,
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::ToggleOverview,
        ],
    );

    let output = layout
        .outputs()
        .find(|output| output.name() == "output1")
        .cloned()
        .unwrap();
    let monitor = layout.monitor_for_output(&output).unwrap();

    let geos: Vec<_> = monitor.workspaces_render_geo().take(2).collect();
    assert_eq!(geos.len(), 2);

    let dx = (geos[0].loc.x - geos[1].loc.x).abs();
    let dy = (geos[0].loc.y - geos[1].loc.y).abs();
    assert!(
        dx > dy,
        "expected overview workspaces to be arranged horizontally, got dx={dx}, dy={dy}"
    );
}

#[test]
fn vertical_main_axis_set_section_width_changes_tile_height() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
        ],
    );

    let (_, win) = layout.windows().next().unwrap();
    let before = win.requested_size().unwrap();

    check_ops_on_layout(
        &mut layout,
        [Op::SetSectionWidth(SizeChange::AdjustProportion(5.))],
    );

    let (_, win) = layout.windows().next().unwrap();
    let after = win.requested_size().unwrap();

    assert_eq!(before.w, after.w);
    assert!(
        after.h > before.h,
        "expected height to grow: {before:?} -> {after:?}"
    );
}

#[test]
fn vertical_main_axis_interactive_resize_bottom_changes_tile_height() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
        ],
    );

    let (_, win) = layout.windows().next().unwrap();
    let before = win.requested_size().unwrap();

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::BOTTOM,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: 0.,
                dy: 120.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );

    let (_, win) = layout.windows().next().unwrap();
    let after = win.requested_size().unwrap();

    assert_eq!(before.w, after.w);
    assert!(
        after.h > before.h,
        "expected interactive resize to grow height: {before:?} -> {after:?}"
    );
}

#[test]
fn vertical_main_axis_interactive_move_tracks_pointer_along_y() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
        ],
    );

    let output = layout
        .outputs()
        .find(|o| o.name() == "output1")
        .cloned()
        .unwrap();

    let (tile_pos, start) = {
        let ws = layout.active_workspace().unwrap();
        let (tile, tile_pos, _) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == 1)
            .unwrap();

        let start = tile_pos + tile.window_loc() + Point::from((10., 10.));
        (tile_pos, start)
    };

    assert!(layout.interactive_move_begin(1, &output, start));

    let delta = Point::from((0., 220.));
    let pointer_pos = start + delta;
    assert!(layout.interactive_move_update(&1, delta, output.clone(), pointer_pos));

    let tile_pos_after = {
        let ws = layout.active_workspace().unwrap();
        ws.tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == 1)
            .unwrap()
            .1
    };

    let moved_x = (tile_pos_after.x - tile_pos.x).abs();
    let moved_y = (tile_pos_after.y - tile_pos.y).abs();
    assert!(
        moved_y > moved_x,
        "expected move gesture to follow y in vertical mode, got dx={moved_x}, dy={moved_y}"
    );
}

#[test]
fn vertical_main_axis_floating_move_section_right_moves_window_down() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let mut before = None;
    layout.with_windows(|win, _, _, layout| {
        if *win.id() == 1 {
            before = layout.tile_pos_in_workspace_view;
        }
    });
    let before = before.unwrap();

    check_ops_on_layout(&mut layout, [Op::MoveSectionRight]);

    let mut after = None;
    layout.with_windows(|win, _, _, layout| {
        if *win.id() == 1 {
            after = layout.tile_pos_in_workspace_view;
        }
    });
    let after = after.unwrap();

    let moved_x = after.0 - before.0;
    let moved_y = after.1 - before.1;
    assert!(
        moved_y > moved_x.abs(),
        "expected move-section-right to move floating window down in vertical mode, got dx={moved_x}, dy={moved_y}"
    );
}

#[test]
fn vertical_main_axis_floating_move_window_down_moves_window_right() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let mut before = None;
    layout.with_windows(|win, _, _, layout| {
        if *win.id() == 1 {
            before = layout.tile_pos_in_workspace_view;
        }
    });
    let before = before.unwrap();

    check_ops_on_layout(&mut layout, [Op::MoveWindowDown]);

    let mut after = None;
    layout.with_windows(|win, _, _, layout| {
        if *win.id() == 1 {
            after = layout.tile_pos_in_workspace_view;
        }
    });
    let after = after.unwrap();

    let moved_x = after.0 - before.0;
    let moved_y = after.1 - before.1;
    assert!(
        moved_x > moved_y.abs(),
        "expected move-window-down to move floating window right in vertical mode, got dx={moved_x}, dy={moved_y}"
    );
}

#[test]
fn vertical_main_axis_floating_set_section_width_changes_window_height() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let (_, win) = layout.windows().next().unwrap();
    let before = win.expected_size().unwrap();

    check_ops_on_layout(
        &mut layout,
        [Op::SetSectionWidth(SizeChange::AdjustProportion(5.))],
    );

    let (_, win) = layout.windows().next().unwrap();
    let after = win.expected_size().unwrap();

    assert_eq!(before.w, after.w);
    assert!(
        after.h > before.h,
        "expected floating section width to grow height in vertical mode: {before:?} -> {after:?}"
    );
}

#[test]
fn vertical_main_axis_floating_set_window_height_changes_window_width() {
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;

    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::ToggleWindowFloating { id: None },
        ],
    );

    let (_, win) = layout.windows().next().unwrap();
    let before = win.expected_size().unwrap();

    check_ops_on_layout(
        &mut layout,
        [Op::SetWindowHeight {
            id: None,
            change: SizeChange::AdjustProportion(5.),
        }],
    );

    let (_, win) = layout.windows().next().unwrap();
    let after = win.expected_size().unwrap();

    assert_eq!(before.h, after.h);
    assert!(
        after.w > before.w,
        "expected floating window height to grow width in vertical mode: {before:?} -> {after:?}"
    );
}

#[test]
fn scrolling_windows_have_ipc_tile_positions() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);

    let mut tile_pos = None;
    layout.with_windows(|win, _, _, layout| {
        if *win.id() == 1 {
            tile_pos = layout.tile_pos_in_workspace_view;
        }
    });

    assert!(tile_pos.is_some());
}

#[test]
fn operations_dont_panic() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    let every_op = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::RemoveOutput(0),
        Op::RemoveOutput(1),
        Op::RemoveOutput(2),
        Op::FocusOutput(0),
        Op::FocusOutput(1),
        Op::FocusOutput(2),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(1),
            layout_config: None,
        },
        Op::UnnameWorkspace { ws_name: 1 },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(2),
            next_to_id: 1,
        },
        Op::AddWindowToNamedWorkspace {
            params: TestWindowParams::new(3),
            ws_name: 1,
        },
        Op::CloseWindow(0),
        Op::CloseWindow(1),
        Op::CloseWindow(2),
        Op::FullscreenWindow(1),
        Op::FullscreenWindow(2),
        Op::FullscreenWindow(3),
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::MaximizeWindowToEdges { id: Some(2) },
        Op::MaximizeWindowToEdges { id: Some(3) },
        Op::FocusSectionLeft,
        Op::FocusSectionRight,
        Op::FocusSectionRightOrFirst,
        Op::FocusSectionLeftOrLast,
        Op::FocusWindowOrMonitorUp(0),
        Op::FocusWindowOrMonitorDown(1),
        Op::FocusSectionOrMonitorLeft(0),
        Op::FocusSectionOrMonitorRight(1),
        Op::FocusWindowUp,
        Op::FocusWindowUpOrSectionLeft,
        Op::FocusWindowUpOrSectionRight,
        Op::FocusWindowOrWorkspaceUp,
        Op::FocusWindowDown,
        Op::FocusWindowDownOrSectionLeft,
        Op::FocusWindowDownOrSectionRight,
        Op::FocusWindowOrWorkspaceDown,
        Op::MoveSectionLeft,
        Op::MoveSectionRight,
        Op::MoveSectionLeftOrToMonitorLeft(0),
        Op::MoveSectionRightOrToMonitorRight(1),
        Op::ConsumeWindowIntoSection,
        Op::ExpelWindowFromSection,
        Op::CenterSection,
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::FocusWorkspace(1),
        Op::FocusWorkspace(2),
        Op::MoveWindowToWorkspaceDown(true),
        Op::MoveWindowToWorkspaceUp(true),
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 1,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 2,
        },
        Op::MoveSectionToWorkspaceDown(true),
        Op::MoveSectionToWorkspaceUp(true),
        Op::MoveSectionToWorkspace(1, true),
        Op::MoveSectionToWorkspace(2, true),
        Op::MoveWindowDown,
        Op::MoveWindowDownOrToWorkspaceDown,
        Op::MoveWindowUp,
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::ConsumeOrExpelWindowRight { id: None },
        Op::MoveWorkspaceToOutput(1),
        Op::ToggleSectionTabbedDisplay,
        Op::ToggleTabbed,
    ];

    for third in &every_op {
        for second in &every_op {
            for first in &every_op {
                // eprintln!("{first:?}, {second:?}, {third:?}");

                let mut layout = Layout::default();
                first.clone().apply(&mut layout);
                layout.verify_invariants();
                second.clone().apply(&mut layout);
                layout.verify_invariants();
                third.clone().apply(&mut layout);
                layout.verify_invariants();
            }
        }
    }
}

#[test]
fn operations_from_starting_state_dont_panic() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    // Running every op from an empty state doesn't get us to all the interesting states. So,
    // also run it from a manually-created starting state with more things going on to exercise
    // more code paths.
    let setup_ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveWindowToWorkspaceDown(true),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusSectionLeft,
        Op::ConsumeWindowIntoSection,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
        Op::MoveWindowToOutput {
            window_id: None,
            output_id: 2,
            target_ws_idx: None,
        },
        Op::FocusOutput(1),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::Communicate(4),
        Op::Communicate(5),
    ];

    let every_op = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::RemoveOutput(0),
        Op::RemoveOutput(1),
        Op::RemoveOutput(2),
        Op::FocusOutput(0),
        Op::FocusOutput(1),
        Op::FocusOutput(2),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(1),
            layout_config: None,
        },
        Op::UnnameWorkspace { ws_name: 1 },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(6),
            next_to_id: 0,
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(7),
            next_to_id: 1,
        },
        Op::AddWindowToNamedWorkspace {
            params: TestWindowParams::new(5),
            ws_name: 1,
        },
        Op::CloseWindow(0),
        Op::CloseWindow(1),
        Op::CloseWindow(2),
        Op::FullscreenWindow(1),
        Op::FullscreenWindow(2),
        Op::FullscreenWindow(3),
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::MaximizeWindowToEdges { id: Some(2) },
        Op::MaximizeWindowToEdges { id: Some(3) },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: false,
        },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: true,
        },
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: false,
        },
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: true,
        },
        Op::FocusSectionLeft,
        Op::FocusSectionRight,
        Op::FocusSectionRightOrFirst,
        Op::FocusSectionLeftOrLast,
        Op::FocusWindowOrMonitorUp(0),
        Op::FocusWindowOrMonitorDown(1),
        Op::FocusSectionOrMonitorLeft(0),
        Op::FocusSectionOrMonitorRight(1),
        Op::FocusWindowUp,
        Op::FocusWindowUpOrSectionLeft,
        Op::FocusWindowUpOrSectionRight,
        Op::FocusWindowOrWorkspaceUp,
        Op::FocusWindowDown,
        Op::FocusWindowDownOrSectionLeft,
        Op::FocusWindowDownOrSectionRight,
        Op::FocusWindowOrWorkspaceDown,
        Op::MoveSectionLeft,
        Op::MoveSectionRight,
        Op::MoveSectionLeftOrToMonitorLeft(0),
        Op::MoveSectionRightOrToMonitorRight(1),
        Op::ConsumeWindowIntoSection,
        Op::ExpelWindowFromSection,
        Op::CenterSection,
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::FocusWorkspace(1),
        Op::FocusWorkspace(2),
        Op::FocusWorkspace(3),
        Op::MoveWindowToWorkspaceDown(true),
        Op::MoveWindowToWorkspaceUp(true),
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 1,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 2,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 3,
        },
        Op::MoveSectionToWorkspaceDown(true),
        Op::MoveSectionToWorkspaceUp(true),
        Op::MoveSectionToWorkspace(1, true),
        Op::MoveSectionToWorkspace(2, true),
        Op::MoveSectionToWorkspace(3, true),
        Op::MoveWindowDown,
        Op::MoveWindowDownOrToWorkspaceDown,
        Op::MoveWindowUp,
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::ConsumeOrExpelWindowRight { id: None },
        Op::ToggleSectionTabbedDisplay,
        Op::ToggleTabbed,
    ];

    for third in &every_op {
        for second in &every_op {
            for first in &every_op {
                // eprintln!("{first:?}, {second:?}, {third:?}");

                let mut layout = Layout::default();
                for op in &setup_ops {
                    op.clone().apply(&mut layout);
                }

                let mut layout = Layout::default();
                first.clone().apply(&mut layout);
                layout.verify_invariants();
                second.clone().apply(&mut layout);
                layout.verify_invariants();
                third.clone().apply(&mut layout);
                layout.verify_invariants();
            }
        }
    }
}

#[test]
fn primary_active_workspace_idx_not_updated_on_output_add() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
        Op::FocusWorkspace(3),
        Op::AddOutput(2),
    ];

    check_ops(ops);
}

#[test]
fn window_closed_on_previous_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn removing_output_must_keep_empty_focus_on_primary() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    // The workspace from the removed output was inserted at position 0, so the active workspace
    // must change to 1 to keep the focus on the empty workspace.
    assert_eq!(monitors[0].active_workspace_idx, 1);
}

#[test]
fn move_to_workspace_by_idx_does_not_leave_empty_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert!(monitors[0].workspaces[1].has_windows());
}

#[test]
fn empty_workspaces_dont_move_back_to_original_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspace(1),
        Op::CloseWindow(1),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn named_workspaces_dont_update_original_output_on_adding_window() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        // Adding a window updates the original output for unnamed workspaces.
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        // Connecting the previous output should move the named workspace back since its
        // original output wasn't updated.
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output1");
}

#[test]
fn workspaces_update_original_output_on_moving_to_same_output() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToOutput(2),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}

#[test]
fn workspaces_update_original_output_on_moving_to_same_monitor() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(1),
            output_id: 2,
        },
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}

#[test]
fn large_negative_height_change() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::AdjustProportion(-1e129),
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}

#[test]
fn large_max_size() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
                ..TestWindowParams::new(1)
            },
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}

#[test]
fn workspace_cleanup_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceDown,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch_from_last() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch_gets_cleaned_up() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(2),
        Op::MoveSectionToWorkspaceDown(true),
        Op::MoveSectionToWorkspaceDown(true),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn move_workspace_to_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::MoveWorkspaceToOutput(2),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal {
        monitors,
        active_monitor_idx,
        ..
    } = layout.monitor_set
    else {
        unreachable!()
    };

    assert_eq!(active_monitor_idx, 1);
    assert_eq!(monitors[0].workspaces.len(), 1);
    assert!(!monitors[0].workspaces[0].has_windows());
    assert_eq!(monitors[1].active_workspace_idx, 0);
    assert_eq!(monitors[1].workspaces.len(), 2);
    assert!(monitors[1].workspaces[0].has_windows());
}

#[test]
fn open_right_of_on_different_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 1,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[0].scrolling().active_section_idx(),
        1,
        "the new window must become active"
    );
}

#[test]
// empty_workspace_above_first = true
fn open_right_of_on_different_workspace_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 2,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[1].scrolling().active_section_idx(),
        1,
        "the new window must become active"
    );
}

#[test]
fn removing_all_outputs_preserves_empty_named_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddNamedWorkspace {
            ws_name: 2,
            output_name: None,
            layout_config: None,
        },
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::NoOutputs { workspaces } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(workspaces.len(), 2);
}

#[test]
fn config_change_updates_cached_sizes() {
    let mut config = Config::default();
    let border = &mut config.layout.border;
    border.off = false;
    border.width = 2.;

    let mut layout = Layout::new(Clock::default(), &config);

    Op::AddWindow {
        params: TestWindowParams {
            bbox: Rectangle::from_size(Size::from((1280, 200))),
            ..TestWindowParams::new(1)
        },
    }
    .apply(&mut layout);

    config.layout.border.width = 4.;
    layout.update_config(&config);

    layout.verify_invariants();
}

#[test]
fn preset_height_change_removes_preset() {
    let mut config = Config::default();
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1), PresetSize::Fixed(2)];

    let mut layout = Layout::new(Clock::default(), &config);

    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SwitchPresetWindowHeight { id: None },
        Op::SwitchPresetWindowHeight { id: None },
    ];
    for op in ops {
        op.apply(&mut layout);
    }

    // Leave only one.
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1)];

    layout.update_config(&config);

    layout.verify_invariants();
}

#[test]
fn set_window_height_recomputes_to_auto() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        Op::FocusWindowUp,
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
    ];

    check_ops(ops);
}

#[test]
fn one_window_in_section_becomes_weight_1() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        Op::Communicate(2),
        Op::FocusWindowUp,
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        Op::Communicate(1),
        Op::CloseWindow(0),
        Op::CloseWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn fixed_height_takes_max_non_auto_into_account() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SetWindowHeight {
            id: Some(0),
            change: SizeChange::SetFixed(704),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: niri_config::Layout {
            border: niri_config::Border {
                off: false,
                width: 4.,
                ..Default::default()
            },
            gaps: 0.,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn start_interactive_move_then_remove_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::CloseWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_onto_empty_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_onto_empty_output_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn interactive_move_onto_last_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceDown,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_onto_first_empty_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceUp,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 1 },
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn output_active_workspace_is_preserved() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 1);
}

#[test]
fn output_active_workspace_is_preserved_with_other_outputs() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[1].active_workspace_idx, 1);
}

#[test]
fn named_workspace_to_output() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::MoveWorkspaceToOutput(1),
        Op::FocusWorkspaceUp,
    ];
    check_ops(ops);
}

#[test]
// empty_workspace_above_first = true
fn named_workspace_to_output_ewaf() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn move_window_to_empty_workspace_above_first() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveWorkspaceUp,
        Op::MoveWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceDown,
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn move_window_to_different_output() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::MoveWorkspaceToOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn close_window_empty_ws_above_first() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::CloseWindow(1),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn add_and_remove_output() {
    let ops = [
        Op::AddOutput(2),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn switch_ewaf_on() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let mut layout = check_ops(ops);
    layout.update_options(Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    });
    layout.verify_invariants();
}

#[test]
fn switch_ewaf_off() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);
    layout.update_options(Options::default());
    layout.verify_invariants();
}

#[test]
fn interactive_move_drop_on_other_output_during_animation() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::InteractiveMoveBegin {
            window: 3,
            output_idx: 3,
            px: 0.0,
            py: 0.0,
        },
        Op::FocusWorkspaceDown,
        Op::AddOutput(4),
        Op::InteractiveMoveUpdate {
            window: 3,
            dx: 0.0,
            dy: 8300.68619826683,
            output_idx: 4,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(4),
        Op::InteractiveMoveEnd { window: 3 },
    ];
    check_ops(ops);
}

#[test]
fn add_window_next_to_only_interactively_moved_without_outputs() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(1),
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(1),
        // We have no outputs, and the only existing window is interactively moved, meaning there
        // are no workspaces either.
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 2,
        },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_toggle_floating_ends_dnd_gesture() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::Refresh { is_active: false },
        Op::ToggleWindowFloating { id: None },
        Op::InteractiveMoveEnd { window: 2 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_from_workspace_with_layout_config() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: Some(Box::new(niri_config::LayoutPart {
                border: Some(niri_config::BorderRule {
                    on: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now remove and add the output. It will have the same workspace.
        Op::RemoveOutput(1),
        Op::AddOutput(1),
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now move onto a different workspace.
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
    ];

    check_ops(ops);
}

#[test]
fn in_place_drag_to_floating_merges_workspace_layout_config() {
    // Regression: the in-place (sway) drag's float hand-off detaches the source into a `Moving`
    // follow via `detach_inplace_into_move`. That detach must merge the *source workspace's* layout
    // override into the detached tile's options, exactly like the classic Starting→Moving detach
    // path — otherwise the live `Moving` tile carries the wrong border/gap options and
    // `verify_invariants` (which asserts the merged form for a `Moving` tile) fails after the toggle.
    let ops = [
        // Named workspace with a border override; window 2 lands on it (mirrors the setup of
        // `interactive_move_from_workspace_with_layout_config`).
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: Some(Box::new(niri_config::LayoutPart {
                border: Some(niri_config::BorderRule {
                    on: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::Communicate(2),
        Op::AdvanceAnimations { msec_delta: 2000 },
        // Enter the in-place drag and move past the detach threshold (the source stays in the tree).
        Op::InteractiveMoveBegin { window: 2, output_idx: 1, px: 66., py: 356. },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 400.,
            dy: 0.,
            output_idx: 1,
            px: 466.,
            py: 356.,
        },
        // Hand off to floating: detaches into a `Moving` tile whose options must include the
        // workspace override. verify_invariants (run after each op) asserts the merged form.
        Op::ToggleWindowFloating { id: Some(2) },
        Op::AdvanceAnimations { msec_delta: 1000 },
    ];
    check_ops(ops);
}

#[test]
fn set_width_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetSectionWidth(SizeChange::SetFixed(-100)),
    ];
    check_ops(ops);
}

#[test]
fn set_height_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(-100),
        },
    ];
    check_ops(ops);
}

#[test]
fn interactive_resize_to_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::InteractiveResizeBegin {
            window: 3,
            edges: ResizeEdge::BOTTOM_RIGHT,
        },
        Op::InteractiveResizeUpdate {
            window: 3,
            dx: -10000.,
            dy: -10000.,
        },
    ];
    check_ops(ops);
}

#[test]
fn windows_on_other_workspaces_remain_activated() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspaceDown,
        Op::Refresh { is_active: true },
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();
    assert!(win.0.pending_activated.get());
}

#[test]
fn stacking_add_parent_brings_up_child() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(1),
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
    ];

    check_ops(ops);
}

#[test]
fn stacking_add_parent_brings_up_descendants() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(2),
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(0),
                ..TestWindowParams::new(1)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
    ];

    check_ops(ops);
}

#[test]
fn stacking_activate_brings_up_descendants() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(0),
                ..TestWindowParams::new(1)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(1),
                ..TestWindowParams::new(2)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::FocusWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn stacking_set_parent_brings_up_child() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::SetParent {
            id: 0,
            new_parent_id: Some(1),
        },
    ];

    check_ops(ops);
}

#[test]
fn move_window_to_workspace_with_different_active_output() {
    let ops = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
        },
    ];

    check_ops(ops);
}

#[test]
fn set_first_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}

#[test]
fn set_first_workspace_name_ewaf() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn set_last_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}

#[test]
fn move_workspace_to_same_monitor_doesnt_reorder() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(0),
            output_id: 0,
        },
    ];

    let layout = check_ops(ops);
    let counts: Vec<_> = layout
        .workspaces()
        .map(|(_, _, ws)| ws.windows().count())
        .collect();
    assert_eq!(counts, &[1, 2, 0]);
}

#[test]
fn removing_window_above_preserves_focused_window() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusSectionFirst,
        Op::ConsumeWindowIntoSection,
        Op::ConsumeWindowIntoSection,
        Op::FocusWindowDown,
        Op::CloseWindow(0),
    ];

    let layout = check_ops(ops);
    let win = layout.focus().unwrap();
    assert_eq!(win.0.id, 1);
}

#[test]
fn preset_section_width_fixed_correct_with_border() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetSectionWidth,
    ];

    let options = Options {
        layout: niri_config::Layout {
            preset_section_widths: vec![PresetSize::Fixed(500)],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);

    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);

    // Add border.
    let options = Options {
        layout: niri_config::Layout {
            preset_section_widths: vec![PresetSize::Fixed(500)],
            border: niri_config::Border {
                off: false,
                width: 5.,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    layout.update_options(options);

    // With border, the window gets less size.
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 490);

    // However, preset fixed width will still work correctly.
    layout.toggle_width(true);
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);
}

#[test]
fn preset_section_width_reset_after_set_width() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetSectionWidth,
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::AdjustFixed(-10),
        },
        Op::SwitchPresetSectionWidth,
    ];

    let options = Options {
        layout: niri_config::Layout {
            preset_section_widths: vec![PresetSize::Fixed(500), PresetSize::Fixed(1000)],
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);
}

#[test]
fn move_section_to_workspace_unfocused_with_multiple_monitors() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 101,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 102,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::SetWorkspaceName {
            new_ws_name: 201,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MoveSectionToOutput {
            output_id: 1,
            target_ws_idx: Some(0),
            activate: false,
        },
        Op::FocusOutput(1),
    ];

    let layout = check_ops(ops);

    assert_eq!(layout.active_workspace().unwrap().name().unwrap(), "ws102");

    for (mon, win) in layout.windows() {
        let mon = mon.unwrap();
        let ws = mon
            .workspaces
            .iter()
            .find(|w| w.has_window(win.id()))
            .unwrap();

        assert_eq!(
            ws.name().unwrap(),
            match win.id() {
                1 | 4 => "ws101",
                2 => "ws102",
                3 => "ws201",
                _ => unreachable!(),
            }
        );
    }
}

#[test]
fn move_section_to_workspace_down_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveSectionToWorkspaceDown(false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}

#[test]
fn move_section_to_workspace_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveSectionToWorkspace(1, false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}

#[test]
fn restore_to_floating_persists_across_fullscreen_maximize() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        // Maximize then fullscreen.
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        // Unfullscreen.
        Op::FullscreenWindow(1),
    ];

    let mut layout = check_ops(ops);

    // Unfullscreening should return the window to the maximized state.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_some());

    let ops = [
        // Unmaximize.
        Op::MaximizeWindowToEdges { id: None },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Unmaximize should return the window back to floating.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_none());
}

#[test]
fn unmaximize_during_fullscreen_does_not_float() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        // Maximize then fullscreen.
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        // Unmaximize.
        Op::MaximizeWindowToEdges { id: None },
    ];

    let mut layout = check_ops(ops);

    // Unmaximize shouldn't have changed the window state since it's fullscreen.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_some());

    let ops = [
        // Unfullscreen.
        Op::FullscreenWindow(1),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Unfullscreen should return the window back to floating.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_none());
}

#[test]
fn move_section_to_workspace_maximize_and_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        Op::MoveSectionToWorkspaceDown(true),
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();

    // Unfullscreening should return to maximized because the window was maximized before.
    assert_eq!(win.pending_sizing_mode(), SizingMode::Maximized);
}

#[test]
fn move_window_to_workspace_maximize_and_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        Op::MoveWindowToWorkspaceDown(true),
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();

    // Unfullscreening should return to maximized because the window was maximized before.
    //
    // FIXME: it currently doesn't because windows themselves can only be either fullscreen or
    // maximized. So when a window is fullscreen, whether it is also maximized or not is stored in
    // the section. MoveWindowToWorkspace removes the window from the section and this information is
    // forgotten.
    assert_eq!(win.pending_sizing_mode(), SizingMode::Normal);
}

#[test]
fn tabs_with_different_border() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                rules: Some(ResolvedWindowRules {
                    border: niri_config::BorderRule {
                        on: true,
                        ..Default::default()
                    },
                    ..ResolvedWindowRules::default()
                }),
                ..TestWindowParams::new(2)
            },
        },
        Op::SwitchPresetWindowHeight { id: None },
        Op::ToggleSectionTabbedDisplay,
        Op::ToggleTabbed,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: niri_config::Layout {
            struts: Struts {
                left: FloatOrInt(0.),
                right: FloatOrInt(0.),
                top: FloatOrInt(20000.),
                bottom: FloatOrInt(0.),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn expel_pending_left_from_fullscreen_tabbed_section() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        // 1 is now fullscreen, view_offset_to_restore is set.
        Op::ToggleSectionTabbedDisplay,
        Op::ToggleTabbed,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: Some(2) },
        // 2 is consumed into a fullscreen section, fullscreen is requested but not applied.
        //
        // Now, get it back out while keeping it focused.
        //
        // Importantly, we expel it *left*, which results in adding a new section with the exact
        // same active_section_idx.
        Op::FocusWindow(2),
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    check_ops(ops);
}

#[test]
fn workspace_render_geo_at_fractional_scale() {
    let ops = [
        Op::AddScaledOutput {
            id: 1,
            scale: 1.1,
            layout_config: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };

    let mon = &monitors[0];
    let mut iter = mon.workspaces_with_render_geo();
    let (_ws, geo) = iter.next().unwrap();
    assert!(
        iter.next().is_none(),
        "animations are completed, only one workspace should be visible"
    );
    assert_eq!(
        geo.loc.y, 0.,
        "active workspace must be at y = 0 exactly, \
         otherwise a pointer against the screen edge at y = 0 won't hit it"
    );
}

fn parent_id_causes_loop(layout: &Layout<TestWindow>, id: usize, mut parent_id: usize) -> bool {
    if parent_id == id {
        return true;
    }

    'outer: loop {
        for (_, win) in layout.windows() {
            if win.0.id == parent_id {
                match win.0.parent_id.get() {
                    Some(new_parent_id) => {
                        if new_parent_id == id {
                            // Found a loop.
                            return true;
                        }

                        parent_id = new_parent_id;
                        continue 'outer;
                    }
                    // Reached window with no parent.
                    None => return false,
                }
            }
        }

        // Parent is not in the layout.
        return false;
    }
}

fn arbitrary_spacing() -> impl Strategy<Value = f64> {
    // Give equal weight to:
    // - 0: the element is disabled
    // - 4: some reasonable value
    // - random value, likely unreasonably big
    prop_oneof![Just(0.), Just(4.), ((1.)..=65535.)]
}

fn arbitrary_spacing_neg() -> impl Strategy<Value = f64> {
    // Give equal weight to:
    // - 0: the element is disabled
    // - 4: some reasonable value
    // - -4: some reasonable negative value
    // - random value, likely unreasonably big
    prop_oneof![Just(0.), Just(4.), Just(-4.), ((1.)..=65535.)]
}

fn arbitrary_struts() -> impl Strategy<Value = Struts> {
    (
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
    )
        .prop_map(|(left, right, top, bottom)| Struts {
            left: FloatOrInt(left),
            right: FloatOrInt(right),
            top: FloatOrInt(top),
            bottom: FloatOrInt(bottom),
        })
}

fn arbitrary_center_focused_section() -> impl Strategy<Value = CenterFocusedSection> {
    prop_oneof![
        Just(CenterFocusedSection::Never),
        Just(CenterFocusedSection::OnOverflow),
        Just(CenterFocusedSection::Always),
    ]
}

fn arbitrary_tab_indicator_position() -> impl Strategy<Value = TabIndicatorPosition> {
    prop_oneof![
        Just(TabIndicatorPosition::Left),
        Just(TabIndicatorPosition::Right),
        Just(TabIndicatorPosition::Top),
        Just(TabIndicatorPosition::Bottom),
    ]
}

prop_compose! {
    fn arbitrary_focus_ring()(
        off in any::<bool>(),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::BorderRule {
        niri_config::BorderRule {
            off,
            on: !off,
            width,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_border()(
        off in any::<bool>(),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::BorderRule {
        niri_config::BorderRule {
            off,
            on: !off,
            width,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_shadow()(
        off in any::<bool>(),
        softness in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::ShadowRule {
        niri_config::ShadowRule {
            off,
            on: !off,
            softness,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_tab_indicator()(
        off in any::<bool>(),
        hide_when_single_tab in prop::option::of(any::<bool>().prop_map(Flag)),
        place_within_section in prop::option::of(any::<bool>().prop_map(Flag)),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
        gap in prop::option::of(arbitrary_spacing_neg().prop_map(FloatOrInt)),
        length in prop::option::of((0f64..2f64)
            .prop_map(|x| TabIndicatorLength { total_proportion: Some(x) })),
        position in prop::option::of(arbitrary_tab_indicator_position()),
    ) -> niri_config::TabIndicatorPart {
        niri_config::TabIndicatorPart {
            off,
            on: !off,
            hide_when_single_tab,
            place_within_section,
            width,
            gap,
            length,
            position,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_layout_part()(
        gaps in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
        struts in prop::option::of(arbitrary_struts()),
        focus_ring in prop::option::of(arbitrary_focus_ring()),
        border in prop::option::of(arbitrary_border()),
        shadow in prop::option::of(arbitrary_shadow()),
        tab_indicator in prop::option::of(arbitrary_tab_indicator()),
        center_focused_section in prop::option::of(arbitrary_center_focused_section()),
        always_center_single_section in prop::option::of(any::<bool>().prop_map(Flag)),
        empty_workspace_above_first in prop::option::of(any::<bool>().prop_map(Flag)),
        tiling_drag in prop::option::of(prop::sample::select(vec![
            niri_config::TilingDrag::Detach,
            niri_config::TilingDrag::InPlace,
        ])),
    ) -> niri_config::LayoutPart {
        niri_config::LayoutPart {
            gaps,
            struts,
            center_focused_section,
            always_center_single_section,
            empty_workspace_above_first,
            focus_ring,
            border,
            shadow,
            tab_indicator,
            tiling_drag,
            ..Default::default()
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if std::env::var_os("RUN_SLOW_TESTS").is_none() {
            eprintln!("ignoring slow test");
            0
        } else {
            ProptestConfig::default().cases
        },
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_operations_dont_panic(
        ops: Vec<Op>,
        layout_config in arbitrary_layout_part(),
    ) {
        // eprintln!("{ops:?}");
        let options = Options {
            layout: niri_config::Layout::from_part(&layout_config),
            ..Default::default()
        };

        check_ops_with_options(options, ops);
    }
}

// ============================================================================
// Dedicated unit tests for splits and tabs
// ============================================================================

/// Helper: get the render position and size of a window by ID.
fn window_geo(layout: &Layout<TestWindow>, id: usize) -> Option<(Point<f64, Logical>, Size<f64, Logical>)> {
    let ws = layout.active_workspace().unwrap();
    ws.tiles_with_render_positions().find_map(|(tile, pos, _)| {
        if *tile.window().id() == id {
            Some((pos, tile.window().size().to_f64()))
        } else {
            None
        }
    })
}

/// Helper: count tiles in the active workspace.
fn tile_count(layout: &Layout<TestWindow>) -> usize {
    layout.active_workspace().unwrap().tiles().count()
}

/// Helper: whether the window with the given id is currently rendered (visible). Returns None if
/// the window isn't present.
fn window_visible(layout: &Layout<TestWindow>, id: usize) -> Option<bool> {
    let ws = layout.active_workspace().unwrap();
    ws.tiles_with_render_positions()
        .find_map(|(tile, _, visible)| (*tile.window().id() == id).then_some(visible))
}

/// Helper: window ids in tile (tree/leaf) order across the active workspace.
fn window_order(layout: &Layout<TestWindow>) -> Vec<usize> {
    let ws = layout.active_workspace().unwrap();
    ws.tiles().map(|tile| *tile.window().id()).collect()
}

/// Helper: the id of the currently active (focused) window in the active workspace.
fn active_window_id(layout: &Layout<TestWindow>) -> Option<usize> {
    layout
        .active_workspace()
        .unwrap()
        .active_window()
        .map(|w| *w.id())
}

/// Helper: locate a window anywhere in the layout (across every output/workspace), returning its
/// output name and its output-local render position + size. Unlike `window_geo`, this isn't limited
/// to the active workspace, so it works for cross-output assertions.
fn window_output_and_geo(
    layout: &Layout<TestWindow>,
    id: usize,
) -> Option<(String, Point<f64, Logical>, Size<f64, Logical>)> {
    layout.workspaces().find_map(|(mon, _, ws)| {
        ws.tiles_with_render_positions().find_map(|(tile, pos, _)| {
            if *tile.window().id() == id {
                Some((
                    mon.map(|m| m.output_name().clone()).unwrap_or_default(),
                    pos,
                    tile.window().size().to_f64(),
                ))
            } else {
                None
            }
        })
    })
}

/// Helper: the pending sizing mode of a window found anywhere in the layout.
fn window_sizing_mode(layout: &Layout<TestWindow>, id: usize) -> Option<SizingMode> {
    layout
        .windows()
        .find(|(_, w)| *w.id() == id)
        .map(|(_, w)| w.pending_sizing_mode())
}

#[test]
fn split_window_creates_side_by_side_tiles() {
    // SplitWindow + AddWindow should create a main-axis split with two side-by-side tiles.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
    ]);

    // Both windows should be in the same section.
    assert_eq!(tile_count(&layout), 2);

    // The two windows should be side by side (different x positions).
    let (pos1, _) = window_geo(&layout, 1).unwrap();
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    assert_ne!(pos1.x, pos2.x, "windows should be at different x positions");
}

#[test]
fn consume_window_into_split_places_side_by_side() {
    // ConsumeWindowIntoSplit should pull a window from an adjacent section and place it
    // side by side with the focused window.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::AddWindow { params: TestWindowParams::new(2) },
        // Now we have two sections, each with one window.
        Op::ConsumeWindowIntoSplit,
    ]);

    // Both windows should still be present.
    assert_eq!(tile_count(&layout), 2);

    // They should be side by side within one section.
    let (pos1, _) = window_geo(&layout, 1).unwrap();
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    assert_ne!(pos1.x, pos2.x, "consumed window should be side by side");
}

#[test]
fn toggle_tabbed_hides_inactive_tiles() {
    // Build a real two-window section (cross split), then tab it. ToggleTabbed should show only the
    // active tile and hide the rest.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::ToggleTabbed,
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    // Both windows should still be in the layout.
    assert_eq!(tile_count(&layout), 2);

    // In tabbed mode exactly one tile is visible at a time, and it's the active one (window 2,
    // the last added). The inactive tab must be hidden.
    assert_eq!(window_visible(&layout, 2), Some(true), "active tab should be visible");
    assert_eq!(window_visible(&layout, 1), Some(false), "inactive tab should be hidden");

    // Both tabs occupy the same position (stacked on top of each other).
    let (pos1, _) = window_geo(&layout, 1).unwrap();
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    assert_eq!(pos1, pos2, "tabbed tiles should share the same position");
}

#[test]
fn spatial_focus_resolves_screen_direction_per_orientation() {
    // On a landscape monitor the strip runs horizontally, so screen-left moves between sections.
    let mut h = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::AddWindow { params: TestWindowParams::new(2) },
    ]);
    assert_eq!(active_window_id(&h), Some(2));
    h.focus_screen_left();
    assert_eq!(
        active_window_id(&h),
        Some(1),
        "screen-left walks the horizontal strip on a landscape monitor"
    );
    h.focus_screen_right();
    assert_eq!(active_window_id(&h), Some(2), "screen-right walks back");
    // Move the focused window (2) screen-left: it swaps past window 1, giving order [2, 1].
    h.move_screen_left();
    assert_eq!(window_order(&h), vec![2, 1], "screen-left move reorders the strip");

    // On a portrait monitor the strip runs vertically: screen-UP walks the strip, while
    // screen-left/right stay within a section (cross axis). This is the case that was confusing with
    // the old logical section/window binds.
    let mut options = Options::default();
    options.layout.main_axis = MainAxis::Vertical;
    let mut v = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: TestWindowParams::new(1) },
            Op::AddWindow { params: TestWindowParams::new(2) },
        ],
    );
    assert_eq!(active_window_id(&v), Some(2));
    v.focus_screen_left();
    assert_eq!(
        active_window_id(&v),
        Some(2),
        "screen-left is within-section (no sibling here) on a portrait monitor"
    );
    v.focus_screen_up();
    assert_eq!(
        active_window_id(&v),
        Some(1),
        "screen-up walks the vertical strip on a portrait monitor"
    );
}

#[test]
fn toggle_split_layout_flips_row_and_section() {
    // A side-by-side row [1 | 2]; toggle split → a vertical section [1 / 2]; toggle again → row.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(2) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, _) = window_geo(&layout, 2).unwrap();
    assert_ne!(p1.x, p2.x, "starts as a row (different x)");

    layout.toggle_split_layout();
    check_ops_on_layout(
        &mut layout,
        [Op::Communicate(1), Op::Communicate(2), Op::AdvanceAnimations { msec_delta: 1000 }],
    );
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, _) = window_geo(&layout, 2).unwrap();
    assert_eq!(p1.x, p2.x, "after toggle: a section (shared x)");
    assert_ne!(p1.y, p2.y, "after toggle: stacked (different y)");

    layout.toggle_split_layout();
    check_ops_on_layout(
        &mut layout,
        [Op::Communicate(1), Op::Communicate(2), Op::AdvanceAnimations { msec_delta: 1000 }],
    );
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, _) = window_geo(&layout, 2).unwrap();
    assert_ne!(p1.x, p2.x, "toggled back to a row");
}

#[test]
fn stacked_root_reserves_one_row_per_child_not_per_leaf() {
    use super::tile_node::Layout;

    // Flat: Stacked[1, 2] — two direct children → two title rows.
    let flat = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SetLayout(Layout::Stacked),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    // Nested: Stacked[1, SplitH[2,3]] — still two direct children, so it must reserve TWO rows,
    // not three (one per leaf). Regression for the root using leaf-count.
    let nested = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::FocusWindow(1),
        Op::SetLayout(Layout::Stacked),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    // Window 1 sits at the content top in both; equal y means equal reserved header bands.
    let (flat1, _) = window_geo(&flat, 1).unwrap();
    let (nested1, _) = window_geo(&nested, 1).unwrap();
    assert_eq!(
        flat1.y, nested1.y,
        "nested stacked root must reserve per-child rows like the flat one \
         (flat y={}, nested y={})",
        flat1.y, nested1.y
    );
}

#[test]
fn stacked_layout_reserves_a_row_per_tab_and_shows_one() {
    use super::tile_node::Layout;

    let build = |layout: Layout| {
        check_ops([
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(3) },
            Op::SetLayout(layout),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ])
    };

    let tabbed = build(Layout::Tabbed);
    let stacked = build(Layout::Stacked);

    // A stacked container shows only its active child (window 3, the last added).
    assert_eq!(window_visible(&stacked, 3), Some(true), "active stacked child visible");
    assert_eq!(window_visible(&stacked, 1), Some(false), "inactive stacked child hidden");
    assert_eq!(window_visible(&stacked, 2), Some(false), "inactive stacked child hidden");

    // Stacked reserves one title row per tab (3 rows) vs tabbed's single row, so the active
    // content sits lower.
    let (t3, _) = window_geo(&tabbed, 3).unwrap();
    let (s3, _) = window_geo(&stacked, 3).unwrap();
    assert!(
        s3.y > t3.y + 10.,
        "stacked reserves more header rows than tabbed (tabbed y={}, stacked y={})",
        t3.y,
        s3.y
    );
}

#[test]
fn set_window_height_converges_across_flat_and_nested() {
    // A fixed window height must resolve to the same *window* size whether the leaf lives in a flat
    // cross section (flat sizing path) or inside a nested split (recursive sizing path). Both store
    // the span as a *tile* span, so the decoration delta is accounted for exactly once, at the
    // storage site — the nested case previously sized the tile to the window span and came out short
    // by the border delta.
    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 2.;

    // Flat: a plain cross section [1 / 2]. Resize window 1 to a fixed height.
    let flat = check_ops_with_options(
        options.clone(),
        [
            Op::AddOutput(1),
            Op::AddWindow { params: TestWindowParams::new(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: TestWindowParams::new(2) },
            Op::SetWindowHeight { id: Some(1), change: SizeChange::SetFixed(300) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let (_, flat_size) = window_geo(&flat, 1).unwrap();

    // Nested: a main split whose second child is a nested cross split [2 / 3]. The root is a Main
    // split, so the section is laid out by the recursive path. Resize window 2 (inside the nested
    // cross split) to the same fixed height.
    let nested = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: TestWindowParams::new(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: TestWindowParams::new(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: TestWindowParams::new(3) },
            Op::SetWindowHeight { id: Some(2), change: SizeChange::SetFixed(300) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let (_, nested_size) = window_geo(&nested, 2).unwrap();

    assert!(
        (flat_size.h - 300.).abs() <= 1.,
        "flat window height should resolve to ~300, got {}",
        flat_size.h
    );
    assert!(
        (nested_size.h - flat_size.h).abs() <= 1.,
        "nested window height {} should match the flat one {}",
        nested_size.h,
        flat_size.h
    );
}

#[test]
fn tabbing_a_resized_row_does_not_force_a_bogus_height() {
    // A horizontal row sizes its children along the main axis (a fixed child is a *width*). Tabbing
    // it flips the resize axis to cross (a fixed child would be a *height*), so any leftover fixed
    // width must be reset to Auto rather than silently reinterpreted as a height. Otherwise the
    // tabbed windows get forced to a bogus height derived from the old width.
    let build = |resize: bool| {
        let mut ops = vec![
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
        ];
        if resize {
            // Pin window 1 to a fixed *width* within the row.
            ops.push(Op::SetWindowWidth { id: Some(1), change: SizeChange::SetFixed(400) });
        }
        ops.extend([
            Op::ToggleTabbed,
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ]);
        check_ops(ops)
    };

    let control = build(false);
    let resized = build(true);

    let (_, control_size) = window_geo(&control, 1).unwrap();
    let (_, resized_size) = window_geo(&resized, 1).unwrap();
    assert!(
        (resized_size.h - control_size.h).abs() <= 1.,
        "a width-resized row that is tabbed must get the same (full) height as an un-resized one \
         (control h={}, resized h={})",
        control_size.h,
        resized_size.h
    );
}

#[test]
fn height_conversion_keeps_finite_weights() {
    // `convert_heights_to_auto` divides each slot height by the median; a degenerate (zero /
    // non-finite) median or height would yield a NaN weight, which `verify_invariants` (run after
    // every op below) now rejects. Stress the conversion path with varied and tiny heights across a
    // nested tree and confirm every weight stays finite.
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        // Pin one to a tiny fixed height, then convert everything back to auto (SetWindowHeight on an
        // auto leaf runs `convert_heights_to_auto` first).
        Op::SetWindowHeight { id: Some(2), change: SizeChange::SetFixed(1) },
        Op::SetWindowHeight { id: Some(1), change: SizeChange::SetProportion(90.) },
        Op::ResetWindowHeight { id: Some(1) },
        Op::SetWindowHeight { id: Some(3), change: SizeChange::AdjustFixed(-100000) },
        Op::ResetWindowHeight { id: Some(2) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
}

#[test]
fn nested_height_resize_ignores_horizontal_siblings_min() {
    // In SplitH[1, SplitV[2, 3]], resizing window 2's height competes for cross space only with its
    // cross sibling (window 3), not with window 1, which sits beside the whole vertical split and
    // gets the full cross extent independently. A large min *height* on window 1 must therefore not
    // restrict how tall window 2 can be made. (Regression: the clamp used to sum every leaf flat.)
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                // Window 1: a big minimum height, but it's a horizontal sibling of the split.
                min_max_size: (Size::from((0, 500)), Size::from((0, 0))),
                ..TestWindowParams::new(1)
            },
        },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SetWindowHeight { id: Some(2), change: SizeChange::SetFixed(400) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    let (_, size2) = window_geo(&layout, 2).unwrap();
    assert!(
        (size2.h - 400.).abs() <= 5.,
        "window 2 should reach its requested height ~400 (its only cross competitor is window 3); \
         got {} — a horizontal sibling's min height must not clamp it",
        size2.h
    );
}

#[test]
fn stacked_active_child_fills_below_header_no_bottom_loss() {
    use super::tile_node::Layout;

    // Reference: a single plain window fills the whole section; its bottom edge is the section's
    // content bottom.
    let plain = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::Communicate(1),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let (p_pos, p_size) = window_geo(&plain, 1).unwrap();
    let ref_bottom = p_pos.y + p_size.h;

    // Flat Stacked root (non-recursive sizing path): two leaf children.
    let flat = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SetLayout(Layout::Stacked),
        Op::FocusWindow(1),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let (f_pos, f_size) = window_geo(&flat, 1).unwrap();
    let flat_bottom = f_pos.y + f_size.h;

    // Nested Stacked root (recursive sizing path): one leaf + a horizontal pair, so the root has a
    // nested child and goes through `update_tile_sizes_recursive`. There the header band was being
    // double-counted — subtracted in the available size *and* again in `request_sizes` — so the
    // active child lost height at the bottom as well as the (correct) header offset at the top.
    let nested = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::FocusWindow(1),
        Op::SetLayout(Layout::Stacked),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let (n_pos, n_size) = window_geo(&nested, 1).unwrap();
    let nested_bottom = n_pos.y + n_size.h;

    // The active child starts *below* the reserved header band (top is correctly offset).
    assert!(
        n_pos.y > p_pos.y + 1.,
        "stacked active child should start below the header band (plain y={}, nested y={})",
        p_pos.y,
        n_pos.y
    );

    // ...and fills all the way to the section bottom (no missing strip at the bottom).
    assert!(
        (flat_bottom - ref_bottom).abs() < 1.,
        "flat stacked active child must reach the section bottom (ref={ref_bottom}, flat={flat_bottom})"
    );
    assert!(
        (nested_bottom - ref_bottom).abs() < 1.,
        "nested stacked active child must reach the section bottom — the header band is reserved at \
         the top only, not double-counted at the bottom (ref={ref_bottom}, nested={nested_bottom})"
    );
}

#[test]
fn group_tab_reports_subtree_union_size_and_group_label() {
    use super::tile_node::Layout;

    // Root = Stacked[ 1, SplitH[2,3] ]: tab 0 is a single leaf, tab 1 is a horizontal group.
    let nested = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::FocusWindow(1),
        Op::SetLayout(Layout::Stacked),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    // The horizontal pair: window 2 (left) and window 3 (right), side by side.
    let (w2_pos, _) = window_geo(&nested, 2).unwrap();
    let (w3_pos, w3_size) = window_geo(&nested, 3).unwrap();
    let pair_union_w = (w3_pos.x + w3_size.w) - w2_pos.x;

    let ws = nested.active_workspace().unwrap();
    let infos = ws.scrolling().tab_child_infos(&[]);
    assert_eq!(infos.len(), 2, "root Stacked has two direct children → two tabs");

    let (g0, t0) = &infos[0]; // leaf 1
    let (g1, t1) = &infos[1]; // SplitH[2,3]

    // Bug #4: the group tab's geometry is the *union* of windows 2 and 3, not one descendant leaf.
    // With the bug it would report a single leaf's size (≈ one window), narrower than the pair.
    assert!(
        g1.size.w > g0.size.w + 100.,
        "group tab must span both windows of the subtree, not one leaf: leaf w={}, group w={}",
        g0.size.w,
        g1.size.w
    );
    assert!(
        (g1.size.w - pair_union_w).abs() < 1.,
        "group tab width must equal the union of windows 2 and 3: union={pair_union_w}, group w={}",
        g1.size.w
    );

    // Bug #3: the group tab carries the sway/i3 tree representation (layout glyph + the children's
    // titles in brackets), not a borrowed single-window title; the leaf tab keeps its own title.
    assert_eq!(
        t1, "H[app2 app3]",
        "group tab is the sway-style tree repr with compact app-id child identifiers"
    );
    assert_eq!(t0, "win1", "the leaf tab keeps its own (verbose) window title");
}

#[test]
fn toggle_tabbed_is_family_aware() {
    // Toggling a vertical (cross) stack tabs into Stacked (one title row per child, stacked down);
    // toggling a horizontal (main) row tabs into Tabbed (one row of side-by-side titles). Matches
    // sway's two tab styles.
    let build = |dir: niri_ipc::SplitDirection| {
        check_ops([
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(dir),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(dir),
            Op::AddWindow { params: wide_window(3) },
            Op::ToggleTabbed,
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ])
    };
    let vstack = build(niri_ipc::SplitDirection::Cross);
    let hstack = build(niri_ipc::SplitDirection::Main);

    // The active child of the toggled vstack sits lower (3 stacked title rows) than the toggled
    // hstack (a single tab row).
    let (v, _) = window_geo(&vstack, 3).unwrap();
    let (h, _) = window_geo(&hstack, 3).unwrap();
    assert!(
        v.y > h.y + 10.,
        "vstack should toggle to Stacked (more rows) and hstack to Tabbed (one row): \
         vstack y={}, hstack y={}",
        v.y,
        h.y
    );
}

#[test]
fn simplify_merges_same_family_splits_only() {
    use std::rc::Rc;

    use super::tile_node::{Layout as L, SplitChildData, TileNode as TN};

    let leaf = |id: usize| -> TN<TestWindow> {
        let win = TestWindow::new(TestWindowParams::new(id));
        let tile = super::tile::Tile::new(
            win,
            Size::from((1280., 720.)),
            1.0,
            Clock::with_time(Duration::ZERO),
            Rc::new(Options::default()),
        );
        TN::Leaf(tile)
    };
    let node = |layout: L, children: Vec<TN<TestWindow>>| -> TN<TestWindow> {
        let n = children.len();
        TN::internal(layout, children, 0, vec![SplitChildData::new_auto(); n], None)
    };
    let ids = |n: &TN<TestWindow>| -> Vec<usize> {
        n.leaves().map(|(t, _)| *t.window().id()).collect()
    };

    // V[ 1, V[2,3], 4 ] is a section directly containing a section → flatten to V[1,2,3,4].
    let mut root = node(L::SplitV, vec![leaf(1), node(L::SplitV, vec![leaf(2), leaf(3)]), leaf(4)]);
    root.simplify();
    assert_eq!(root.child_count(), 4, "same-family section should merge");
    assert_eq!(ids(&root), vec![1, 2, 3, 4], "order preserved");
    root.verify_structure();

    // V[ H[5,6], 7 ] is cross-family → the row stays nested.
    let mut cross = node(L::SplitV, vec![node(L::SplitH, vec![leaf(5), leaf(6)]), leaf(7)]);
    cross.simplify();
    assert_eq!(cross.child_count(), 2, "cross-family nesting is preserved");
    cross.verify_structure();

    // A single-child split nested in a same-family split collapses then merges: V[ V[8] ] → 8 lifted
    // so the parent becomes a plain V[8,...]; here V[1-child] flattens to the leaf.
    let mut chain = node(L::SplitV, vec![node(L::SplitV, vec![leaf(8)]), leaf(9)]);
    chain.simplify();
    assert_eq!(ids(&chain), vec![8, 9]);
    assert_eq!(chain.child_count(), 2);
    chain.verify_structure();
}

#[test]
fn born_tabbed_section_untabs_to_a_vertical_section() {
    // A section born tabbed (via a default-section-display rule) must, when un-tabbed, collapse to a
    // vertical section (windows stacked, different y) — not a horizontal row. Regression for the
    // prev_split default of a freshly-tabbed node.
    let mut tabbed_rule = TestWindowParams::new(1);
    tabbed_rule.rules = Some(ResolvedWindowRules {
        default_section_display: Some(niri_ipc::SectionDisplay::Tabbed),
        ..ResolvedWindowRules::default()
    });

    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: tabbed_rule },
        Op::AddWindow { params: TestWindowParams::new(2) },
        // Pull window 2 into the born-tabbed section (it becomes a second tab).
        Op::ConsumeOrExpelWindowLeft { id: None },
        // Now turn tabbing off.
        Op::SetSectionDisplay(niri_ipc::SectionDisplay::Normal),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    assert_eq!(tile_count(&layout), 2);
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, _) = window_geo(&layout, 2).unwrap();
    assert_eq!(p1.x, p2.x, "un-tabbed born-tabbed section must be vertical (shared x)");
    assert_ne!(p1.y, p2.y, "un-tabbed born-tabbed section must stack windows (different y)");
}

#[test]
fn toggle_tabbed_then_untoggle_restores_split() {
    // Toggling tabbed on and then off should restore the split layout.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::ToggleTabbed,
        Op::ToggleTabbed,
    ]);

    // Both windows should be present.
    assert_eq!(tile_count(&layout), 2);

    // After untoggling, the two windows should be stacked vertically (different y).
    let (pos1, _) = window_geo(&layout, 1).unwrap();
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    assert_ne!(pos1.y, pos2.y, "windows should be stacked after untoggling tabbed");
}

#[test]
fn toggle_tabbed_on_nested_row_tabs_only_the_row() {
    // Section layout: window 1 on top, a side-by-side row [2 | 3] below. Tabbing while focused
    // inside the row should tab ONLY the row (the active leaf's parent), leaving window 1 in place
    // — not collapse all three windows into one tabbed container.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ToggleTabbed,
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    assert_eq!(tile_count(&layout), 3);

    // Window 1 stays visible (it lives outside the tabbed row).
    assert_eq!(
        window_visible(&layout, 1),
        Some(true),
        "the window above the row stays visible"
    );
    // Inside the tabbed row, only the active tab (3) shows.
    assert_eq!(
        window_visible(&layout, 3),
        Some(true),
        "active tab in the row is visible"
    );
    assert_eq!(
        window_visible(&layout, 2),
        Some(false),
        "inactive tab in the row is hidden"
    );

    // The two row tabs share a position; window 1 sits above them.
    let (pos1, _) = window_geo(&layout, 1).unwrap();
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    let (pos3, _) = window_geo(&layout, 3).unwrap();
    assert_eq!(pos2, pos3, "tabbed row tiles share a position");
    assert!(pos1.y < pos2.y, "window 1 is above the tabbed row");
}

#[test]
fn untoggling_a_tabbed_row_restores_side_by_side() {
    // Tabbing then untabbing a side-by-side row must restore the row (horizontal), not collapse it
    // into a vertical stack — the split's restore axis is preserved through the tabbed state.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ToggleTabbed,
        Op::ToggleTabbed,
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    assert_eq!(tile_count(&layout), 3);
    assert_eq!(window_visible(&layout, 2), Some(true));
    assert_eq!(window_visible(&layout, 3), Some(true));

    // Row restored: 2 and 3 side by side (different x).
    let (pos2, _) = window_geo(&layout, 2).unwrap();
    let (pos3, _) = window_geo(&layout, 3).unwrap();
    assert_ne!(
        pos2.x, pos3.x,
        "the row must be horizontal again after untabbing"
    );
}

#[test]
fn move_tab_reorders_tabs() {
    // MoveTab should reorder tabs within a tabbed container. The three windows start in order
    // [1, 2, 3] with window 3 active (last added). Moving the active tab left swaps it with its
    // predecessor, giving [1, 3, 2].
    let before = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ToggleTabbed,
    ]);
    assert_eq!(window_order(&before), vec![1, 2, 3]);

    let after = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ToggleTabbed,
        Op::MoveTab(niri_ipc::TabDirection::Left),
    ]);

    // No windows lost, and the active tab (3) moved one slot left.
    assert_eq!(tile_count(&after), 3);
    assert_eq!(window_order(&after), vec![1, 3, 2], "active tab should move left");
}

#[test]
fn split_then_focus_within_split() {
    // After creating a split, focus navigation should work within the split.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
    ]);

    // Three windows, all in the same section split along the main axis.
    assert_eq!(tile_count(&layout), 3);

    // Focus navigation should not panic.
    let layout2 = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::FocusSectionLeft,
        Op::FocusSectionLeft,
        Op::FocusSectionRight,
    ]);
    assert_eq!(tile_count(&layout2), 3);
}

#[test]
fn close_window_in_split_collapses() {
    // Closing a window in a split should not leave an empty split.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::CloseWindow(2),
    ]);

    // Only one window should remain.
    assert_eq!(tile_count(&layout), 1);
    assert!(window_geo(&layout, 1).is_some());
}

#[test]
fn close_all_windows_in_split_removes_section() {
    // Closing all windows in a split should remove the section.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::CloseWindow(1),
        Op::CloseWindow(2),
    ]);

    // No tiles should remain.
    assert_eq!(tile_count(&layout), 0);
}

#[test]
fn split_in_fullscreen_section_does_not_violate_invariant() {
    // SplitWindow in a fullscreen section should not create a split
    // (falls back to normal insertion when the section is fullscreen).
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SetFullscreenWindow { window: 1, is_fullscreen: true },
        Op::Communicate(1),
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
    ]);

    // The split must have been suppressed: both windows exist, but window 1 (fullscreen) is not
    // shrunk into a side-by-side split — it lands in its own section, wider than the normally-tiled
    // window 2. check_ops also verifies the fullscreen invariant.
    assert_eq!(tile_count(&layout), 2);
    let (_, size1) = window_geo(&layout, 1).unwrap();
    let (_, size2) = window_geo(&layout, 2).unwrap();
    assert!(
        size1.w > size2.w,
        "fullscreen window (w={}) should be wider than the separately-tiled window (w={}) — \
         i.e. no split was created",
        size1.w,
        size2.w,
    );
}

#[test]
fn consume_into_split_with_three_sections() {
    // ConsumeWindowIntoSplit with three sections should work correctly.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::AddWindow { params: TestWindowParams::new(3) },
        // Three sections, each with one window.
        Op::ConsumeWindowIntoSplit,
    ]);

    assert_eq!(tile_count(&layout), 3);

    // The consume must have created a side-by-side split: some pair of windows now shares a cross
    // position (same y) at different main positions (different x), while a third stays apart.
    let geos: Vec<_> = (1..=3)
        .map(|id| window_geo(&layout, id).unwrap().0)
        .collect();
    let has_side_by_side = (0..geos.len()).any(|i| {
        (0..geos.len()).any(|j| i != j && geos[i].y == geos[j].y && geos[i].x != geos[j].x)
    });
    assert!(
        has_side_by_side,
        "consume-into-split should leave a side-by-side pair, got positions {geos:?}"
    );
}

#[test]
fn toggle_tabbed_on_main_split() {
    // Tabbing a main-axis split turns its two side-by-side windows into tabs: only the active one
    // is visible. (Visibility is render-order state, independent of in-flight animations.)
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::ToggleTabbed,
    ]);

    assert_eq!(tile_count(&layout), 2);
    assert_eq!(window_visible(&layout, 2), Some(true), "active tab visible");
    assert_eq!(window_visible(&layout, 1), Some(false), "inactive tab hidden");

    // Untoggling restores the side-by-side split: both windows visible again.
    let layout2 = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::ToggleTabbed,
        Op::ToggleTabbed,
    ]);

    assert_eq!(tile_count(&layout2), 2);
    assert_eq!(window_visible(&layout2, 1), Some(true), "both visible after untoggle");
    assert_eq!(window_visible(&layout2, 2), Some(true), "both visible after untoggle");
}

#[test]
fn fullscreen_section_does_not_trip_tile_data_check() {
    // Fullscreen/maximized layout sizes tiles directly and bypasses the flat per-leaf `data`
    // bookkeeping, so a fullscreen section's cached `data.size` legitimately differs from the
    // (fullscreen) tile size. check_ops/verify_invariants must tolerate that.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SetFullscreenWindow { window: 1, is_fullscreen: true },
        Op::Communicate(1),
        // A tabbed fullscreen section is also valid and must not trip the check.
        Op::SetSectionDisplay(niri_ipc::SectionDisplay::Tabbed),
    ]);
    assert_eq!(tile_count(&layout), 1);
}

#[test]
fn untoggling_tabbed_clears_fullscreen_on_multi_tile_section() {
    // A fullscreen tabbed section (allowed) toggled back to normal must NOT stay fullscreen, since
    // a non-tabbed multi-tile section can't be fullscreen. check_ops verifies this invariant.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::ToggleTabbed,
        Op::SetFullscreenWindow { window: 2, is_fullscreen: true },
        Op::Communicate(2),
        Op::ToggleTabbed,
    ]);

    // Two windows remain and the section is back to a normal (non-fullscreen) split.
    assert_eq!(tile_count(&layout), 2);
}

fn wide_window(id: usize) -> TestWindowParams {
    let mut p = TestWindowParams::new(id);
    // Give a real minimum width so a side-by-side row fills the section (the default test window
    // shrinks to a few pixels, leaving no testable interior).
    p.min_max_size = (Size::from((300, 200)), Size::from((0, 0)));
    p
}

/// Builds a row (col 0: Main[1,2]) + window 3 in col 1, drags window 3 onto col 0 at the given
/// cursor and drops it, then returns the settled window geometries (1, 2, 3).
fn drag_window3_onto_row(px: f64, py: f64) -> [Point<f64, Logical>; 3] {
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::AddWindow { params: wide_window(3) },
        ],
    );
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 3, output_idx: 1, px: 900., py: 360. },
            Op::InteractiveMoveUpdate { window: 3, dx: -700., dy: 0., output_idx: 1, px, py },
            Op::InteractiveMoveEnd { window: 3 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    [
        window_geo(&layout, 1).unwrap().0,
        window_geo(&layout, 2).unwrap().0,
        window_geo(&layout, 3).unwrap().0,
    ]
}

#[test]
fn drag_onto_a_tabbed_section_body_adds_a_tab() {
    use super::monitor::InsertPosition;
    // Tabbed[1,2]; a drop in its body reports InsertTab into the section (sway: drop on the tabs/
    // content → new tab), which add_tile_as_tab applies by joining the existing tab group.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SetLayout(super::tile_node::Layout::Tabbed),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));
    assert!(
        matches!(ip(166., 360.), InsertPosition::InsertTab(0, _)),
        "drop on a tabbed section body should add a tab, got {:?}",
        ip(166., 360.)
    );
}

#[test]
fn drag_over_tab_header_adds_tab_over_content_uses_region_map() {
    use super::monitor::InsertPosition;
    // Fully-tabbed root: Tabbed[1, 2]. The titlebar band at the top adds a tab; below it, the
    // content follows the precise per-window region map (edge → split the visible window, centre →
    // tab in detach mode).
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SetLayout(super::tile_node::Layout::Tabbed),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    // The two tabs share the content rect; use window 2's geometry for it.
    let (cpos, csize) = window_geo(&layout, 2).unwrap();
    let cx = cpos.x + csize.w / 2.;
    let cy = cpos.y + csize.h / 2.;

    // A point just above the content (in the reserved titlebar band) adds a tab.
    let header_y = cpos.y - 6.;
    assert!(
        matches!(ip(cx, header_y), InsertPosition::InsertTab(0, _)),
        "over the tab header band should add a tab, got {:?}",
        ip(cx, header_y)
    );
    // The left edge of the content splits the visible window side-by-side. (Stay clear of the
    // section's left gap, which would read as a new section, but within the left third.)
    let left_x = cpos.x + csize.w * 0.15;
    assert!(
        matches!(ip(left_x, cy), InsertPosition::InSplit(0, _, SplitAxis::Main, false)),
        "left edge of the tabbed content should split the visible window, got {:?}",
        ip(left_x, cy)
    );
    // The centre groups into tabs (detach mode).
    assert!(
        matches!(ip(cx, cy), InsertPosition::InsertTab(0, _)),
        "centre of the tabbed content should tab, got {:?}",
        ip(cx, cy)
    );
}

#[test]
fn drag_over_nested_tabbing_header_adds_tab_over_content_uses_region_map() {
    use super::monitor::InsertPosition;
    // Nested tabbing container under a horizontal root: SplitH[1, Stacked[2, 3]] with window 3 the
    // visible tab. The nested container reserves its own titlebar band; a drop there adds a tab to
    // it, while a drop over its content follows the per-window region map on the visible leaf (3 =
    // flat index 2).
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(3) },
            Op::ToggleTabbed, // Cross[2,3] -> Stacked[2,3]; window 3 stays visible.
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    let (cpos, csize) = window_geo(&layout, 3).unwrap();
    let cx = cpos.x + csize.w / 2.;
    let cy = cpos.y + csize.h / 2.;

    // Over the nested container's titlebar band → add a tab to *that* container.
    let header_y = cpos.y - 6.;
    assert!(
        matches!(ip(cx, header_y), InsertPosition::InsertTab(0, _)),
        "over the nested tab header should add a tab, got {:?}",
        ip(cx, header_y)
    );
    // Left edge of the visible content → split window 3 (leaf 2) side-by-side.
    let left_x = cpos.x + csize.w * 0.15;
    assert!(
        matches!(ip(left_x, cy), InsertPosition::InSplit(0, 2, SplitAxis::Main, false)),
        "left edge of the nested content should split window 3, got {:?}",
        ip(left_x, cy)
    );
    // Centre → group window 3 into tabs (detach mode).
    assert!(
        matches!(ip(cx, cy), InsertPosition::InsertTab(0, 2)),
        "centre of the nested content should tab window 3, got {:?}",
        ip(cx, cy)
    );
}

#[test]
fn in_place_drag_over_tab_header_adds_tab_centre_swaps() {
    use super::monitor::InsertPosition;
    // In-place mode: a tabbed section's titlebar band still adds a tab, but the content centre is a
    // swap target (sway's centre-drop), not a tab group.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SetLayout(super::tile_node::Layout::Tabbed),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    let (cpos, csize) = window_geo(&layout, 2).unwrap();
    let cx = cpos.x + csize.w / 2.;
    let cy = cpos.y + csize.h / 2.;

    let header_y = cpos.y - 6.;
    assert!(
        matches!(ip(cx, header_y), InsertPosition::InsertTab(0, _)),
        "over the tab header band should add a tab even in-place, got {:?}",
        ip(cx, header_y)
    );
    assert!(
        matches!(ip(cx, cy), InsertPosition::Swap(0, _)),
        "centre of the tabbed content should swap in-place, got {:?}",
        ip(cx, cy)
    );
}

#[test]
fn drag_below_a_windows_bottom_stacks_below_above_stacks_above() {
    // Row [1 | 2]. Dropping in the bottom region of window 1 splits *that window* (a Cross split),
    // stacking window 3 below it in window 1's column — windows 1 and 3 share the column (same x),
    // window 3 lands below (greater y).
    let [p1, _p2, p3] = drag_window3_onto_row(166., 600.);
    assert_eq!(p1.x, p3.x, "windows 1 and 3 share a column (x)");
    assert!(p3.y > p1.y, "the bottom region stacks window 3 BELOW window 1 (p3={p3:?}, p1={p1:?})");

    // Dropping in the top region stacks it above (smaller y), still in window 1's column.
    let [p1, _p2, p3] = drag_window3_onto_row(166., 100.);
    assert_eq!(p1.x, p3.x, "windows 1 and 3 share a column (x)");
    assert!(p3.y < p1.y, "the top region stacks window 3 ABOVE window 1 (p3={p3:?}, p1={p1:?})");
}

#[test]
fn drop_in_a_nested_row_targets_the_window_under_the_cursor() {
    use super::monitor::InsertPosition;
    // Cross[1, 2, Main[3,4]] — a vertical stack whose bottom child is a side-by-side row.
    // Windows: 1 at y≈16, 2 at y≈251, the row (3 left / 4 right) at y≈486..704.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(3) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(4) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::Communicate(4),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    assert_eq!(window_order(&layout), vec![1, 2, 3, 4]);
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    // The top/bottom of the row's left tile (window 3 = leaf 2) now split *that window* (a Cross
    // split), stacking the dragged window above/below it — not above/below the whole row. (The old
    // above/below-the-whole-row InSection escalation is deliberately gone; the inter-tile gaps still
    // reach InSection for escaping the row.)
    assert!(
        matches!(ip(166., 660.), InsertPosition::InSplit(0, 2, SplitAxis::Cross, true)),
        "bottom of the row's left tile should stack below that window, got {:?}",
        ip(166., 660.)
    );
    assert!(
        matches!(ip(166., 540.), InsertPosition::InSplit(0, 2, SplitAxis::Cross, false)),
        "top of the row's left tile should stack above that window, got {:?}",
        ip(166., 540.)
    );
    // Centre of the row's left tile targets that tile (window 3 = leaf 2) — grouping into tabs in
    // the default (detach) mode.
    assert!(
        matches!(ip(166., 590.), InsertPosition::InsertTab(0, 2)),
        "centre of the row should target a tile, got {:?}",
        ip(166., 590.)
    );
}

#[test]
fn drag_into_row_targets_the_tile_under_the_cursor() {
    use super::monitor::InsertPosition;

    // A horizontal row of two wide windows: window 1 spans x≈[16,316], window 2 x≈[332,632],
    // both full height. (See dimensions verified interactively.)
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    // Centre of the left tile → target tile 0; centre of the right tile → target tile 1.
    // (Previously a row always reported tile 0 regardless of x.) The centre region groups into tabs
    // in the default (detach) mode, but the point is the *targeted tile index* follows the cursor.
    assert!(
        matches!(ip(166., 360.), InsertPosition::InsertTab(0, 0)),
        "left tile centre should target tile 0, got {:?}",
        ip(166., 360.)
    );
    assert!(
        matches!(ip(482., 360.), InsertPosition::InsertTab(0, 1)),
        "right tile centre should target tile 1, got {:?}",
        ip(482., 360.)
    );

    // The top/bottom regions now split the *targeted window* (a Cross split), stacking the dragged
    // window above/below that one tile — not above/below the whole row. (The old whole-row
    // above/below InSection escalation is deliberately gone.) The top of the right tile (window 2 =
    // leaf 1) stacks above it; the bottom of the left tile (window 1 = leaf 0) stacks below it.
    assert!(
        matches!(ip(482., 100.), InsertPosition::InSplit(0, 1, SplitAxis::Cross, false)),
        "top region of the right tile should stack above that window, got {:?}",
        ip(482., 100.)
    );
    assert!(
        matches!(ip(166., 600.), InsertPosition::InSplit(0, 0, SplitAxis::Cross, true)),
        "bottom region of the left tile should stack below that window, got {:?}",
        ip(166., 600.)
    );
}

#[test]
fn drag_into_tile_regions_split_at_edges_and_tab_at_centre() {
    use super::monitor::InsertPosition;

    // A horizontal row [1 | 2]. The tile interior is a sway-style region map: the left/right
    // edge-ward regions place the window side-by-side (Main); the centre groups into tabs in the
    // default (detach) mode (the in-place mode swaps there instead — see the Swap-region test).
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    // Left tile spans x≈[16,316], full section height; centre (166, ~mid) groups into tabs.
    assert!(
        matches!(ip(166., 360.), InsertPosition::InsertTab(0, 0)),
        "centre should group into tabs, got {:?}",
        ip(166., 360.)
    );
    assert!(
        matches!(
            ip(60., 360.),
            InsertPosition::InSplit(0, 0, SplitAxis::Main, false)
        ),
        "left edge should place beside on the left (Main), got {:?}",
        ip(60., 360.)
    );
    assert!(
        matches!(
            ip(270., 360.),
            InsertPosition::InSplit(0, 0, SplitAxis::Main, true)
        ),
        "right edge should place beside on the right (Main), got {:?}",
        ip(270., 360.)
    );
}

#[test]
fn drag_into_tile_centre_tabs_the_windows() {
    // End-to-end: drop window 3 into the centre of the left tile of row [1 | 2]. Windows 1 and 3
    // are grouped into a tabbed container (shared position, one shown at a time); window 2 stays
    // beside them.
    let [p1, p2, p3] = drag_window3_onto_row(166., 360.);
    assert_eq!(p1.x, p3.x, "tabbed windows 1 and 3 share a position (x)");
    assert_eq!(p1.y, p3.y, "tabbed windows 1 and 3 share a position (y)");
    assert_ne!(p2.x, p1.x, "window 2 stays beside the tabbed pair");
}

#[test]
fn in_place_drag_mode_swaps_at_centre() {
    use super::monitor::InsertPosition;

    // In the in-place (sway) drag mode the source stays in the tree, so the centre region is a swap
    // target rather than a tab group. (The full in-place state machine that applies the swap is a
    // later stage; here we verify the region map already routes centre → Swap under that mode.)
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));
    assert!(
        matches!(ip(166., 360.), InsertPosition::Swap(0, 0)),
        "in-place centre should be a swap target, got {:?}",
        ip(166., 360.)
    );
}

#[test]
fn in_place_interior_region_map_is_not_gap_offset() {
    use super::monitor::InsertPosition;

    // Regression: the gap-aiming fudge (`+ gaps/2`) must only bias the closest-gap search, not the
    // per-tile interior hit-test / rel_x-rel_y region map. With the fudge leaking in, every interior
    // drop zone was displaced gaps/2 up-left, so a cursor a few px inside the centre band (but
    // within gaps/2 of the band's right edge) was misclassified as a split. Uses default gaps (16).
    let layout = in_place_row();
    let ws = layout.active_workspace().unwrap();
    let ip = |x: f64, y: f64| ws.scrolling_insert_position(Point::from((x, y)));

    let (pos, size) = window_geo(&layout, 1).unwrap();
    assert!(size.w > 0. && size.h > 0.);

    // rel_x = 0.65 sits inside the centre band [1/3, 2/3] but within gaps/2 (8px on a ~300px tile,
    // ≈0.027 in rel terms) of the band's right edge, so the +gaps/2 fudge would push it to ≈0.677 —
    // outside the band — and misreport a Main split. Unfudged it stays a centre Swap.
    let x = pos.x + size.w * 0.65;
    let y = pos.y + size.h * 0.5;
    assert!(
        matches!(ip(x, y), InsertPosition::Swap(0, 0)),
        "a cursor inside window 1's centre band should be a Swap, not gap-offset into a split, \
         got {:?}",
        ip(x, y)
    );

    // The exact centre is unambiguously a Swap target too.
    let cx = pos.x + size.w * 0.5;
    let cy = pos.y + size.h * 0.5;
    assert!(
        matches!(ip(cx, cy), InsertPosition::Swap(0, 0)),
        "the exact centre of window 1 should be a Swap, got {:?}",
        ip(cx, cy)
    );
}

/// Builds a Main row `[1, 2]` (section 0) under the in-place tiling-drag mode, settled. Window 1
/// occupies x≈[16,316] (centre ≈166), window 2 x≈[332,632] (centre ≈482), both full height.
fn in_place_row() -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    )
}

#[test]
fn in_place_centre_drop_swaps_windows() {
    // Required test 1: a centre-drop onto another window SWAPS the two, with no new tab and no
    // change in window count.
    let mut layout = in_place_row();

    let p1_before = window_geo(&layout, 1).unwrap().0;
    let p2_before = window_geo(&layout, 2).unwrap().0;
    assert!(p1_before.x < p2_before.x, "window 1 starts left of window 2");

    // Drag window 1 onto window 2's centre.
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: 166., py: 360. },
            Op::InteractiveMoveUpdate { window: 1, dx: 316., dy: 0., output_idx: 1, px: 482., py: 360. },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    assert_eq!(tile_count(&layout), 2, "no window added or removed by the swap");
    assert_eq!(
        window_order(&layout),
        vec![2, 1],
        "the two windows exchanged slots (no tab created)"
    );
    let p1_after = window_geo(&layout, 1).unwrap().0;
    let p2_after = window_geo(&layout, 2).unwrap().0;
    assert_eq!(p1_after, p2_before, "window 1 took window 2's slot");
    assert_eq!(p2_after, p1_before, "window 2 took window 1's slot");
}

#[test]
fn in_place_centre_drop_on_self_is_noop() {
    // Required test 3: a centre-drop on the SOURCE itself is a no-op. Cross the detach threshold,
    // then bring the cursor back over window 1's own centre and release.
    let mut layout = in_place_row();

    let order_before = window_order(&layout);
    let p1_before = window_geo(&layout, 1).unwrap();
    let p2_before = window_geo(&layout, 2).unwrap();

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: 166., py: 360. },
            // Past the threshold (>256px) — switches to in-place; source stays in the tree.
            Op::InteractiveMoveUpdate { window: 1, dx: 300., dy: 0., output_idx: 1, px: 466., py: 360. },
            // Back over window 1's own centre.
            Op::InteractiveMoveUpdate { window: 1, dx: -300., dy: 0., output_idx: 1, px: 166., py: 360. },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    assert_eq!(tile_count(&layout), 2, "no window added or removed");
    assert_eq!(window_order(&layout), order_before, "layout order unchanged");
    assert_eq!(window_geo(&layout, 1).unwrap(), p1_before, "window 1 unchanged");
    assert_eq!(window_geo(&layout, 2).unwrap(), p2_before, "window 2 unchanged");
}

/// Builds two 1280x720 outputs, each holding exactly one normal window: window 1 on `output1`,
/// window 2 on `output2` (window 2 is added while `output2` is focused so it lands there). Both are
/// full-size tiles centred on their own output, settled, under the in-place tiling-drag mode.
fn two_outputs_one_window_each() -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::AddOutput(2),
            // Focus output2 so the next window opens there rather than back on output1.
            Op::FocusOutput(2),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    )
}

#[test]
fn in_place_centre_drop_swaps_across_outputs() {
    // Cross-output in-place centre-drop: dragging window 1 (output1) onto window 2's centre
    // (output2) must SWAP the two windows across trees — window 1 ends up on output2's workspace and
    // window 2 on output1's, with focus following the dragged window to output2. This is the only
    // test that fires an actual cross-output `swap_tiles_cross_workspace`.
    let mut layout = two_outputs_one_window_each();

    // Self-check the starting layout: one window per output, on the expected output.
    let (o1, p1, s1) = window_output_and_geo(&layout, 1).unwrap();
    let (o2, p2, s2) = window_output_and_geo(&layout, 2).unwrap();
    assert_eq!(o1, "output1", "window 1 starts on output1");
    assert_eq!(o2, "output2", "window 2 starts on output2");
    assert_eq!(layout.windows().count(), 2, "two windows total to begin with");

    // Output-local centres of each window (both outputs are 1280x720, so geometry matches).
    let c1 = (p1.x + s1.w / 2., p1.y + s1.h / 2.);
    let c2 = (p2.x + s2.w / 2., p2.y + s2.h / 2.);

    check_ops_on_layout(
        &mut layout,
        [
            // Grab window 1 at its centre on output1.
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: c1.0, py: c1.1 },
            // Move onto output2, landing on window 2's centre. dx=1280 (>256px threshold) both
            // crosses the detach threshold to enter in-place mode and represents the pointer being
            // over the neighbouring output.
            Op::InteractiveMoveUpdate {
                window: 1,
                dx: 1280.,
                dy: 0.,
                output_idx: 2,
                px: c2.0,
                py: c2.1,
            },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    // A swap adds/removes nothing: still two windows, one per output.
    assert_eq!(layout.windows().count(), 2, "no window added or removed by the swap");

    // The two windows exchanged outputs.
    let (o1_after, _, _) = window_output_and_geo(&layout, 1).unwrap();
    let (o2_after, _, _) = window_output_and_geo(&layout, 2).unwrap();
    assert_eq!(o1_after, "output2", "window 1 crossed to output2");
    assert_eq!(o2_after, "output1", "window 2 crossed to output1");

    // Focus followed the dragged window to its new output.
    assert_eq!(
        layout.active_output().unwrap().name(),
        "output2",
        "active output followed the drag to output2"
    );
    assert_eq!(active_window_id(&layout), Some(1), "dragged window 1 is focused");
}

/// Helper: the current render (move-animation) offset of a window found anywhere in the layout. A
/// non-zero offset means the tile is mid-slide.
fn window_render_offset(layout: &Layout<TestWindow>, id: usize) -> Option<Point<f64, Logical>> {
    layout.workspaces().find_map(|(_, _, ws)| {
        ws.tiles_with_render_positions().find_map(|(tile, _, _)| {
            (*tile.window().id() == id).then(|| tile.render_offset())
        })
    })
}

/// Helper: the `(workspace id, tiling slot)` of a window, searched across every workspace.
fn window_ws_and_slot(
    layout: &Layout<TestWindow>,
    id: usize,
) -> Option<(WorkspaceId, (usize, usize))> {
    layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.scrolling_position_of(&id).map(|slot| (ws.id(), slot)))
}

/// Helper: fetch the `Output` handle with the given name from the layout.
fn output_named(layout: &Layout<TestWindow>, name: &str) -> Output {
    layout
        .outputs()
        .find(|o| o.name() == name)
        .unwrap_or_else(|| panic!("output {name} not found"))
        .clone()
}

#[test]
fn focus_screen_right_crosses_to_adjacent_output() {
    // Spatial focus at the tree edge crosses to the physically-adjacent output. output1 holds a
    // single window (window 1), so focusing right from it hits the tree edge and crosses to output2.
    let mut layout = two_outputs_one_window_each();
    let out1 = output_named(&layout, "output1");
    let out2 = output_named(&layout, "output2");

    layout.focus_output(&out1);
    assert_eq!(layout.active_output().unwrap().name(), "output1", "starting on output1");
    assert_eq!(active_window_id(&layout), Some(1), "window 1 is focused to start");

    let crossed = layout.focus_screen_right_or_output(&out2);
    assert!(crossed, "at the tree edge, focus crosses to the neighbour output");
    assert_eq!(
        layout.active_output().unwrap().name(),
        "output2",
        "focus crossed to output2"
    );
    assert_eq!(active_window_id(&layout), Some(2), "output2's window is now focused");
}

#[test]
fn move_screen_right_carries_window_to_adjacent_output() {
    // A spatial move at the tree edge carries the window across to the adjacent output. window 1 is
    // alone on output1, so moving it right hits the edge and hands it to output2 via move_to_output.
    let mut layout = two_outputs_one_window_each();
    let out1 = output_named(&layout, "output1");
    let out2 = output_named(&layout, "output2");

    layout.focus_output(&out1);
    assert_eq!(window_output_and_geo(&layout, 1).unwrap().0, "output1", "window 1 starts on output1");

    let crossed = layout.move_screen_right_or_to_output(&out2);
    assert!(crossed, "at the tree edge, the move carries the window to the neighbour output");
    assert_eq!(
        window_output_and_geo(&layout, 1).unwrap().0,
        "output2",
        "window 1 crossed to output2"
    );
    assert_eq!(
        layout.active_output().unwrap().name(),
        "output2",
        "active output followed the moved window"
    );
    // The pre-existing window on output2 is untouched.
    assert_eq!(window_output_and_geo(&layout, 2).unwrap().0, "output2", "window 2 stayed on output2");
}

#[test]
fn focus_screen_left_stays_when_interior_neighbour_exists() {
    // Negative case: when NOT at the tree edge, spatial focus stays on the source output. output1
    // holds two side-by-side windows [1, 2]; focusing left from window 2 lands on the interior
    // neighbour (window 1) and does not cross to output2.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::AddOutput(2),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let out2 = output_named(&layout, "output2");

    // Active window is window 2 (the right tile); its left neighbour (window 1) is interior.
    assert_eq!(layout.active_output().unwrap().name(), "output1", "starting on output1");
    assert_eq!(active_window_id(&layout), Some(2), "window 2 is focused to start");

    let crossed = layout.focus_screen_left_or_output(&out2);
    assert!(!crossed, "an interior neighbour exists, so focus stays put");
    assert_eq!(
        layout.active_output().unwrap().name(),
        "output1",
        "focus stayed on output1"
    );
    assert_eq!(active_window_id(&layout), Some(1), "focus moved to the interior neighbour");
}

/// Builds two workspaces, each holding a two-tile section, where window 1 (in one section) is a
/// fixed-narrow tile and window 2 (in the other) is wide. Swapping window 1 and window 2 therefore
/// reflows each section's neighbour (window 3 beside window 1; window 4 beside window 2). The two
/// sections live on `ws_a`/`ws_b`; whether those share an output is controlled by `same_output`.
fn cross_workspace_swap_layout(same_output: bool) -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;

    // Window 1 is a fixed 300px-wide tile and window 2 a fixed 500px-wide one. Because tiled tiles
    // here render at their minimum width, swapping the two changes each section's width and so
    // shifts each section's neighbour (window 3 beside window 1; window 4 beside window 2).
    let mut narrow = wide_window(1);
    narrow.min_max_size = (Size::from((300, 200)), Size::from((300, 200)));
    let mut wide = wide_window(2);
    wide.min_max_size = (Size::from((500, 200)), Size::from((500, 200)));

    let mut ops = vec![
        Op::AddOutput(1),
        // Section A: [1 (300px), 3].
        Op::AddWindow { params: narrow },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
    ];
    if same_output {
        // Section B: [2 (500px), 4], then pushed to a new workspace below on the SAME output.
        ops.extend([
            Op::AddWindow { params: wide },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(4) },
            Op::MoveSectionToWorkspaceDown(false),
        ]);
    } else {
        // Section B: [2 (500px), 4] on a SECOND output.
        ops.extend([
            Op::AddOutput(2),
            Op::FocusOutput(2),
            Op::AddWindow { params: wide },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(4) },
        ]);
    }
    ops.extend([
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::Communicate(4),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    check_ops_with_options(options, ops)
}

#[test]
fn cross_workspace_same_output_swap_slides_tiles() {
    // A cross-workspace centre-drop swap between two workspaces on the SAME output slides the
    // reflowed tiles into place (instead of teleporting them).
    let mut layout = cross_workspace_swap_layout(true);

    let (ws1, slot1) = window_ws_and_slot(&layout, 1).unwrap();
    let (ws2, slot2) = window_ws_and_slot(&layout, 2).unwrap();
    assert_ne!(ws1, ws2, "windows 1 and 2 start on different workspaces");

    // Everything settled from the build, so no tile is mid-slide before the swap.
    assert_eq!(window_render_offset(&layout, 3).unwrap().x, 0.);
    assert_eq!(window_render_offset(&layout, 4).unwrap().x, 0.);

    let swapped = layout.swap_tiles_cross_workspace(ws1, slot1, ws2, slot2);
    assert!(swapped, "the cross-workspace swap succeeded");

    // The two windows exchanged workspaces.
    assert_eq!(window_ws_and_slot(&layout, 1).unwrap().0, ws2, "window 1 moved to ws2");
    assert_eq!(window_ws_and_slot(&layout, 2).unwrap().0, ws1, "window 2 moved to ws1");

    // Each section's neighbour is now mid-slide (the swapped-in tile changed the section width).
    assert_ne!(
        window_render_offset(&layout, 3).unwrap().x,
        0.,
        "window 3 slides after the same-output swap"
    );
    assert_ne!(
        window_render_offset(&layout, 4).unwrap().x,
        0.,
        "window 4 slides after the same-output swap"
    );
}

#[test]
fn cross_output_swap_does_not_animate() {
    // The same swap across two DIFFERENT outputs teleports (niri's layout isn't position-aware), so
    // no tile ends up mid-slide even though the sections reflow.
    let mut layout = cross_workspace_swap_layout(false);

    let (ws1, slot1) = window_ws_and_slot(&layout, 1).unwrap();
    let (ws2, slot2) = window_ws_and_slot(&layout, 2).unwrap();
    assert_ne!(ws1, ws2, "windows 1 and 2 start on different workspaces");
    assert_eq!(window_output_and_geo(&layout, 1).unwrap().0, "output1");
    assert_eq!(window_output_and_geo(&layout, 2).unwrap().0, "output2");

    let swapped = layout.swap_tiles_cross_workspace(ws1, slot1, ws2, slot2);
    assert!(swapped, "the cross-output swap succeeded");

    // The windows exchanged outputs...
    assert_eq!(window_output_and_geo(&layout, 1).unwrap().0, "output2", "window 1 crossed outputs");
    assert_eq!(window_output_and_geo(&layout, 2).unwrap().0, "output1", "window 2 crossed outputs");

    // ...but nothing slid: the reflowed neighbours snapped straight to their new slots.
    assert_eq!(
        window_render_offset(&layout, 3).unwrap().x,
        0.,
        "window 3 teleports across outputs"
    );
    assert_eq!(
        window_render_offset(&layout, 4).unwrap().x,
        0.,
        "window 4 teleports across outputs"
    );
}

#[test]
fn in_place_centre_drop_onto_maximized_target_does_not_swap() {
    // A centre-drop swap is only valid between two normal-sized tiles. Dropping a normal window onto
    // a fullscreen (section-level sizing) target must REFUSE the swap and fall back to the detach
    // path (which unsets the sizing mode), so the fullscreen flag never rides along to the wrong
    // window. Uses a single row [1, 2] on one output; window 2 is the fullscreen target.
    let mut layout = in_place_row();

    // Fullscreen the target (window 2). Fullscreen is section-level, so a swap would strand it.
    check_ops_on_layout(
        &mut layout,
        [
            Op::FullscreenWindow(2),
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    assert!(
        window_sizing_mode(&layout, 2).unwrap().is_fullscreen(),
        "window 2 is fullscreen before the drag"
    );
    assert!(
        window_sizing_mode(&layout, 1).unwrap().is_normal(),
        "window 1 is normal before the drag"
    );

    // Drag window 1's centre onto the fullscreen window 2 (occupying the whole output) and release.
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: 166., py: 360. },
            Op::InteractiveMoveUpdate { window: 1, dx: 640., dy: 0., output_idx: 1, px: 640., py: 360. },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    assert_eq!(tile_count(&layout), 2, "no window added or removed");
    // The swap was refused and the drop took the detach fallback instead. That path unsets sizing
    // modes, so BOTH windows are normal afterwards — crucially, window 1 did not inherit window 2's
    // section-level fullscreen. That is exactly what a bad in-place swap would have done: strand the
    // fullscreen state on window 1's arriving tile, which trips a layout invariant (verified by
    // temporarily removing the guard).
    assert!(
        window_sizing_mode(&layout, 1).unwrap().is_normal(),
        "dragged window 1 must not have inherited a fullscreen slot via a bad swap"
    );
    assert!(
        window_sizing_mode(&layout, 2).unwrap().is_normal(),
        "the detach fallback cleared the fullscreen mode rather than swapping it onto the wrong tile"
    );
}

/// Runs the same drag of window 1 onto `(to_x, to_y)` under the given tiling-drag mode and returns
/// the resulting (window order, per-window geometry) so the two modes can be compared.
fn drag_window1_to(
    mode: niri_config::TilingDrag,
    to_x: f64,
    to_y: f64,
) -> (Vec<usize>, Vec<(Point<f64, Logical>, Size<f64, Logical>)>) {
    let mut options = Options::default();
    options.layout.tiling_drag = mode;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: 166., py: 360. },
            Op::InteractiveMoveUpdate { window: 1, dx: to_x - 166., dy: to_y - 360., output_idx: 1, px: to_x, py: to_y },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let order = window_order(&layout);
    let geos = order
        .iter()
        .map(|id| window_geo(&layout, *id).unwrap())
        .collect();
    (order, geos)
}

#[test]
fn in_place_edge_drop_moves_like_detach() {
    // Required test 2: an edge-drop still MOVES/splits exactly like the detach mode. Drop into the
    // right third of window 2 (a side region → place beside).
    let in_place = drag_window1_to(niri_config::TilingDrag::InPlace, 580., 360.);
    let detach = drag_window1_to(niri_config::TilingDrag::Detach, 580., 360.);
    assert_eq!(
        in_place, detach,
        "an edge drop must land identically in in-place and detach modes"
    );

    // And it really did place window 1 beside (to the right of) window 2.
    let (_order, geos) = &in_place;
    assert_eq!(geos.len(), 2, "window count unchanged");
    let p1 = window_geo_from(&in_place, 1);
    let p2 = window_geo_from(&in_place, 2);
    assert!(p1.0.x > p2.0.x, "window 1 ended up to the right of window 2");
}

#[test]
fn in_place_same_workspace_move_keeps_count_and_lands_right() {
    // Required test 4: a same-workspace move leaves the window count unchanged and lands where
    // expected. Drop window 1 far to the right (past window 2) → its own new section on the right.
    let in_place = drag_window1_to(niri_config::TilingDrag::InPlace, 1000., 360.);
    let detach = drag_window1_to(niri_config::TilingDrag::Detach, 1000., 360.);
    assert_eq!(
        in_place, detach,
        "a same-workspace move must match the detach mode"
    );

    let (order, geos) = &in_place;
    assert_eq!(geos.len(), 2, "no window added or removed");
    assert!(
        order.contains(&1) && order.contains(&2),
        "both windows still present, got {order:?}"
    );
    let p1 = window_geo_from(&in_place, 1);
    let p2 = window_geo_from(&in_place, 2);
    assert!(p1.0.x > p2.0.x, "window 1 moved to the right of window 2");
}

#[test]
fn in_place_centre_drop_targets_the_visible_tab() {
    use super::monitor::InsertPosition;

    // `SplitH[ 1, Stacked[2, 3] ]` with window 3 the visible tab. The hidden tab (2) shares 3's
    // rect, so a centre-drop over the stacked body must target the *visible* leaf (3 = flat idx 2),
    // not the first leaf sharing the rect (2). Regression for the hit-test picking a hidden tab.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(3) },
            Op::ToggleTabbed, // SplitV[2,3] -> Stacked[2,3] (family-aware); window 3 stays visible.
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let (cpos, csize) = window_geo(&layout, 3).unwrap();
    let centre = Point::from((cpos.x + csize.w / 2., cpos.y + csize.h / 2.));
    let got = ws.scrolling_insert_position(centre);
    assert!(
        matches!(got, InsertPosition::Swap(0, 2)),
        "centre over the visible tab (window 3 = leaf 2) must target it, not the hidden tab; got {got:?}"
    );
}

#[test]
fn in_place_swap_keeps_slots_fixed_exchanges_occupants() {
    // Row [1, 2] with unequal widths (1 wide on the left, 2 narrow on the right). An in-place
    // centre-swap exchanges the windows but leaves the *slots* fixed — so the wide left slot now
    // holds window 2 and the narrow right slot holds window 1 (sway: containers stay, occupants
    // move). Guards against the swap carrying each window's width with it.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::SetWindowWidth { id: Some(1), change: SizeChange::SetFixed(800) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let (p1, s1) = window_geo(&layout, 1).unwrap();
    let (p2, s2) = window_geo(&layout, 2).unwrap();
    assert!(p1.x < p2.x && s1.w > s2.w, "window 1 starts wide on the left");

    let (c1x, c1y) = (p1.x + s1.w / 2., p1.y + s1.h / 2.);
    let (c2x, c2y) = (p2.x + s2.w / 2., p2.y + s2.h / 2.);
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: c1x, py: c1y },
            Op::InteractiveMoveUpdate {
                window: 1,
                dx: c2x - c1x,
                dy: 0.,
                output_idx: 1,
                px: c2x,
                py: c2y,
            },
            Op::InteractiveMoveEnd { window: 1 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    assert_eq!(window_order(&layout), vec![2, 1], "occupants exchanged");
    let (q1, t1) = window_geo(&layout, 1).unwrap();
    let (q2, t2) = window_geo(&layout, 2).unwrap();
    assert!(q2.x < q1.x, "window 2 now holds the left slot");
    assert!(
        t2.w > t1.w,
        "the wide left slot stayed put and now holds window 2 (slots fixed, not occupant-carried): \
         w2={}, w1={}",
        t2.w,
        t1.w
    );
}

/// Looks up a window's geometry in a `(order, geos)` pair returned by `drag_window1_to`.
fn window_geo_from(
    result: &(Vec<usize>, Vec<(Point<f64, Logical>, Size<f64, Logical>)>),
    id: usize,
) -> (Point<f64, Logical>, Size<f64, Logical>) {
    let (order, geos) = result;
    let idx = order.iter().position(|w| *w == id).unwrap();
    geos[idx]
}

#[test]
fn nested_tabbed_render_and_hit_do_not_panic() {
    // Exercise the nested tab-header render-element update and hit-testing over a tabbed row, to
    // guard the per-node geometry collection against panics (the property test doesn't render).
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::ToggleTabbed,
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let output = layout.outputs().next().unwrap().clone();
    layout.update_render_elements(Some(&output));

    // Sweep the whole output; clicking a nested tab maps to that tab's representative window.
    for x in (0..1280).step_by(32) {
        for y in (0..720).step_by(32) {
            let _ = layout.window_under(&output, Point::from((x as f64, y as f64)));
        }
    }
}

#[test]
fn in_place_drag_render_does_not_panic() {
    // In-place (sway) tiling-drag feedback: with the drag active, update_render_elements must run
    // the drop-indicator path (update_insert_hint_in_place) without panicking, for a swap-target
    // hover (centre of another tile), a split/move hover (near a window edge), and the suppressed
    // self-hover (back over the source's own slot).
    //
    // NOTE: this only exercises the indicator path. The translucent following ghost in
    // render_interactive_move_for_output needs a real GlesRenderer (offscreen compositing), which
    // the headless test harness lacks, so it can't be driven here — it is guarded by construction
    // (offscreen buffer + constant alpha) and must be verified visually in a nested session.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::InPlace;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: wide_window(2) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );
    let output = layout.outputs().next().unwrap().clone();

    // Begin dragging window 1 and cross the detach threshold (>256px) to enter the in-place state,
    // hovering window 2's centre: a Swap target.
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 1, output_idx: 1, px: 166., py: 360. },
            Op::InteractiveMoveUpdate { window: 1, dx: 316., dy: 0., output_idx: 1, px: 482., py: 360. },
        ],
    );
    layout.update_render_elements(Some(&output));

    // Hover near the right edge of the row — a split/move InsertPosition rather than a Swap.
    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveMoveUpdate { window: 1, dx: 900., dy: 0., output_idx: 1, px: 1100., py: 360. }],
    );
    layout.update_render_elements(Some(&output));

    // Back over the source's own slot: the self-hover case, where the hint is suppressed.
    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveMoveUpdate { window: 1, dx: 0., dy: 0., output_idx: 1, px: 166., py: 360. }],
    );
    layout.update_render_elements(Some(&output));
}

#[test]
fn tabbed_row_reserves_space_for_its_header() {
    // Tabbing a nested row must push its content down to leave a band for the row's own tab header,
    // rather than drawing the content under the header. Compare the row tile's y with the row
    // tabbed vs. not: tabbing should move it strictly downward by the header band.
    let untabbed = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let tabbed = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::ToggleTabbed,
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    let (untabbed_row, _) = window_geo(&untabbed, 2).unwrap();
    let (tabbed_row, _) = window_geo(&tabbed, 2).unwrap();
    assert!(
        tabbed_row.y > untabbed_row.y + 10.,
        "tabbing the row should push its content down for the header band \
         (untabbed y={}, tabbed y={})",
        untabbed_row.y,
        tabbed_row.y
    );
}

#[test]
fn drag_beside_a_stacked_window_splits_that_window() {
    use super::monitor::InsertPosition;

    // Main[1, Cross[2, 3]] — window 1 on the left, a vertical stack [2 over 3] on the right. A
    // left/right drop on a stacked tile now splits *that one window* (a Main split of the leaf
    // under the cursor), turning it into a side-by-side pair within the stack — not "beside the
    // whole stack".
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(3) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let ws = layout.active_workspace().unwrap();

    // A point in the right third of window 2 (top tile of the stack, flat leaf index 1).
    let (p2, s2) = window_geo(&layout, 2).unwrap();
    let x = p2.x + s2.w * 0.8;
    let y = p2.y + s2.h * 0.5;
    match ws.scrolling_insert_position(Point::from((x, y))) {
        InsertPosition::InSplit(0, 1, SplitAxis::Main, true) => {}
        other => panic!("expected a Main split of window 2 to its right, got {other:?}"),
    }
}

#[test]
fn drag_beside_a_stacked_window_splits_that_window_end_to_end() {
    // End-to-end: Main[1, Cross[2,3]] in section 0, window 4 alone in section 1. Dragging 4 onto the
    // right third of window 2 (the stack's top tile) splits *that window*: window 4 lands beside
    // window 2 (shared row, shared y), window 3 stays below them, and window 4 is only as tall as
    // window 2 — not the full section height.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(3) },
        Op::AddWindow { params: wide_window(4) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::Communicate(4),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    let (p2, s2) = window_geo(&layout, 2).unwrap();
    let drop_x = p2.x + s2.w * 0.8;
    let drop_y = p2.y + s2.h * 0.5;
    let (p4, _) = window_geo(&layout, 4).unwrap();
    let start: Point<f64, Logical> = Point::from((p4.x + 20., p4.y + 20.));

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin { window: 4, output_idx: 1, px: start.x, py: start.y },
            // First cross the detach threshold with a large move, then settle on the precise drop
            // point over window 2's right third.
            Op::InteractiveMoveUpdate {
                window: 4,
                dx: -500.,
                dy: 200.,
                output_idx: 1,
                px: start.x - 500.,
                py: start.y + 200.,
            },
            Op::InteractiveMoveUpdate {
                window: 4,
                dx: drop_x - (start.x - 500.),
                dy: drop_y - (start.y + 200.),
                output_idx: 1,
                px: drop_x,
                py: drop_y,
            },
            Op::InteractiveMoveEnd { window: 4 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::Communicate(4),
            Op::AdvanceAnimations { msec_delta: 1000 },
        ],
    );

    assert_eq!(tile_count(&layout), 4);
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, s2) = window_geo(&layout, 2).unwrap();
    let (p3, _) = window_geo(&layout, 3).unwrap();
    let (p4, s4) = window_geo(&layout, 4).unwrap();
    assert_eq!(p2.y, p4.y, "windows 2 and 4 form a side-by-side row (shared y)");
    assert!(p4.x > p2.x, "window 4 sits to the right of window 2 (p4={p4:?}, p2={p2:?})");
    assert!(p3.y > p2.y, "window 3 stays below the 2|4 row (p3={p3:?}, p2={p2:?})");
    assert!(p1.x < p2.x, "window 1 stays to the left of the stack");
    assert!(
        (s4.h - s2.h).abs() < 2.,
        "window 4 is only as tall as window 2 — it split that one window, not the whole stack \
         (s4={s4:?}, s2={s2:?})"
    );
}

#[test]
fn vertical_insert_into_a_row_stacks_above_below_not_beside() {
    // Section 0 is a horizontal row (Main split of windows 1 and 2). Moving window 3 into it
    // vertically (consume) must wrap the row in a Cross split so window 3 becomes a new row, not a
    // third cell beside the others.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    assert_eq!(tile_count(&layout), 3);
    let (p1, _) = window_geo(&layout, 1).unwrap();
    let (p2, _) = window_geo(&layout, 2).unwrap();
    let (p3, _) = window_geo(&layout, 3).unwrap();
    // Windows 1 and 2 remain the side-by-side row.
    assert_eq!(p1.y, p2.y, "the row stays a row");
    assert_ne!(p1.x, p2.x, "the row stays a row");
    // Window 3 lands on a different row (different y), not beside 1/2.
    assert_ne!(p3.y, p1.y, "consumed window stacks as a new row, not into the row");
}

#[test]
fn split_nests_at_target_leaf_not_root() {
    // Repeatedly splitting the active window builds a genuinely nested tree rather than appending
    // at the section root. Final shape: Cross[1, Main[2, Cross[3, 4]]] (window 4 stacked under 3,
    // that pair beside 2, and all of it under 1).
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(4) },
    ]);

    assert_eq!(tile_count(&layout), 4);
    assert_eq!(window_order(&layout), vec![1, 2, 3, 4]);
    assert_eq!(active_window_id(&layout), Some(4));

    // Directional focus confirms the nesting: from window 4, up reaches its Cross sibling (3),
    // and left crosses the inner Main split to window 2.
    let up = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(4) },
        Op::FocusWindowUp,
    ]);
    assert_eq!(active_window_id(&up), Some(3), "up -> Cross sibling of window 4");

    let left = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(4) },
        Op::FocusSectionLeft,
    ]);
    assert_eq!(active_window_id(&left), Some(2), "left -> across inner Main split");
}

#[test]
fn directional_focus_walks_nested_tree() {
    // Build Main[1, Cross[2, 3]]: window 1 on the left, a vertically-split pair (2 over 3) on the
    // right. Focus should walk the tree like i3/sway: up/down within the inner Cross split, and
    // left/right across the outer Main split (descending into the most-recently-focused leaf).
    let base = [
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
    ];

    // After construction window 3 is active (bottom-right).
    let layout = check_ops(base.iter().cloned());
    assert_eq!(active_window_id(&layout), Some(3));

    // Up moves to window 2 within the inner Cross split.
    let layout = check_ops(base.iter().cloned().chain([Op::FocusWindowUp]));
    assert_eq!(active_window_id(&layout), Some(2), "up -> sibling in inner cross split");

    // Down from window 2 returns to window 3.
    let layout = check_ops(base.iter().cloned().chain([Op::FocusWindowUp, Op::FocusWindowDown]));
    assert_eq!(active_window_id(&layout), Some(3));

    // Left from the right-hand pair crosses the outer Main split to window 1.
    let layout = check_ops(base.iter().cloned().chain([Op::FocusSectionLeft]));
    assert_eq!(active_window_id(&layout), Some(1), "left -> across outer main split");

    // Right from window 1 descends back into the right pair's last-focused leaf (window 3).
    let layout =
        check_ops(base.iter().cloned().chain([Op::FocusSectionLeft, Op::FocusSectionRight]));
    assert_eq!(active_window_id(&layout), Some(3), "right -> back into the split");
}

#[test]
fn move_window_up_reorders_within_nested_cross_split() {
    // Main[1, Cross[2, 3]] with window 3 active. Moving the window up swaps it with window 2
    // inside the inner Cross split (not across the outer Main split), giving Cross[3, 2].
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::MoveWindowUp,
    ]);

    assert_eq!(window_order(&layout), vec![1, 3, 2], "moved up within the inner split");
    assert_eq!(active_window_id(&layout), Some(3), "focus follows the moved window");
}

#[test]
fn directional_swap_moves_subtree_across_nested_split() {
    // Main[1, Cross[2, 3]] with window 3 active. Swapping left moves the active leaf's whole
    // subtree (the Cross[2,3] pair) past window 1 -> Main[Cross[2,3], 1]. Leaf order becomes
    // [2, 3, 1] and window 3 stays focused.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SwapWindowInDirection(ScrollDirection::Left),
    ]);

    assert_eq!(window_order(&layout), vec![2, 3, 1], "subtree moved past window 1");
    assert_eq!(active_window_id(&layout), Some(3), "focus follows the moved window");
}

#[test]
fn tabbed_tab_containing_split_shows_all_its_leaves() {
    // Build a section whose root, once tabbed, has a tab that is itself a split:
    //   Main[1, Cross[2, 3]]  -- toggle section display -->  Tabbed[1, Cross[2, 3]]
    // The active tab (containing windows 2 and 3) must show BOTH of its windows, while the other
    // tab (window 1) stays hidden. A naive "only the single active leaf is visible" would wrongly
    // hide window 2.
    //
    // We use ToggleSectionTabbedDisplay (Mod+W) here, which always tabs the section root regardless
    // of focus depth — unlike ToggleTabbed (Mod+Ctrl+W), which tabs the focused window's immediate
    // parent.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::ToggleSectionTabbedDisplay,
    ]);

    assert_eq!(tile_count(&layout), 3);
    assert_eq!(window_visible(&layout, 3), Some(true), "active leaf visible");
    assert_eq!(
        window_visible(&layout, 2),
        Some(true),
        "the other leaf of the active tab's split must also be visible"
    );
    assert_eq!(window_visible(&layout, 1), Some(false), "the other tab is hidden");
}


#[test]
fn move_active_tile_removes_the_focused_leaf_not_a_sibling() {
    // Bug 1: root V[H[1,3], 2] with window 2 active (root child index 1, but flat leaf index 2).
    // Moving the active window to another workspace must remove the *focused* leaf (2), not the leaf
    // sitting at the root-child index (window 3).
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::FocusWindow(1),
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::FocusWindow(2),
        // Moves the *active* tile (2) down; focus stays on the source workspace.
        Op::MoveWindowToWorkspaceDown(false),
    ]);

    assert_eq!(
        window_order(&layout),
        vec![1, 3],
        "the focused leaf (2) must be the one that left the section, not window 3"
    );
}


#[test]
fn remove_leaf_that_flattens_nesting_keeps_the_active_window() {
    // Bug 5: root V[H[1,3], 2] with window 2 active (root child 1). Closing window 1 collapses
    // H[1,3] to a single leaf and simplify flattens the tree to V[3,2]. The active-index fixup keys
    // off a pre-removal flat index, so it must decide against the *pre-removal* shape — the active
    // window has to stay 2, not silently become 3.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::FocusWindow(1),
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::FocusWindow(2),
        Op::CloseWindow(1),
    ]);

    assert_eq!(window_order(&layout), vec![3, 2], "the tree flattened to V[3,2]");
    assert_eq!(
        active_window_id(&layout),
        Some(2),
        "the active window must stay 2 after the removal flattened the nesting"
    );
}


#[test]
fn set_window_width_resizes_nested_main_split_child() {
    // Bug 2: root H[1, V[2,3]]. Resizing window 3's width used to index the root's `data` with the
    // flat leaf index 2 (len-2 Vec) and panic. It must instead resize the nearest Main-split
    // ancestor's child slot — here the whole V[2,3] column — so windows 2 and 3 (sharing the column)
    // both change width together and window 1 gets the rest.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(3) },
        // Grow window 3's column to 75% of the working area.
        Op::SetWindowWidth { id: Some(3), change: SizeChange::SetProportion(75.) },
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);

    let (_, s1) = window_geo(&layout, 1).unwrap();
    let (_, s2) = window_geo(&layout, 2).unwrap();
    let (_, s3) = window_geo(&layout, 3).unwrap();
    assert!(
        (s2.w - s3.w).abs() < 1.,
        "windows 2 and 3 share the resized column, so their widths match: {} vs {}",
        s2.w,
        s3.w
    );
    assert!(
        s3.w > s1.w + 50.,
        "the resized column (2/3) must be wider than window 1: col={}, win1={}",
        s3.w,
        s1.w
    );
}


#[test]
fn root_tab_hit_maps_group_tab_to_its_representative_leaf() {
    // Bug 3: root Stacked[1, H[2,3]] — two drawn tabs (one per root child) but three leaves. The
    // root header hit-test must use the child count and map each tab to its child's representative
    // leaf; window 2 (buried inside the group tab, whose representative leaf is 3) must never be a
    // tab-indicator target, or clicking the drawn group tab activates the wrong window.
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: wide_window(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: wide_window(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: wide_window(3) },
        Op::FocusWindow(1),
        Op::SetLayout(super::tile_node::Layout::Stacked),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::AdvanceAnimations { msec_delta: 1000 },
    ]);
    let output = layout.outputs().next().unwrap().clone();
    layout.update_render_elements(Some(&output));

    let mut tab_hits = std::collections::BTreeSet::new();
    for x in (0..1280).step_by(4) {
        for y in (0..720).step_by(4) {
            if let Some((w, HitType::Activate { is_tab_indicator: true })) =
                layout.window_under(&output, Point::from((x as f64, y as f64)))
            {
                tab_hits.insert(*w.id());
            }
        }
    }

    assert!(
        tab_hits.contains(&3),
        "the group tab must be hittable and resolve to its representative leaf (3): {tab_hits:?}"
    );
    assert!(
        !tab_hits.contains(&2),
        "window 2 is buried inside the group tab and must never be a root-header target: {tab_hits:?}"
    );
}


/// Helper: window ids grouped by section (left-to-right), each section in tree/leaf order.
fn section_window_groups(layout: &Layout<TestWindow>) -> Vec<Vec<usize>> {
    layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .sections()
        .map(|s| s.tiles().map(|(t, _)| *t.window().id()).collect())
        .collect()
}

#[test]
fn cross_section_swap_removes_the_focused_target_leaf() {
    // Bug 4: target section (left) = H[1,2,3,4] with window 4 active; source section (right) = a
    // single focused window 5. Swapping left must exchange window 5 with the target's *active*
    // window (4) — not eject a tile via `target_tile_idx + 1` (a SplitH-root wrap doesn't shift the
    // target's flat index) and not write an out-of-range root active_idx via
    // `set_active_tile_idx(flat)`.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SplitWindow(niri_ipc::SplitDirection::Main),
        Op::AddWindow { params: TestWindowParams::new(4) },
        // A fresh single-window section to the right, which stays focused.
        Op::AddWindow { params: TestWindowParams::new(5) },
        Op::SwapWindowInDirection(ScrollDirection::Left),
    ]);

    let groups = section_window_groups(&layout);
    assert_eq!(
        groups.iter().map(Vec::len).sum::<usize>(),
        5,
        "no window lost in the swap: {groups:?}"
    );
    assert!(
        groups.iter().any(|g| g == &[4]),
        "the target's active window (4) is swapped out into its own section: {groups:?}"
    );
    let grouped = groups.iter().find(|g| g.len() > 1).expect("a multi-window section remains");
    for id in [1, 2, 3, 5] {
        assert!(
            grouped.contains(&id),
            "window {id} stays in the grouped section (4 was the one swapped out): {groups:?}"
        );
    }
}


#[test]
fn set_section_display_normal_simplifies_untabbed_root() {
    // Bug 6b: Stacked[1, V[2,3]] with the root's prev_split = SplitV. Setting the display back to
    // Normal un-tabs the root to SplitV, which then directly contains the inner SplitV. The un-tab
    // must simplify (merge to V[1,2,3]); otherwise the same-family nesting trips verify_structure.
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow { params: TestWindowParams::new(1) },
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(2) },
        Op::SetLayout(super::tile_node::Layout::Stacked),
        Op::SplitWindow(niri_ipc::SplitDirection::Cross),
        Op::AddWindow { params: TestWindowParams::new(3) },
        Op::SetSectionDisplay(SectionDisplay::Normal),
    ]);

    assert_eq!(
        window_order(&layout),
        vec![1, 2, 3],
        "the un-tabbed root merged its same-family nesting into a flat V[1,2,3]"
    );
}

#[test]
fn toggle_tabbed_untab_of_nested_born_tab_simplifies() {
    // Bug 6a: a tab born via a centre drop defaults its prev_split to SplitV. Dropping window 3 onto
    // window 2's centre (detach mode) builds V[1, Tabbed[2,3]]. Un-tabbing that nested tab with
    // ToggleTabbed goes through toggle_tabbed's nested branch and yields SplitV directly inside the
    // root SplitV — which must be simplified to V[1,2,3], or verify_structure asserts.
    let mut options = Options::default();
    options.layout.tiling_drag = niri_config::TilingDrag::Detach;
    let mut layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: wide_window(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: wide_window(3) },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
            // Detach window 3 and lift it away from its slot.
            Op::InteractiveMoveBegin { window: 3, output_idx: 1, px: 640., py: 600. },
            Op::InteractiveMoveUpdate { window: 3, dx: 0., dy: -400., output_idx: 1, px: 640., py: 200. },
        ],
    );

    // With window 3 detached, drop it on the centre of window 2 to form a born tab group.
    let (p2, s2) = window_geo(&layout, 2).unwrap();
    let (cx, cy) = (p2.x + s2.w / 2., p2.y + s2.h / 2.);
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveUpdate { window: 3, dx: 0., dy: 0., output_idx: 1, px: cx, py: cy },
            Op::InteractiveMoveEnd { window: 3 },
            Op::Communicate(1),
            Op::Communicate(2),
            Op::Communicate(3),
            Op::AdvanceAnimations { msec_delta: 1000 },
            // Un-tab the nested born tab group.
            Op::ToggleTabbed,
        ],
    );

    assert_eq!(
        window_order(&layout),
        vec![1, 2, 3],
        "un-tabbing the nested born tab merged into a flat V[1,2,3]"
    );
}


/// Helper: the `pending_activated` flag of a window (what `set_activated` writes).
fn window_pending_activated(layout: &Layout<TestWindow>, id: usize) -> bool {
    layout
        .windows()
        .find(|(_, w)| *w.id() == id)
        .unwrap()
        .1
         .0
        .pending_activated
        .get()
}

#[test]
fn refresh_activates_the_focused_nested_leaf() {
    // Bug 7a: root V[1, H[2,3]] with window 3 active (root child index 1, flat leaf index 2). With
    // deactivate_unfocused_windows, refresh must mark the actually-focused leaf (3) active — not the
    // tile sitting at the root-child index (window 2).
    let options = Options {
        deactivate_unfocused_windows: true,
        ..Options::default()
    };
    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow { params: TestWindowParams::new(1) },
            Op::SplitWindow(niri_ipc::SplitDirection::Cross),
            Op::AddWindow { params: TestWindowParams::new(2) },
            Op::SplitWindow(niri_ipc::SplitDirection::Main),
            Op::AddWindow { params: TestWindowParams::new(3) },
            Op::Refresh { is_active: true },
        ],
    );

    assert!(
        window_pending_activated(&layout, 3),
        "the focused nested leaf (3) must be activated"
    );
    assert!(
        !window_pending_activated(&layout, 2),
        "its sibling (2, at the root-child index) must not be activated"
    );
    assert!(
        !window_pending_activated(&layout, 1),
        "the other section leaf (1) must not be activated"
    );
}

