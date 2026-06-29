use std::iter::zip;
use std::rc::Rc;

use niri_ipc::ColumnDisplay;
use ordered_float::NotNan;
use smithay::utils::{Logical, Point, Size};

use super::axis::AxisMap;
use super::tab_indicator::TabHeader;
use super::tile::Tile;
use super::{LayoutElement, Options};
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

/// Per-leaf layout metadata produced by [`TileNode::leaf_layout`].
///
/// `tile` is a raw pointer so callers can capture geometry before a subsequent mutable walk
/// without fighting the borrow checker; it is only valid for as long as the tree is unchanged.
pub struct LeafLayout<W: LayoutElement> {
    pub tile: *const Tile<W>,
    pub pos: Point<f64, Logical>,
    /// The leaf's main-axis size (axis-mapped width), used for centering.
    pub main_size: f64,
    /// Whether the leaf is being interactively resized by its start edge.
    pub resizing_by_start: bool,
    /// Whether the leaf shares the column's main-axis origin (no Main split ancestor).
    pub aligned: bool,
}

/// A recursive tile tree node.
///
/// A column's root is a `TileNode`. In Phase 1, the tree is effectively flat: the root is
/// either a `Leaf` (single window), a `Split { axis: Cross }` (normal column), or a
/// `Tabbed` node (tabbed column). Later phases introduce `Split { axis: Main }` and
/// nested structures.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
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
        tab_header: TabHeader,
        /// The split axis to restore when this node is un-tabbed. A tabbed container is
        /// axis-agnostic (children overlap), but it was created from a split along some axis
        /// — `Cross` for a normal column, `Main` for a side-by-side row — and toggling back
        /// should return to that arrangement rather than always collapsing to a column.
        restore_axis: SplitAxis,
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
        tab_header: TabHeader,
    ) -> Self {
        let data = tiles.iter().map(|_| SplitChildData::new_auto()).collect();
        let children = tiles.into_iter().map(TileNode::Leaf).collect();
        TileNode::Tabbed {
            children,
            active_idx,
            data,
            tab_header,
            // A freshly-tabbed column is conceptually a cross-axis stack.
            restore_axis: SplitAxis::Cross,
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

    /// Returns true if any direct child is a Split or Tabbed (i.e. nesting exists).
    pub fn has_nested_children(&self) -> bool {
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                children.iter().any(|c| !matches!(c, TileNode::Leaf(_)))
            }
        }
    }

    /// Returns the offset of the active leaf (recursive).
    /// `origin` is the starting position, `gaps` is the inter-tile gap.
    #[allow(clippy::only_used_in_recursion)]
    pub fn active_leaf_offset(
        &self,
        origin: Point<f64, Logical>,
        gaps: f64,
        scale: f64,
        axis: AxisMap,
        is_root: bool,
    ) -> Point<f64, Logical> {
        match self {
            TileNode::Leaf(_) => origin,
            TileNode::Split { axis: split_axis, children, data, active_idx } => {
                let is_main = *split_axis == SplitAxis::Main;
                // Adjust origin for siblings before the active child.
                let mut offset = origin;
                for (i, d) in data.iter().enumerate() {
                    if i == *active_idx {
                        break;
                    }
                    if is_main {
                        offset.x += d.size.w + gaps;
                    } else {
                        offset.y += d.size.h + gaps;
                    }
                }
                children[*active_idx].active_leaf_offset(offset, gaps, scale, axis, false)
            }
            TileNode::Tabbed { children, active_idx, tab_header, .. } => {
                // All children share the same position (tabbed). A nested tabbed container also
                // reserves a header band, so its content is pushed past it (the root column's
                // header offset is applied separately by `tiles_origin`).
                let content_origin = if is_root {
                    origin
                } else {
                    origin + tab_header.content_offset(children.len(), scale)
                };
                children[*active_idx].active_leaf_offset(content_origin, gaps, scale, axis, false)
            }
        }
    }

    /// Collects full layout metadata for every leaf in tree order (recursive).
    ///
    /// This is the single source of truth for leaf geometry. It computes each leaf's
    /// position by walking the tree, and additionally reports the leaf's main-axis size, its
    /// interactive-resize flag, and whether it is "main-axis aligned" — i.e. reachable from the
    /// root without crossing a Main split, so it shares the column's main-axis origin and is
    /// eligible for main-axis centering. Centering itself is applied by the caller (the column),
    /// which knows the relevant options.
    pub fn leaf_layout(&self, origin: Point<f64, Logical>, gaps: f64, scale: f64) -> Vec<LeafLayout<W>> {
        let mut out = Vec::new();
        self.collect_leaf_layout(origin, gaps, scale, true, true, &mut out);
        out
    }

    fn collect_leaf_layout(
        &self,
        origin: Point<f64, Logical>,
        gaps: f64,
        scale: f64,
        aligned: bool,
        is_root: bool,
        out: &mut Vec<LeafLayout<W>>,
    ) {
        match self {
            TileNode::Leaf(tile) => {
                // A bare leaf root: no parent split, so no main size/centering applies.
                out.push(LeafLayout {
                    tile: tile as *const _,
                    pos: origin,
                    main_size: 0.,
                    resizing_by_start: false,
                    aligned: false,
                });
            }
            TileNode::Split { axis: split_axis, children, data, .. } => {
                let is_main = *split_axis == SplitAxis::Main;
                // Children of a Main split are spread along the main axis, so they are no longer
                // aligned to the column's main origin.
                let child_aligned = aligned && !is_main;
                let mut pos = origin;
                for (i, child) in children.iter().enumerate() {
                    match child {
                        TileNode::Leaf(tile) => out.push(LeafLayout {
                            tile: tile as *const _,
                            pos,
                            main_size: data.get(i).map_or(0., |d| d.size.w),
                            resizing_by_start: data
                                .get(i)
                                .is_some_and(|d| d.interactively_resizing_by_start_edge),
                            aligned: child_aligned,
                        }),
                        _ => child.collect_leaf_layout(pos, gaps, scale, child_aligned, false, out),
                    }
                    if i < data.len() {
                        if is_main {
                            pos.x += data[i].size.w + gaps;
                        } else {
                            pos.y += data[i].size.h + gaps;
                        }
                    }
                }
            }
            TileNode::Tabbed { children, data, tab_header, .. } => {
                // A nested tabbed container reserves a band for its own header (the root column's
                // header offset is applied separately, by `tiles_origin`). Push the children's
                // content down past that band so it doesn't render under the header.
                let content_origin = if is_root {
                    origin
                } else {
                    origin + tab_header.content_offset(children.len(), scale)
                };
                for (i, child) in children.iter().enumerate() {
                    match child {
                        TileNode::Leaf(tile) => out.push(LeafLayout {
                            tile: tile as *const _,
                            pos: content_origin,
                            main_size: data.get(i).map_or(0., |d| d.size.w),
                            resizing_by_start: data
                                .get(i)
                                .is_some_and(|d| d.interactively_resizing_by_start_edge),
                            aligned,
                        }),
                        _ => child.collect_leaf_layout(content_origin, gaps, scale, aligned, false, out),
                    }
                }
            }
        }
    }

    /// Recursively checks the tree's structural invariants (used by tests/verify):
    /// `data` and `children` stay the same length, `active_idx` is in range, no node is empty, and
    /// there are no redundant single-child wrappers around a non-leaf (those are always collapsed).
    pub fn verify_structure(&self) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, data, active_idx, .. }
            | TileNode::Tabbed { children, data, active_idx, .. } => {
                assert_eq!(
                    children.len(),
                    data.len(),
                    "children/data length mismatch ({} vs {})",
                    children.len(),
                    data.len()
                );
                assert!(!children.is_empty(), "a split/tabbed node must have children");
                assert!(
                    *active_idx < children.len(),
                    "active_idx {} out of range (len {})",
                    active_idx,
                    children.len()
                );
                if children.len() == 1 {
                    assert!(
                        matches!(children[0], TileNode::Leaf(_)),
                        "a single-child split/tabbed node must wrap a leaf (else it should collapse)"
                    );
                }
                for child in children {
                    assert!(child.leaf_count() >= 1, "a child subtree must be non-empty");
                    child.verify_structure();
                }
            }
        }
    }

    /// Returns, in leaf (tree) order, whether each leaf is currently visible.
    ///
    /// A leaf is hidden only if some `Tabbed` ancestor on its path shows a different tab. All
    /// children of a `Split` are visible, so a split that is itself a tab reveals all its leaves.
    pub fn leaf_visibility(&self) -> Vec<bool> {
        let mut out = Vec::new();
        self.collect_leaf_visibility(true, &mut out);
        out
    }

    fn collect_leaf_visibility(&self, visible: bool, out: &mut Vec<bool>) {
        match self {
            TileNode::Leaf(_) => out.push(visible),
            TileNode::Split { children, .. } => {
                for child in children {
                    child.collect_leaf_visibility(visible, out);
                }
            }
            TileNode::Tabbed { children, active_idx, .. } => {
                for (i, child) in children.iter().enumerate() {
                    child.collect_leaf_visibility(visible && i == *active_idx, out);
                }
            }
        }
    }

    /// Returns the active leaf (following active_idx down the tree).
    pub fn active_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                let idx = (*active_idx).min(children.len().saturating_sub(1));
                children[idx].active_leaf()
            }
        }
    }

    /// Returns the active leaf (mutable).
    pub fn active_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                let idx = (*active_idx).min(children.len().saturating_sub(1));
                children[idx].active_leaf_mut()
            }
        }
    }

    /// Returns the path to the active leaf.
    pub fn active_leaf_path(&self) -> TilePath {
        match self {
            TileNode::Leaf(_) => Vec::new(),
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                let idx = (*active_idx).min(children.len().saturating_sub(1));
                let mut path = vec![idx];
                path.extend(children[idx].active_leaf_path());
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
                    if let TileNode::Leaf(_tile) = &children[idx] {
                        let tile = children.remove(idx).into_leaf();
                        data.remove(idx);

                        // Adjust active_idx.
                        if idx < *active_idx {
                            *active_idx -= 1;
                        } else if idx == *active_idx {
                            if *active_idx >= children.len() {
                                // Removed the last child (or active_idx was stale); activate the new last.
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
                        // Clamp active_idx to valid range in case it was stale.
                        if !children.is_empty() {
                            *active_idx = (*active_idx).min(children.len() - 1);
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

    /// Returns the tab header if this is a tabbed node.
    pub fn tab_header(&self) -> Option<&TabHeader> {
        match self {
            TileNode::Tabbed { tab_header, .. } => Some(tab_header),
            _ => None,
        }
    }

    /// Returns the tab header (mutable) if this is a tabbed node.
    pub fn tab_header_mut(&mut self) -> Option<&mut TabHeader> {
        match self {
            TileNode::Tabbed { tab_header, .. } => Some(tab_header),
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
                tab_header,
                ..
            } => {
                for child in children {
                    child.advance_animations();
                }
                tab_header.advance_animations();
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
                tab_header,
                ..
            } => {
                tab_header.are_animations_ongoing()
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
                tab_header,
                ..
            } => {
                tab_header.are_animations_ongoing()
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
                tab_header,
                ..
            } => {
                for child in children {
                    child.update_shaders();
                }
                tab_header.update_shaders();
            }
        }
    }

    /// Updates config for all tiles in this subtree.
    pub fn update_config_tiles(
        &mut self,
        tile_view_size: Size<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
        axis: AxisMap,
    ) {
        match self {
            TileNode::Leaf(tile) => {
                tile.update_config(tile_view_size, scale, options);
            }
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                for (child, d) in zip(children, data) {
                    child.update_config_tiles(tile_view_size, scale, options.clone(), axis);
                    // Update data for leaf children.
                    if let TileNode::Leaf(tile) = child {
                        d.update(tile, axis);
                    }
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

    /// Returns a reference to the data for the leaf at the given path.
    pub fn leaf_data(&self, path: &[usize]) -> Option<&SplitChildData> {
        if path.is_empty() {
            return None;
        }
        match self {
            TileNode::Leaf(_) => None,
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                let idx = path[0];
                if path.len() == 1 {
                    data.get(idx)
                } else {
                    children.get(idx).and_then(|c| c.leaf_data(&path[1..]))
                }
            }
        }
    }

    /// Updates the size and resize state for the leaf at the given path.
    pub fn update_leaf_data(&mut self, path: &[usize], size: Size<f64, Logical>, resizing_by_start: bool) {
        if path.is_empty() {
            return;
        }
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                let idx = path[0];
                if path.len() == 1 {
                    if let Some(d) = data.get_mut(idx) {
                        d.size = size;
                        d.interactively_resizing_by_start_edge = resizing_by_start;
                    }
                } else if let Some(child) = children.get_mut(idx) {
                    child.update_leaf_data(&path[1..], size, resizing_by_start);
                }
            }
        }
    }

    /// Updates the span for the leaf at the given path.
    pub fn update_leaf_span(&mut self, path: &[usize], span: ChildSpan) {
        if path.is_empty() {
            return;
        }
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, data, .. } | TileNode::Tabbed { children, data, .. } => {
                let idx = path[0];
                if path.len() == 1 {
                    if let Some(d) = data.get_mut(idx) {
                        d.span = span;
                    }
                } else if let Some(child) = children.get_mut(idx) {
                    child.update_leaf_span(&path[1..], span);
                }
            }
        }
    }

    /// Collects the paths of every `Tabbed` node in the subtree (including this node if it is one),
    /// in pre-order. `prefix` is the path of `self`.
    pub fn collect_tabbed_paths(&self, prefix: &mut TilePath, out: &mut Vec<TilePath>) {
        if matches!(self, TileNode::Tabbed { .. }) {
            out.push(prefix.clone());
        }
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    prefix.push(i);
                    child.collect_tabbed_paths(prefix, out);
                    prefix.pop();
                }
            }
        }
    }

    /// Returns the path to the active leaf, following `active_idx` down the tree. Empty if the
    /// root is itself a leaf.
    pub fn active_path(&self) -> TilePath {
        let mut path = Vec::new();
        let mut node = self;
        loop {
            match node {
                TileNode::Leaf(_) => break,
                TileNode::Split { children, active_idx, .. }
                | TileNode::Tabbed { children, active_idx, .. } => {
                    let idx = (*active_idx).min(children.len().saturating_sub(1));
                    path.push(idx);
                    node = &children[idx];
                }
            }
        }
        path
    }

    /// Returns the flat leaf index of the active leaf (following active_idx down the tree).
    pub fn path_for_leaf_index_from_active(&self) -> Option<usize> {
        let mut count = 0;
        self.find_active_leaf_index(&mut count)
    }

    fn find_active_leaf_index(&self, idx: &mut usize) -> Option<usize> {
        match self {
            TileNode::Leaf(_) => {
                let result = Some(*idx);
                *idx += 1;
                result
            }
            TileNode::Split { children, active_idx, .. }
            | TileNode::Tabbed { children, active_idx, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if i == *active_idx {
                        return child.find_active_leaf_index(idx);
                    } else {
                        *idx += child.leaf_count();
                    }
                }
                None
            }
        }
    }

    /// Returns the path to the Nth leaf (flat index → path).
    pub fn path_for_leaf_index(&self, idx: usize) -> Option<TilePath> {
        let mut current = idx;
        self.path_for_leaf_index_inner(&mut current)
    }

    fn path_for_leaf_index_inner(&self, idx: &mut usize) -> Option<TilePath> {
        match self {
            TileNode::Leaf(_) => {
                if *idx == 0 {
                    Some(Vec::new())
                } else {
                    *idx -= 1;
                    None
                }
            }
            TileNode::Split { children, .. } | TileNode::Tabbed { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if let Some(mut path) = child.path_for_leaf_index_inner(idx) {
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
    pub fn toggle_tabbed(&mut self, tab_header_config: niri_config::TabHeaderConfig) {
        match self {
            TileNode::Tabbed {
                children,
                active_idx,
                data,
                restore_axis,
                ..
            } => {
                // Convert back to a split along the axis we were created from (a column for a
                // tabbed column, a row for a tabbed row).
                let children = std::mem::take(children);
                let data = std::mem::take(data);
                let active_idx = *active_idx;
                *self = TileNode::Split {
                    axis: *restore_axis,
                    children,
                    active_idx,
                    data,
                };
            }
            TileNode::Split {
                axis,
                children,
                active_idx,
                data,
            } => {
                let tab_header = TabHeader::new(tab_header_config);
                let restore_axis = *axis;
                let children = std::mem::take(children);
                let data = std::mem::take(data);
                let active_idx = *active_idx;
                *self = TileNode::Tabbed {
                    children,
                    active_idx,
                    data,
                    tab_header,
                    restore_axis,
                };
            }
            TileNode::Leaf(_) => {
                // Can't toggle a leaf; this should be handled at the column level.
            }
        }
    }

    /// Sets the display mode to tabbed or normal.
    pub fn set_display(&mut self, display: ColumnDisplay, tab_header_config: niri_config::TabHeaderConfig) {
        if self.display_mode() == display {
            return;
        }
        self.toggle_tabbed(tab_header_config);
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

    /// Recursively collapses single-child splits/tabs and removes empty children in this subtree.
    pub fn collapse_all_single_child(&mut self) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Split { children, data, active_idx, .. }
            | TileNode::Tabbed { children, data, active_idx, .. } => {
                // First, recurse into children.
                for child in children.iter_mut() {
                    child.collapse_all_single_child();
                }
                // Remove empty children (can happen when a nested split's last child was removed).
                let mut i = 0;
                while i < children.len() {
                    let is_empty = matches!(&children[i], TileNode::Split { children: c, .. } | TileNode::Tabbed { children: c, .. } if c.is_empty());
                    if is_empty {
                        children.remove(i);
                        data.remove(i);
                        if *active_idx > i {
                            *active_idx -= 1;
                        } else if *active_idx == i && !children.is_empty() {
                            *active_idx = (*active_idx).min(children.len() - 1);
                        }
                    } else {
                        i += 1;
                    }
                }
                // Then collapse self if single child.
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
            TileNode::Split { children: _, data, .. } | TileNode::Tabbed { children: _, data, .. } => {
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

    /// Recursively request tile sizes for all leaves in this subtree.
    ///
    /// `available` is the total size allocated to this node (in axis-mapped coordinates:
    /// main = w, cross = h). For a Leaf, this directly requests the tile size. For a Split,
    /// it distributes the appropriate axis span among children and recurses. For a Tabbed
    /// node, all children get the same span.
    ///
    /// `gaps` is the inter-tile gap. `axis` is the axis map for coordinate conversion.
    /// `transaction` is the transaction to use; pass `None` for hidden (non-active tabbed) children.
    pub fn request_sizes(
        &mut self,
        available: Size<f64, Logical>,
        gaps: f64,
        axis: AxisMap,
        scale: f64,
        animate: bool,
        transaction: Option<&Transaction>,
    ) {
        match self {
            TileNode::Leaf(tile) => {
                // Clamp to positive values to avoid panics in to_i32_floor.
                let size = Size::from((available.w.max(1.), available.h.max(1.)));
                tile.request_tile_size(axis.size_out(size), animate, transaction.cloned());
            }
            TileNode::Split { axis: split_axis, children, data, active_idx } => {
                let count = children.len();
                if count == 0 {
                    return;
                }

                let is_main = *split_axis == SplitAxis::Main;

                // Determine which axis we're distributing along.
                // For a Main split: distribute along w (main axis), each child gets full h.
                // For a Cross split: distribute along h (cross axis), each child gets full w.
                //
                // Only the gaps *between* children are taken from `available`: the outer gaps are
                // already accounted for at the column level (the cross `available` is the working
                // area minus its two edge gaps; the main `available` is the column width, which is
                // defined as sum-of-children + between-gaps). Positioning (`leaf_layout`) likewise
                // places only between-children gaps, so the two must agree to reach a steady state.
                let gap_total = gaps * count.saturating_sub(1) as f64;

                let (distributable, per_child_other);
                if is_main {
                    distributable = (available.w - gap_total).max(1.);
                    per_child_other = available.h;
                } else {
                    distributable = (available.h - gap_total).max(1.);
                    per_child_other = available.w;
                }

                // Collect min sizes for clamping.
                let min_spans: Vec<f64> = children.iter().map(|c| {
                    let (min_main, _) = c.aggregate_min_max_main_span(axis);
                    if is_main { min_main } else {
                        c.min_cross_span_subtree(axis).max(1.)
                    }
                }).collect();

                assert_eq!(data.len(), count, "data.len ({}) != children.len ({}) in {:?} split", data.len(), count, split_axis);

                // Resolved span per child, and whether it has been pinned to a fixed value yet.
                // Auto/Preset children start unresolved and get distributed below; fixed children
                // (and any auto child whose minimum exceeds its weighted share) are pinned.
                let mut spans: Vec<f64> = vec![0.; count];
                let mut resolved: Vec<bool> = vec![false; count];
                let mut span_left = distributable;
                let mut total_weight: f64 = 0.;

                // Per-child weight for auto/preset distribution (presets are treated as auto-1
                // for now). Fixed children carry no weight.
                let weights: Vec<f64> = data
                    .iter()
                    .map(|d| match d.span {
                        ChildSpan::Auto { weight } => weight,
                        ChildSpan::Preset(_) => 1.,
                        ChildSpan::Fixed(_) => 0.,
                    })
                    .collect();

                for (i, d) in data.iter().enumerate() {
                    if let ChildSpan::Fixed(span) = d.span {
                        let s = span.max(min_spans[i]).round().max(1.);
                        spans[i] = s;
                        resolved[i] = true;
                        span_left -= s;
                    } else {
                        total_weight += weights[i];
                    }
                }

                // Iteratively distribute the remaining span among auto children, honoring each
                // child's minimum. If a child's weighted share is below its minimum, pin it to the
                // minimum and re-run, since the other children now have less to share. This mirrors
                // the flat-column algorithm in `update_tile_sizes` so nested splits reach the same
                // steady state. Spans are rounded to integer logical pixels (Wayland requirement),
                // which also guarantees the committed tile size matches the cached span exactly.
                let mut auto_left = count - resolved.iter().filter(|r| **r).count();
                'outer: while auto_left > 0 {
                    let mut remaining = span_left;
                    let mut remaining_weight = total_weight;
                    for i in 0..count {
                        if resolved[i] {
                            continue;
                        }
                        let weight = weights[i];
                        let factor = if remaining_weight > 0. {
                            weight / remaining_weight
                        } else {
                            1. / auto_left as f64
                        };
                        let share = remaining * factor;
                        if min_spans[i] > share {
                            let s = min_spans[i].round().max(1.);
                            spans[i] = s;
                            resolved[i] = true;
                            span_left -= s;
                            total_weight -= weight;
                            auto_left -= 1;
                            continue 'outer;
                        }
                        let s = share.round().max(1.);
                        spans[i] = s;
                        remaining -= s;
                        remaining_weight -= weight;
                    }

                    // All minimums satisfied: pin the computed spans.
                    for i in 0..count {
                        if resolved[i] {
                            continue;
                        }
                        resolved[i] = true;
                        span_left -= spans[i];
                        total_weight -= weights[i];
                        auto_left -= 1;
                    }
                    debug_assert_eq!(auto_left, 0);
                }

                // Now recurse into each child with its allocated span.
                for (i, child) in children.iter_mut().enumerate() {
                    let child_span = spans[i];
                    let child_size = if is_main {
                        Size::from((child_span, per_child_other))
                    } else {
                        Size::from((per_child_other, child_span))
                    };
                    // Store the computed size in data for position computation.
                    data[i].size = child_size;

                    child.request_sizes(child_size, gaps, axis, scale, animate, transaction);
                }

                // Ensure active_idx is valid.
                if *active_idx >= count {
                    *active_idx = 0;
                }
            }
            TileNode::Tabbed { children, data, active_idx, tab_header, .. } => {
                let count = children.len();
                if count == 0 {
                    return;
                }

                // All children get the same span.
                let extra = tab_header.extra_size(count, scale);
                let child_cross = (available.h - extra.h).max(1.);
                let child_size = Size::from((available.w, child_cross));

                for (i, child) in children.iter_mut().enumerate() {
                    data[i].size = child_size;
                    let is_active = i == *active_idx;
                    // In tabbed mode, only the active child participates in the transaction.
                    let child_txn = if is_active { transaction } else { None };
                    child.request_sizes(child_size, gaps, axis, scale, animate, child_txn);
                }
            }
        }
    }

    /// Returns the minimum cross-axis span of this subtree (used for layout clamping).
    fn min_cross_span_subtree(&self, axis: AxisMap) -> f64 {
        match self {
            TileNode::Leaf(tile) => {
                axis.size_in(tile.min_size_nonfullscreen()).h
            }
            TileNode::Split { children, axis: split_axis, .. } => {
                if *split_axis == SplitAxis::Main {
                    // Main split: children share the cross span, so min is the max of children.
                    children.iter()
                        .map(|c| c.min_cross_span_subtree(axis))
                        .max_by(|a, b| a.total_cmp(b))
                        .unwrap_or(1.)
                } else {
                    // Cross split: children stack along cross, so min is the sum.
                    children.iter()
                        .map(|c| c.min_cross_span_subtree(axis))
                        .sum::<f64>()
                        .max(1.)
                }
            }
            TileNode::Tabbed { children, .. } => {
                // Tabbed: all children share the cross span, so min is the max.
                children.iter()
                    .map(|c| c.min_cross_span_subtree(axis))
                    .max_by(|a, b| a.total_cmp(b))
                    .unwrap_or(1.)
            }
        }
    }
}
