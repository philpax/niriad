use std::iter::zip;
use std::rc::Rc;

use niri_ipc::SectionDisplay;
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
    /// Inside a section, this creates side-by-side windows.
    Main,
    /// Along the cross axis (vertical in normal monitors).
    ///
    /// This is the existing section behavior — windows stacked vertically.
    Cross,
}

/// How an internal node arranges its children. The four layouts are inspired by the tiling model
/// of sway/i3.
///
/// The four layouts fall into two *families* by axis: `SplitH`/`Tabbed` are the horizontal family
/// (main axis), `SplitV`/`Stacked` the vertical family (cross axis). Within a family, the split
/// shows all children while the tabbing layout shows one at a time — so for navigation a `Tabbed`
/// container behaves like `SplitH` and a `Stacked` one like `SplitV` (focusing "right" cycles tabs,
/// "down" cycles stacked entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Children side by side along the main axis, all visible (sway `L_HORIZ`).
    SplitH,
    /// Children stacked along the cross axis, all visible (sway `L_VERT`).
    SplitV,
    /// Children as tabs; one visible; a single row of side-by-side tab titles (sway `L_TABBED`).
    Tabbed,
    /// Children stacked; one visible; one title row per child (sway `L_STACKED`).
    Stacked,
}

impl Layout {
    /// The screen axis this layout arranges along (or, for a tabbing layout, the axis it navigates
    /// along and the split it collapses to when un-tabbed).
    pub fn axis(self) -> SplitAxis {
        match self {
            Layout::SplitH | Layout::Tabbed => SplitAxis::Main,
            Layout::SplitV | Layout::Stacked => SplitAxis::Cross,
        }
    }

    /// Whether this is a "tabbing" layout (Tabbed/Stacked): one child visible at a time, with a
    /// titlebar header.
    pub fn is_tabbing(self) -> bool {
        matches!(self, Layout::Tabbed | Layout::Stacked)
    }

    /// Whether this is a plain split (SplitH/SplitV): all children visible.
    pub fn is_split(self) -> bool {
        matches!(self, Layout::SplitH | Layout::SplitV)
    }

    /// Two layouts are in the same family if they share an axis (so they can be merged when nested).
    pub fn same_family(self, other: Layout) -> bool {
        self.axis() == other.axis()
    }

    /// The plain split layout of this layout's family (SplitH for the horizontal family, SplitV for
    /// the vertical family). Used when un-tabbing.
    pub fn split_of_family(self) -> Layout {
        match self.axis() {
            SplitAxis::Main => Layout::SplitH,
            SplitAxis::Cross => Layout::SplitV,
        }
    }

    /// The split layout for an axis.
    pub fn split_for_axis(axis: SplitAxis) -> Layout {
        match axis {
            SplitAxis::Main => Layout::SplitH,
            SplitAxis::Cross => Layout::SplitV,
        }
    }
}

/// Path from a section root to a leaf, as a sequence of child indices.
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
    /// Whether the leaf shares the section's main-axis origin (no Main split ancestor).
    pub aligned: bool,
}

/// A recursive tile tree node. The design draws on sway/i3's container model: a node is either a
/// `Leaf` (a single window) or an `Internal` node arranging children according to a [`Layout`].
///
/// This is the single internal node type — `SplitH`/`SplitV`/`Tabbed`/`Stacked` are all just
/// `layout` values; like sway and i3, tabbed and stacked are layouts rather than separate node
/// types.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TileNode<W: LayoutElement> {
    /// A single window.
    Leaf(Tile<W>),

    /// An internal node arranging its children per `layout`.
    Internal {
        /// How the children are arranged.
        layout: Layout,
        children: Vec<TileNode<W>>,
        active_idx: usize,
        data: Vec<SplitChildData>,
        /// Titlebar header, present iff `layout.is_tabbing()`.
        tab_header: Option<TabHeader>,
        /// The plain split layout to restore when leaving a tabbing layout (sway's
        /// `prev_split_layout`): tabbing a vertical section then un-tabbing returns a section, not a
        /// row. Only meaningful while `layout.is_tabbing()`.
        prev_split: Layout,
    },
}

impl<W: LayoutElement> TileNode<W> {
    /// Creates a new leaf node.
    pub fn leaf(tile: Tile<W>) -> Self {
        TileNode::Leaf(tile)
    }

    /// Creates an internal node from a list of child nodes with the given layout.
    pub fn internal(
        layout: Layout,
        children: Vec<TileNode<W>>,
        active_idx: usize,
        data: Vec<SplitChildData>,
        mut tab_header: Option<TabHeader>,
    ) -> Self {
        if let Some(h) = &mut tab_header {
            h.set_stacked(layout == Layout::Stacked);
        }
        TileNode::Internal {
            layout,
            children,
            active_idx,
            data,
            tab_header,
            // A plain split's prev_split is itself; a node born in a tabbing layout conceptually
            // came from a vertical section (niri's default), so un-tabbing it yields a section, not a
            // row. (Matches the old `restore_axis = Cross` default.)
            prev_split: if layout.is_split() {
                layout
            } else {
                Layout::SplitV
            },
        }
    }

    /// Creates a new cross-axis split (a normal vertical section) from a list of tiles.
    pub fn cross_split(tiles: Vec<Tile<W>>, active_idx: usize) -> Self {
        let data = tiles.iter().map(|_| SplitChildData::new_auto()).collect();
        let children = tiles.into_iter().map(TileNode::Leaf).collect();
        TileNode::internal(Layout::SplitV, children, active_idx, data, None)
    }

    /// Creates a new tabbed node from a list of tiles.
    pub fn tabbed(
        tiles: Vec<Tile<W>>,
        active_idx: usize,
        tab_header: TabHeader,
    ) -> Self {
        let data = tiles.iter().map(|_| SplitChildData::new_auto()).collect();
        let children = tiles.into_iter().map(TileNode::Leaf).collect();
        // A freshly-tabbed section is conceptually a vertical stack.
        TileNode::internal(Layout::Tabbed, children, active_idx, data, Some(tab_header))
    }

    /// The node's layout, or `None` for a leaf.
    pub fn layout(&self) -> Option<Layout> {
        match self {
            TileNode::Leaf(_) => None,
            TileNode::Internal { layout, .. } => Some(*layout),
        }
    }

    /// Whether this node is a tabbing container (Tabbed or Stacked).
    pub fn is_tabbed(&self) -> bool {
        matches!(self, TileNode::Internal { layout, .. } if layout.is_tabbing())
    }

    /// Returns the number of leaves in this subtree.
    pub fn leaf_count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 1,
            TileNode::Internal { children, .. } => {
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
                    TileNode::Internal { children, .. } => {
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
                    TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { layout, children, active_idx, tab_header, .. }
                if layout.is_tabbing() =>
            {
                // All children share the same position (one shown at a time). A nested tabbing
                // container reserves a header band, so its content is pushed past it (the root
                // section's header offset is applied separately by `tiles_origin`).
                let content_origin = if is_root {
                    origin
                } else {
                    tab_header
                        .as_ref()
                        .map_or(origin, |h| origin + h.content_offset(children.len(), scale))
                };
                children[*active_idx].active_leaf_offset(content_origin, gaps, scale, axis, false)
            }
            TileNode::Internal { layout, children, active_idx, data, .. } => {
                let is_main = layout.axis() == SplitAxis::Main;
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
        }
    }

    /// Collects full layout metadata for every leaf in tree order (recursive).
    ///
    /// This is the single source of truth for leaf geometry. It computes each leaf's
    /// position by walking the tree, and additionally reports the leaf's main-axis size, its
    /// interactive-resize flag, and whether it is "main-axis aligned" — i.e. reachable from the
    /// root without crossing a Main split, so it shares the section's main-axis origin and is
    /// eligible for main-axis centering. Centering itself is applied by the caller (the section),
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
            TileNode::Internal { layout, children, data, tab_header, .. }
                if layout.is_tabbing() =>
            {
                // A nested tabbing container reserves a band for its own header (the root section's
                // header offset is applied separately, by `tiles_origin`). Push the children's
                // content down past that band so it doesn't render under the header.
                let content_origin = if is_root {
                    origin
                } else {
                    tab_header
                        .as_ref()
                        .map_or(origin, |h| origin + h.content_offset(children.len(), scale))
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
            TileNode::Internal { layout, children, data, .. } => {
                let is_main = layout.axis() == SplitAxis::Main;
                // Children of a Main split are spread along the main axis, so they are no longer
                // aligned to the section's main origin.
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
        }
    }

    /// Recursively checks the tree's structural invariants (used by tests/verify):
    /// `data` and `children` stay the same length, `active_idx` is in range, no node is empty, and
    /// there are no redundant single-child wrappers around a non-leaf (those are always collapsed).
    pub fn verify_structure(&self) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Internal { layout, children, data, active_idx, .. } => {
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
                // No plain-split child of the same family as a plain-split parent (it must have been
                // merged: a section never directly contains a section, a row never a row).
                if layout.is_split() {
                    for child in children.iter() {
                        if let TileNode::Internal { layout: cl, .. } = child {
                            assert!(
                                !(cl.is_split() && cl.same_family(*layout)),
                                "same-family split nesting not merged: {layout:?} contains {cl:?}"
                            );
                        }
                    }
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
            TileNode::Internal { layout, children, active_idx, .. } => {
                // A tabbing layout shows only its active child; a plain split shows all.
                let tabbing = layout.is_tabbing();
                for (i, child) in children.iter().enumerate() {
                    let child_visible = visible && (!tabbing || i == *active_idx);
                    child.collect_leaf_visibility(child_visible, out);
                }
            }
        }
    }

    /// Returns the active leaf (following active_idx down the tree).
    pub fn active_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Internal { children, active_idx, .. } => {
                let idx = (*active_idx).min(children.len().saturating_sub(1));
                children[idx].active_leaf()
            }
        }
    }

    /// Returns the active leaf (mutable).
    pub fn active_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Internal { children, active_idx, .. } => {
                let idx = (*active_idx).min(children.len().saturating_sub(1));
                children[idx].active_leaf_mut()
            }
        }
    }

    /// Returns the path to the active leaf.
    pub fn active_leaf_path(&self) -> TilePath {
        match self {
            TileNode::Leaf(_) => Vec::new(),
            TileNode::Internal { children, active_idx, .. } => {
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
            TileNode::Internal { children, .. } => {
                children[0].first_leaf()
            }
        }
    }

    /// Returns the first leaf in this subtree (mutable).
    pub fn first_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Internal { children, .. } => {
                children[0].first_leaf_mut()
            }
        }
    }

    /// Returns the last leaf in this subtree (depth-first, last child).
    pub fn last_leaf(&self) -> &Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Internal { children, .. } => {
                children.last().unwrap().last_leaf()
            }
        }
    }

    /// Returns the last leaf in this subtree (mutable).
    pub fn last_leaf_mut(&mut self) -> &mut Tile<W> {
        match self {
            TileNode::Leaf(tile) => tile,
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, active_idx, .. } => {
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
            TileNode::Internal { children, active_idx, data, .. } => {
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
                    // Not a leaf — recurse into the nested node.
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

    /// Returns the tab header if this is a tabbing node.
    pub fn tab_header(&self) -> Option<&TabHeader> {
        match self {
            TileNode::Internal { tab_header, .. } => tab_header.as_ref(),
            _ => None,
        }
    }

    /// Returns the tab header (mutable) if this is a tabbing node.
    pub fn tab_header_mut(&mut self) -> Option<&mut TabHeader> {
        match self {
            TileNode::Internal { tab_header, .. } => tab_header.as_mut(),
            _ => None,
        }
    }

    /// Advances animations for all tiles and tab indicators in this subtree.
    pub fn advance_animations(&mut self) {
        match self {
            TileNode::Leaf(tile) => tile.advance_animations(),
            TileNode::Internal { children, tab_header, .. } => {
                for child in children {
                    child.advance_animations();
                }
                if let Some(h) = tab_header {
                    h.advance_animations();
                }
            }
        }
    }

    /// Returns whether any animations are ongoing in this subtree.
    pub fn are_animations_ongoing(&self) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.are_animations_ongoing(),
            TileNode::Internal { children, tab_header, .. } => {
                tab_header.as_ref().is_some_and(|h| h.are_animations_ongoing())
                    || children.iter().any(TileNode::are_animations_ongoing)
            }
        }
    }

    /// Returns whether any transitions are ongoing in this subtree.
    pub fn are_transitions_ongoing(&self) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.are_transitions_ongoing(),
            TileNode::Internal { children, tab_header, .. } => {
                tab_header.as_ref().is_some_and(|h| h.are_animations_ongoing())
                    || children.iter().any(TileNode::are_transitions_ongoing)
            }
        }
    }

    /// Updates shaders for all tiles and tab indicators in this subtree.
    pub fn update_shaders(&mut self) {
        match self {
            TileNode::Leaf(tile) => tile.update_shaders(),
            TileNode::Internal { children, tab_header, .. } => {
                for child in children {
                    child.update_shaders();
                }
                if let Some(h) = tab_header {
                    h.update_shaders();
                }
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
            TileNode::Internal { children, data, .. } => {
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
            TileNode::Internal { children, data, .. } => {
                for (child, data) in children.iter_mut().zip(data.iter_mut()) {
                    if let TileNode::Leaf(tile) = child {
                        data.update(tile, axis);
                    }
                    // For non-leaf children, data.cached_size would need updating,
                    // they are sized by request_sizes instead.
                }
            }
        }
    }

    /// Returns whether this subtree contains the given window.
    pub fn contains(&self, window: &W::Id) -> bool {
        match self {
            TileNode::Leaf(tile) => tile.window().id() == window,
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, data, .. } => {
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
            TileNode::Internal { children, data, .. } => {
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
            TileNode::Internal { children, data, .. } => {
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

    /// Collects the paths of every tabbing node (Tabbed/Stacked) in the subtree (including this node
    /// if it is one), in pre-order. `prefix` is the path of `self`.
    pub fn collect_tabbed_paths(&self, prefix: &mut TilePath, out: &mut Vec<TilePath>) {
        if self.is_tabbed() {
            out.push(prefix.clone());
        }
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Internal { children, .. } => {
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
                TileNode::Internal { children, active_idx, .. } => {
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
            TileNode::Internal { children, active_idx, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { active_idx, .. } => *active_idx,
        }
    }

    /// Returns the split axis if this is a plain split node (SplitH/SplitV).
    pub fn split_axis(&self) -> Option<SplitAxis> {
        match self {
            TileNode::Internal { layout, .. } if layout.is_split() => Some(layout.axis()),
            _ => None,
        }
    }

    /// Returns the display mode (Normal for Leaf/split, Tabbed for a tabbing layout).
    pub fn display_mode(&self) -> SectionDisplay {
        match self {
            TileNode::Internal { layout, .. } if layout.is_tabbing() => SectionDisplay::Tabbed,
            _ => SectionDisplay::Normal,
        }
    }

    /// Returns the number of direct children (for Split/Tabbed), or 0 for Leaf.
    pub fn child_count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 0,
            TileNode::Internal { children, .. } => {
                children.len()
            }
        }
    }

    /// Toggles between the `Tabbed` layout and a plain split. Tabbing a node remembers its split
    /// layout (`prev_split`) so un-tabbing returns to it (a tabbed section un-tabs to a section, a
    /// tabbed row to a row).
    pub fn toggle_tabbed(&mut self, tab_header_config: niri_config::TabHeaderConfig) {
        let TileNode::Internal { layout, tab_header, prev_split, .. } = self else {
            // Can't toggle a leaf; this should be handled at the section level.
            return;
        };
        if layout.is_tabbing() {
            *layout = *prev_split;
            *tab_header = None;
        } else {
            *prev_split = *layout;
            *layout = Layout::Tabbed;
            *tab_header = Some(TabHeader::new(tab_header_config));
        }
    }

    /// Sets this node's layout directly, creating/dropping the tab header as needed and remembering
    /// the previous split layout when entering a tabbing layout (inspired by sway's layout command:
    /// a flag flip on an existing container). No-op for a leaf.
    pub fn set_layout(&mut self, new: Layout, tab_header_config: niri_config::TabHeaderConfig) {
        let TileNode::Internal { layout, tab_header, prev_split, .. } = self else {
            return;
        };
        if *layout == new {
            return;
        }
        if layout.is_split() {
            *prev_split = *layout;
        }
        *layout = new;
        if new.is_tabbing() {
            if tab_header.is_none() {
                *tab_header = Some(TabHeader::new(tab_header_config));
            }
            if let Some(h) = tab_header {
                h.set_stacked(new == Layout::Stacked);
            }
        } else {
            *tab_header = None;
        }
    }

    /// Sets the display mode to tabbed or normal.
    pub fn set_display(&mut self, display: SectionDisplay, tab_header_config: niri_config::TabHeaderConfig) {
        if self.display_mode() == display {
            return;
        }
        self.toggle_tabbed(tab_header_config);
    }

    /// Sets the active child index (for Split/Tabbed roots). No-op for Leaf.
    pub fn set_active_idx(&mut self, idx: usize) {
        match self {
            TileNode::Leaf(_) => {}
            TileNode::Internal { active_idx, .. } => {
                *active_idx = idx;
            }
        }
    }

    /// Inserts a leaf child at the given index (for Split/Tabbed roots).
    pub fn insert_leaf(&mut self, idx: usize, tile: Tile<W>, data: SplitChildData) {
        match self {
            TileNode::Leaf(_) => panic!("cannot insert into a Leaf node"),
            TileNode::Internal { children, data: child_data, .. } => {
                children.insert(idx, TileNode::Leaf(tile));
                child_data.insert(idx, data);
            }
        }
    }

    /// Removes the leaf child at the given flat index and returns it (for Split/Tabbed roots).
    pub fn remove_leaf_at(&mut self, idx: usize) -> Tile<W> {
        match self {
            TileNode::Leaf(_) => panic!("cannot remove from a Leaf node"),
            TileNode::Internal { children, data, .. } => {
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
            TileNode::Internal { children, data, .. } => {
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
            TileNode::Internal { children, .. } => {
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
            TileNode::Internal { children, data, active_idx, .. } => {
                // First, recurse into children.
                for child in children.iter_mut() {
                    child.collapse_all_single_child();
                }
                // Remove empty children (can happen when a nested split's last child was removed).
                let mut i = 0;
                while i < children.len() {
                    let is_empty = matches!(&children[i], TileNode::Internal { children: c, .. } if c.is_empty());
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

    /// Canonicalizes this subtree (bottom-up), enforcing tree-shape invariants inspired by sway plus our
    /// same-family merge:
    ///
    /// - empty internal children are reaped;
    /// - a single-child internal child is flattened into its only child;
    /// - a plain-split child of the *same family* as a plain-split parent is spliced in, so
    ///   `V[a, V[b,c], d] => V[a,b,c,d]` and `H[H[..]] => H[..]` (a section never directly contains a
    ///   section, a row never directly contains a row).
    ///
    /// Tabbing layouts (Tabbed/Stacked) are never merged — a tab group wrapping a tab group, or a
    /// tab group wrapping a split, is meaningful structure. `self` is **not** collapsed when it ends
    /// up single-child; that is the caller's concern (the section root keeps a lone-leaf wrapper).
    pub fn simplify(&mut self) {
        // Recurse first so children are already canonical.
        if let TileNode::Internal { children, .. } = self {
            for c in children.iter_mut() {
                c.simplify();
            }
        } else {
            return;
        }

        loop {
            let TileNode::Internal { layout, children, data, active_idx, .. } = self else {
                return;
            };
            let self_layout = *layout;
            let mut changed = false;
            let mut i = 0;
            while i < children.len() {
                // Reap an empty internal child.
                if matches!(&children[i], TileNode::Internal { children: c, .. } if c.is_empty()) {
                    children.remove(i);
                    data.remove(i);
                    if *active_idx > i {
                        *active_idx -= 1;
                    }
                    changed = true;
                    continue;
                }

                // Flatten a single-child internal child into its only grandchild (the slot keeps its
                // span data and active flag).
                if matches!(&children[i], TileNode::Internal { children: c, .. } if c.len() == 1) {
                    let TileNode::Internal { children: mut gc, .. } = children.remove(i) else {
                        unreachable!()
                    };
                    children.insert(i, gc.remove(0));
                    changed = true;
                    continue;
                }

                // Merge a same-family plain-split child into this plain split.
                let mergeable = self_layout.is_split()
                    && matches!(&children[i], TileNode::Internal { layout: cl, .. }
                        if cl.is_split() && cl.same_family(self_layout));
                if mergeable {
                    let was_active = *active_idx == i;
                    let TileNode::Internal { children: gc, data: gd, active_idx: ga, .. } =
                        children.remove(i)
                    else {
                        unreachable!()
                    };
                    data.remove(i);
                    let n = gc.len();
                    for (j, (g, gd1)) in gc.into_iter().zip(gd).enumerate() {
                        children.insert(i + j, g);
                        data.insert(i + j, gd1);
                    }
                    if was_active {
                        *active_idx = i + ga;
                    } else if *active_idx > i {
                        *active_idx += n.saturating_sub(1);
                    }
                    changed = true;
                    continue;
                }

                i += 1;
            }

            if !children.is_empty() {
                *active_idx = (*active_idx).min(children.len() - 1);
            }
            if !changed {
                break;
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
            TileNode::Internal { children: _, data, .. } => {
                // The max main span is the largest of the children's cached sizes.
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
            TileNode::Internal { layout, children, data, active_idx, .. }
                if layout.is_split() =>
            {
                let count = children.len();
                if count == 0 {
                    return;
                }

                let is_main = layout.axis() == SplitAxis::Main;

                // Determine which axis we're distributing along.
                // For a Main split: distribute along w (main axis), each child gets full h.
                // For a Cross split: distribute along h (cross axis), each child gets full w.
                //
                // Only the gaps *between* children are taken from `available`: the outer gaps are
                // already accounted for at the section level (the cross `available` is the working
                // area minus its two edge gaps; the main `available` is the section width, which is
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

                assert_eq!(data.len(), count, "data.len ({}) != children.len ({}) in {:?}", data.len(), count, layout);

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
                // the flat-section algorithm in `update_tile_sizes` so nested splits reach the same
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
            TileNode::Internal { children, data, active_idx, tab_header, .. } => {
                let count = children.len();
                if count == 0 {
                    return;
                }

                // A tabbing layout shows one child at a time; all children get the same content
                // span, reduced by the header band reserved at the top.
                let extra = tab_header
                    .as_ref()
                    .map_or(Size::from((0., 0.)), |h| h.extra_size(count, scale));
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
            TileNode::Internal { layout, children, .. } => {
                if *layout == Layout::SplitV {
                    // Vertical split: children stack along cross, so min is the sum.
                    children.iter()
                        .map(|c| c.min_cross_span_subtree(axis))
                        .sum::<f64>()
                        .max(1.)
                } else {
                    // SplitH / Tabbed / Stacked: children share the cross span, so min is the max.
                    // (Tabbing layouts also add a header band, handled where the header is sized.)
                    children.iter()
                        .map(|c| c.min_cross_span_subtree(axis))
                        .max_by(|a, b| a.total_cmp(b))
                        .unwrap_or(1.)
                }
            }
        }
    }
}
