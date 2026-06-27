use std::rc::Rc;

use niri_config::{CenterFocusedColumn, PresetSize};
use niri_ipc::{ColumnDisplay, WindowLayout};
use ordered_float::NotNan;
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::axis::AxisMap;
use super::tab_indicator::TabIndicator;
use super::tile::Tile;
use super::workspace::ResolvedSize;
use super::{LayoutElement, Options};
use crate::animation::Clock;
use crate::layout::SizingMode;
use crate::utils::transaction::Transaction;

/// Axis along which a split's children are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Along the main axis (horizontal in normal monitors).
    ///
    /// Inside a column, this creates side-by-side windows.
    Main,
    /// Along the cross axis (vertical in normal monitors).
    ///
    /// This is the existing column behavior — windows stacked vertically.
    Cross,
}

/// Path from a column root to a leaf, as a sequence of child indices.
pub type TilePath = Vec<usize>;

/// How a child's span is determined along its parent split's axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChildSpan {
    /// Automatically computed span, distributed across the split according to weights.
    Auto { weight: f64 },
    /// Fixed *tile* span in logical pixels.
    Fixed(f64),
    /// One of the preset spans.
    Preset(usize),
}

impl ChildSpan {
    pub const fn auto_1() -> Self {
        ChildSpan::Auto { weight: 1. }
    }
}

/// Extra per-child data stored alongside each child of a split or tabbed node.
///
/// `span` stores *tile* spans (including decorations), not window spans.
/// The existing `WindowHeight::Fixed(f64)` stored *window* spans converted via
/// `tile_cross_span_for_window_cross_span` during layout. During migration, `WindowHeight::Fixed`
/// values convert to tile spans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitChildData {
    /// Requested span of the child along the split's axis.
    pub span: ChildSpan,

    /// Cached actual size of the tile (for leaves) or aggregate (for subtrees).
    pub size: Size<f64, Logical>,

    /// Cached whether the child is being interactively resized by its start edge.
    pub interactively_resizing_by_start_edge: bool,
}

impl SplitChildData {
    pub fn new_auto() -> Self {
        Self {
            span: ChildSpan::auto_1(),
            size: Size::default(),
            interactively_resizing_by_start_edge: false,
        }
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>, axis: AxisMap) {
        self.size = axis.size_in(tile.tile_size());
        self.interactively_resizing_by_start_edge = tile
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(crate::utils::ResizeEdge::LEFT));
    }
}

/// A recursive tile tree node.
///
/// A column's root is a `TileNode`. In Phase 1, the tree is effectively flat: the root is
/// either a `Leaf` (single window), a `Split { axis: Cross }` (normal column), or a
/// `Tabbed` node (tabbed column). Later phases introduce `Split { axis: Main }` and
/// nested structures.
#[derive(Debug)]
pub enum TileNode<W: LayoutElement> {
    /// A single window.
    Leaf(Tile<W>),

    /// Children arranged along an axis, all visible.
    Split {
        axis: SplitAxis,
        children: Vec<TileNode<W>>,
        active_idx: usize,
        data: Vec<SplitChildData>,
    },

    /// Children as tabs, one visible at a time.
    Tabbed {
        children: Vec<TileNode<W>>,
        active_idx: usize,
        data: Vec<SplitChildData>,
        tab_indicator: TabIndicator,
    },
}

impl<W: LayoutElement> TileNode<W> {
    /// Creates a new leaf node.
    pub fn leaf(tile: Tile<W>) -> Self {
        TileNode::Leaf(tile)
    }

    /// Creates a new cross-axis split (existing column behavior) from a list of tiles.
    pub fn cross_split(tiles: Vec<Tile<W>>, active_idx: usize) -> Self {
        let data = tiles.iter().map(|_| SplitChildData::new_auto()).collect();
        let children = tiles.into_iter().map(TileNode::Leaf).collect();
        TileNode::Split {
            axis: SplitAxis::Cross,
            children,
            active_idx,
            data,
        }
    }

    /// Creates a new tabbed node from a list of tiles.
    pub fn tabbed(
        tiles: Vec<Tile<W>>,
        active_idx: usize,
        tab_indicator: TabIndicator,
    ) -> Self {
        let data = tiles.iter().map(|_| SplitChildData::new_auto()).collect();
        let children = tiles.into_iter().map(TileNode::Leaf).collect();
        TileNode::Tabbed {
            children,
            active_idx,
            data,
            tab_indicator,
        }
    }

    /// Whether this node is a tabbed container.
    pub fn is_tabbed(&self) -> bool {
        matches!(self, TileNode::Tabbed { .. })
    }

    /// Returns the number of leaves in this subtree.
    pub fn leaf_count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 1,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.iter().map(TileNode::leaf_count).sum()
            }
        }
    }

    /// Returns whether the subtree is empty (has no leaves).
    pub fn is_empty(&self) -> bool {
        self.leaf_count() == 0
    }

    /// Iterates over all leaves in this subtree with their paths.
    pub fn leaves(&self) -> impl Iterator<Item = (&Tile<W>, TilePath)> {
        let mut stack: Vec<(&TileNode<W>, TilePath)> = vec![(self, Vec::new())];
        std::iter::from_fn(move || {
            while let Some((node, path)) = stack.pop() {
                match node {
                    TileNode::Leaf(tile) => return Some((tile, path)),
                    TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                        for (i, child) in children.iter().enumerate().rev() {
                            let mut p = path.clone();
                            p.push(i);
                            stack.push((child, p));
                        }
                    }
                }
            }
            None
        })
    }

    /// Iterates over all leaves in this subtree with their paths (mutable).
    pub fn leaves_mut(&mut self) -> impl Iterator<Item = (&mut Tile<W>, TilePath)> {
        let mut stack: Vec<(*mut TileNode<W>, TilePath)> = vec![(self as *mut _, Vec::new())];
        std::iter::from_fn(move || {
            while let Some((node_ptr, path)) = stack.pop() {
                // SAFETY: we only push pointers derived from &mut self, and the borrow
                // checker ensures we don't alias them.
                let node = unsafe { &mut *node_ptr };
                match node {
                    TileNode::Leaf(tile) => return Some((tile, path)),
                    TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                        for (i, child) in children.iter_mut().enumerate().rev() {
                            let mut p = path.clone();
                            p.push(i);
                            stack.push((child as *mut _, p));
                        }
                    }
                }
            }
            None
        })
    }

    /// Returns the active leaf (following active_idx down the tree).
    pub fn active_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                children[*active_idx].active_leaf()
            }
        }
    }

    /// Returns the active leaf (mutable).
    pub fn active_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                children[*active_idx].active_leaf_mut()
            }
        }
    }

    /// Returns the path to the active leaf.
    pub fn active_leaf_path(&self) -> TilePath {
        match self {
            TileNode::Leaf(_) => Vec::new(),
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                let mut path = vec![*active_idx];
                path.extend(children[*active_idx].active_leaf_path());
                path
            }
        }
    }

    /// Returns the first leaf in this subtree (depth-first, first child).
    pub fn first_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children[0].first_leaf()
            }
        }
    }

    /// Returns the first leaf in this subtree (mutable).
    pub fn first_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children[0].first_leaf_mut()
            }
        }
    }

    /// Returns the last leaf in this subtree (depth-first, last child).
    pub fn last_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.last().unwrap().last_leaf()
            }
        }
    }

    /// Returns the last leaf in this subtree (mutable).
    pub fn last_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.last_mut().unwrap().last_leaf_mut()
            }
        }
    }

    /// Returns the node at the given path (immutable).
    pub fn node_at(&self, path: &[usize]) -> &Self {
        if path.is_empty() {
            return self;
        }
        match self {
            TileNode::Leaf(_) => self,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children[path[0]].node_at(&path[1..])
            }
        }
    }

    /// Returns the node at the given path (mutable).
    pub fn node_at_mut(&mut self, path: &[usize]) -> &mut Self {
        if path.is_empty() {
            return self;
        }
        match self {
            TileNode::Leaf(_) => self,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children[path[0]].node_at_mut(&path[1..])
            }
        }
    }

    /// Returns the leaf at the given path.
    pub fn leaf_at(&self, path: &[usize]) -> &Tile<W> {
        match self.node_at(path) {
            TileNode::Leaf(tile) => tile,
            _ => panic!("path does not point to a leaf"),
        }
    }

    /// Returns the leaf at the given path (mutable).
    pub fn leaf_at_mut(&mut self, path: &[usize]) -> &mut Tile<W> {
        match self.node_at_mut(path) {
            TileNode::Leaf(tile) => tile,
            _ => panic!("path does not point to a leaf"),
        }
    }

    /// Activates the leaf at the given path, updating active_idx at each level.
    /// Returns true if anything changed.
    pub fn activate_path(&mut self, path: &[usize]) -> bool {
        if path.is_empty() {
            return false;
        }
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                let changed = if *active_idx != path[0] {
                    *active_idx = path[0];
                    true
                } else {
                    false
                };
                // Also recurse if there's more path.
                let child_changed = children[path[0]].activate_path(&path[1..]);
                if changed {
                    // Ensure the newly activated leaf animates to opaque.
                    children[path[0]].active_leaf_mut().ensure_alpha_animates_to_1();
                }
                changed || child_changed
            }
        }
    }

    /// Removes the leaf at the given path and returns it.
    ///
    /// Replicates the active_idx adjustment logic at each tree level: decrement active_idx
    /// for siblings before the removed child, re-activate if the active child was removed,
    /// and handle the last-child edge case.
    pub fn remove_leaf(&mut self, path: &[usize]) -> Option<Tile<W>> {
        if path.is_empty() {
            return None;
        }
        match self {
            TileNode::Leaf(_) => None,
            TileNode::Split { children, active_idx, data, .. }
            | TileNode::Tabbed { children, active_idx, data, .. } => {
                let idx = path[0];
                if idx >= children.len() {
                    return None;
                }

                // If this is the last path element, remove the child directly.
                if path.len() == 1 {
                    if let TileNode::Leaf(tile) = &children[idx] {
                        let tile = children.remove(idx).into_leaf();
                        data.remove(idx);

                        // Adjust active_idx.
                        if idx < *active_idx {
                            *active_idx -= 1;
                        } else if idx == *active_idx {
                            if *active_idx == children.len() {
                                // Removed the last child; activate the new last.
                                if !children.is_empty() {
                                    *active_idx = children.len() - 1;
                                    children[*active_idx]
                                        .active_leaf_mut()
                                        .ensure_alpha_animates_to_1();
                                }
                            } else {
                                // The active shifted to the next tile.
                                children[*active_idx]
                                    .active_leaf_mut()
                                    .ensure_alpha_animates_to_1();
                            }
                        }

                        // If only one child left and it's a leaf, reset its weight.
                        if children.len() == 1 {
                            if let (TileNode::Leaf(_), SplitChildData { span, .. }) =
                                (&children[0], &mut data[0])
                            {
                                if let ChildSpan::Auto { weight } = span {
                                    *weight = 1.;
                                }
                            }
                        }

                        return Some(tile);
                    }
                    // Not a leaf — recurse (shouldn't happen in Phase 1, but handle gracefully).
                    return children[idx].remove_leaf(&path[1..]);
                }

                // Recurse into the child.
                children[idx].remove_leaf(&path[1..])
            }
        }
    }

    /// Converts this node into a `Leaf`, panicking if it's not a leaf.
    fn into_leaf(self) -> Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            _ => panic!("expected a leaf node"),
        }
    }

    /// Returns the tab indicator if this is a tabbed node.
    pub fn tab_indicator(&self) -> Option<&TabIndicator> {
        match self {
            TileNode::Tabbed { tab_indicator, .. } => Some(tab_indicator),
            _ => None,
        }
    }

    /// Returns the tab indicator (mutable) if this is a tabbed node.
    pub fn tab_indicator_mut(&mut self) -> Option<&mut TabIndicator> {
        match self {
            TileNode::Tabbed { tab_indicator, .. } => Some(tab_indicator),
            _ => None,
        }
    }

    /// Advances animations for all tiles and tab indicators in this subtree.
    pub fn advance_animations(&mut self) {
        match self {
            TileNode::Leaf(tile) => tile.advance_animations(),
            TileNode::Split { children, .. } => {
                for child in children {
                    child.advance_animations();
                }
            }
            TileNode::Tabbed {
                children,
                tab_indicator,
                ..
            } => {
                for child in children {
                    child.advance_animations();
                }
                tab_indicator.advance_animations();
            }
        }
    }

    /// Returns whether any animations are ongoing in this subtree.
    pub fn are_animations_ongoing(&self) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.are_animations_ongoing(),
            TileNode::Split { children, .. } => {
                children.iter().any(TileNode::are_animations_ongoing)
            }
            TileNode::Tabbed {
                children,
                tab_indicator,
                ..
            } => {
                tab_indicator.are_animations_ongoing()
                    || children.iter().any(TileNode::are_animations_ongoing)
            }
        }
    }

    /// Returns whether any transitions are ongoing in this subtree.
    pub fn are_transitions_ongoing(&self) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.are_transitions_ongoing(),
            TileNode::Split { children, .. } => {
                children.iter().any(TileNode::are_transitions_ongoing)
            }
            TileNode::Tabbed {
                children,
                tab_indicator,
                ..
            } => {
                tab_indicator.are_animations_ongoing()
                    || children.iter().any(TileNode::are_transitions_ongoing)
            }
        }
    }

    /// Updates shaders for all tiles and tab indicators in this subtree.
    pub fn update_shaders(&mut self) {
        match self {
            TileNode::Leaf(tile) => tile.update_shaders(),
            TileNode::Split { children, .. } => {
                for child in children {
                    child.update_shaders();
                }
            }
            TileNode::Tabbed {
                children,
                tab_indicator,
                ..
            } => {
                for child in children {
                    child.update_shaders();
                }
                tab_indicator.update_shaders();
            }
        }
    }

    /// Updates config for all tiles in this subtree.
    pub fn update_config_tiles(
        &mut self,
        tile_view_size: Size<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        match self {
            TileNode::Leaf(tile) => {
                tile.update_config(tile_view_size, scale, options);
            }
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                for child in children {
                    child.update_config_tiles(tile_view_size, scale, options.clone());
                }
            }
        }
    }

    /// Updates the tile data for all leaves in this subtree.
    pub fn update_data(&mut self, axis: AxisMap) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                for (child, data) in children.iter_mut().zip(data.iter_mut()) {
                    if let TileNode::Leaf(tile) = child {
                        data.update(tile, axis);
                    }
                    // For non-leaf children, data.cached_size would need updating,
                    // but in Phase 1 all children are leaves.
                }
            }
        }
    }

    /// Returns whether this subtree contains the given window.
    pub fn contains(&self, window: &W::Id) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.window().id() == window,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.iter().any(|c| c.contains(window))
            }
        }
    }

    /// Finds the path to the leaf containing the given window.
    pub fn find_path(&self, window: &W::Id) -> Option<TilePath> {
        match self {
            TileNode::Leaf(tile) => {
                if tile.window().id() == window {
                    Some(Vec::new())
                } else {
                    None
                }
            }
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if let Some(mut path) = child.find_path(window) {
                        path.insert(0, i);
                        return Some(path);
                    }
                }
                None
            }
        }
    }

    /// Returns the index of the active child (for Split/Tabbed nodes).
    pub fn active_idx(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 0,
            TileNode::Split { active_idx, .. } | TileNode::Tabbed { active_idx, .. } => *active_idx,
        }
    }

    /// Returns the split axis if this is a split node.
    pub fn split_axis(&self) -> Option<SplitAxis> {
        match self {
            TileNode::Split { axis, .. } => Some(*axis),
            _ => None,
        }
    }

    /// Returns the display mode (Normal for Leaf/Split, Tabbed for Tabbed).
    pub fn display_mode(&self) -> ColumnDisplay {
        match self {
            TileNode::Tabbed { .. } => ColumnDisplay::Tabbed,
            _ => ColumnDisplay::Normal,
        }
    }

    /// Returns the number of direct children (for Split/Tabbed), or 0 for Leaf.
    pub fn child_count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 0,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.len()
            }
        }
    }

    /// Toggles between tabbed and normal display.
    pub fn toggle_tabbed(&mut self, tab_indicator_config: niri_config::TabIndicator) {
        match self {
            TileNode::Tabbed {
                children,
                active_idx,
                data,
                ..
            } => {
                // Convert to cross-axis split (the existing normal column behavior).
                let children = std::mem::take(children);
                let data = std::mem::take(data);
                let active_idx = *active_idx;
                *self = TileNode::Split {
                    axis: SplitAxis::Cross,
                    children,
                    active_idx,
                    data,
                };
            }
            TileNode::Split {
                children,
                active_idx,
                data,
                ..
            } => {
                let tab_indicator = TabIndicator::new(tab_indicator_config);
                let children = std::mem::take(children);
                let data = std::mem::take(data);
                let active_idx = *active_idx;
                *self = TileNode::Tabbed {
                    children,
                    active_idx,
                    data,
                    tab_indicator,
                };
            }
            TileNode::Leaf(_) => {
                // Can't toggle a leaf; this should be handled at the column level.
            }
        }
    }

    /// Sets the display mode to tabbed or normal.
    pub fn set_display(&mut self, display: ColumnDisplay, tab_indicator_config: niri_config::TabIndicator) {
        if self.display_mode() == display {
            return;
        }
        self.toggle_tabbed(tab_indicator_config);
    }

    /// Sets the active child index (for Split/Tabbed roots). No-op for Leaf.
    pub fn set_active_idx(&mut self, idx: usize) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { active_idx, .. } | TileNode::Tabbed { active_idx, .. } => {
                *active_idx = idx;
            }
        }
    }

    /// Inserts a leaf child at the given index (for Split/Tabbed roots).
    pub fn insert_leaf(&mut self, idx: usize, tile: Tile<W>, data: SplitChildData) {
        match self {
            TileNode::Leaf(_) => panic!("cannot insert into a Leaf node"),
            TileNode::Split { children, data: child_data, .. }
            | TileNode::Tabbed {
                children,
                data: child_data,
                ..
            } => {
                children.insert(idx, TileNode::Leaf(tile));
                child_data.insert(idx, data);
            }
        }
    }

    /// Removes the leaf child at the given flat index and returns it (for Split/Tabbed roots).
    pub fn remove_leaf_at(&mut self, idx: usize) -> Tile<W> {
        match self {
            TileNode::Leaf(_) => panic!("cannot remove from a Leaf node"),
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                data.remove(idx);
                match children.remove(idx) {
                    TileNode::Leaf(tile) => tile,
                    _ => panic!("expected a leaf child"),
                }
            }
        }
    }

    /// Swaps two leaf children at the given flat indices (for Split/Tabbed roots).
    pub fn swap_leaves(&mut self, a: usize, b: usize) {
        match self {
            TileNode::Leaf(_) => panic!("cannot swap in a Leaf node"),
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                children.swap(a, b);
                data.swap(a, b);
            }
        }
    }

    /// If this Split/Tabbed node has exactly one child, replace `self` with that child.
    /// This collapses single-child splits (i3 behavior: empty splits collapse).
    /// Returns `true` if a collapse occurred.
    pub fn collapse_single_child(&mut self) -> bool {
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                if children.len() == 1 {
                    let child = children.remove(0);
                    *self = child;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Recursively collapses all single-child splits/tabs in this subtree.
    pub fn collapse_all_single_child(&mut self) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                for child in children.iter_mut() {
                    child.collapse_all_single_child();
                }
                self.collapse_single_child();
            }
        }
    }

    /// Returns the maximum cached main-axis span among all leaves.
    pub fn max_leaf_main_span(&self) -> f64 {
        match self {
            TileNode::Leaf(tile) => {
                // This is in axis-mapped coordinates (w = main, h = cross).
                tile.tile_size().w
            }
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                // For Phase 1 (flat tree), all children are leaves, and the max main span
                // is the max of all children's cached sizes.
                data.iter()
                    .map(|d| NotNan::new(d.size.w).unwrap())
                    .max()
                    .map(NotNan::into_inner)
                    .unwrap_or(0.)
            }
        }
    }

    /// Computes the aggregate min/max main span from all leaves.
    /// Returns (min_main_span, max_main_span).
    pub fn aggregate_min_max_main_span(&self, axis: AxisMap) -> (f64, f64) {
        let mut min_span: f64 = f64::MAX;
        let mut max_span: f64 = 0.;
        for (tile, _) in self.leaves() {
            let min_size = axis.size_in(tile.min_size_nonfullscreen());
            let max_size = axis.size_in(tile.max_size_nonfullscreen());
            let min_w = min_size.w.max(1.);
            let max_w = if max_size.w == 0. {
                f64::from(i32::MAX)
            } else {
                max_size.w
            };
            min_span = min_span.min(min_w);
            max_span = max_span.max(max_w);
        }
        if min_span == f64::MAX {
            min_span = 1.;
        }
        max_span = max_span.max(min_span);
        (min_span, max_span)
    }
}
