# Bringing sway's tiling model to niriad

This is the plan (and running log) for reworking niriad's window tree to follow
[sway](https://github.com/swaywm/sway)'s tiling model as faithfully as the niri substrate allows.
It was written after a deep read of the sway source (`../sway`), summarised below.

## What sway actually does (the model we're targeting)

**One node type.** A sway `container` is either a *leaf* (wraps a view/window) or an *internal* node
(has `children` + a `layout`). The workspace is an implicit top-level split: it holds a `tiling` list
plus its own `layout`. There is no separate "tabbed node" type — tabbed/stacked are just layouts.

**Four layouts**, in two families:
- `SplitH` / `SplitV` — children laid side by side / stacked, all visible.
- `Tabbed` — children as tabs; one visible; a single row of side-by-side tab titles. *Horizontal family.*
- `Stacked` — children stacked; one visible; N stacked title rows (one per child). *Vertical family.*

`Tabbed`≈`SplitH` and `Stacked`≈`SplitV` for the purpose of navigation (focus-right cycles tabs,
focus-down cycles stacked entries).

**Splitting** wraps a container in a new split — *unless* it is the lone child of an `H`/`V` parent,
in which case sway just retags the parent's layout (no new node). Tabbed/stacked parents don't get
that shortcut. Changing layout (`layout tabbed`, etc.) is a flag flip on the *parent* container, never
a new node (except the workspace-wrap edge case).

**Simplification** is two mechanisms:
- `flatten`: a single-child internal node is replaced by its child (layout-agnostic). Run on explicit
  unsplit/layout commands.
- `squash`: a redundant *perpendicular* sandwich `H[ V[ H[a b] ] c ] → H[ a b c ]` is collapsed
  (only when the middle node is non-parallel to its parent but parallel to its grandparent). Run after
  moves. Sway does **not** auto-merge same-family nesting (`V[V[…]]`) — but **we will**, per the
  explicit request that `VStack[VStack[x,y,z]] == VStack[x,y,z]`.
- Empty internal nodes are reaped, bubbling up.

**Arrangement / geometry.** `SplitH`/`SplitV` distribute the main axis by per-child fractions and
reserve no titlebar (each leaf draws its own). `Tabbed` reserves exactly one titlebar row; `Stacked`
reserves N rows. Nesting is additive: a tabbed-in-tabbed shows two tab rows; descending each tabbed
level costs one row of height, each stacked level costs N rows, splits cost zero (the leaf pays its
own single titlebar). Tabbed/stacked never narrow width, only consume height from the top.

**Drag-to-move** has three regions on the target under the pointer:
1. *Titlebar* → insert as a tab/stack member at the computed index (wrap the target in `Tabbed` first
   if it isn't already tabbed/stacked).
2. *Layout edge* (a ~30px band, only on the axis perpendicular to each ancestor's layout, the band
   growing as you walk up ancestors) → split perpendicular and insert as a sibling on that side.
3. *View body* (closest edge within 30% of the shorter dimension) → split toward that edge; the inner
   center → **swap** the two windows.

**Directional focus/move** walks the tree: bubble up to the first ancestor whose layout is parallel to
the requested axis and has a sibling that way, then descend (MRU) into that sibling to a leaf. Move
reorders within the parent, or promotes/descends across ancestors, reorienting the workspace at the
top level when the axis is perpendicular. Tabbed/stacked count as H/V for this.

(Full per-subsystem notes with `file:line` references live in the commit messages / project memory.)

## How this maps onto niri

niri's signature is the **horizontal scrollable strip of columns**. We keep that and read it as
sway's implicit top-level container: the strip *is* the workspace's root split, running along the
monitor's **main axis** (horizontal on landscape monitors, vertical on portrait via niri's existing
`AxisMap`). Each element of the strip — what niri calls a *column* — is just a top-level sway-style
container tree, free to be any layout nested arbitrarily. "Columns aren't special": a column is simply
a top-level node in the strip.

Mapping niriad's existing `TileNode` to sway layouts:
- `Split { axis: Main }` = side-by-side along the strip's axis = sway `SplitH`.
- `Split { axis: Cross }` = stacked perpendicular = sway `SplitV`.
- `Tabbed` = sway `Tabbed`. **(new)** `Stacked` to be added.

Because niri maps Main/Cross to screen x/y per monitor orientation, **spatial** directions
(left/right/up/down) resolve through `AxisMap` to Main/Cross — which is exactly how we fix the
"confusing vertical-monitor" keybindings: bind screen directions, not logical column/window ones.

We deliberately keep niriad's `TileNode` enum shape (`Leaf` / `Split{axis}` / `Tabbed`) rather than
collapsing to a single `Internal{layout}` node — the behaviour will be sway-faithful while avoiding a
multi-thousand-line, regression-prone rewrite of the ~7k-line layout core. A future unification is
noted but not required for behavioural parity.

## Staged implementation (each stage builds, tests, and commits independently)

- **S1 — Stacked layout.** Add a `Stacked` display mode alongside `Tabbed` (single child visible, N
  stacked title rows). Arrangement reserves N rows; rendering draws N rows; it's the vertical family.
- **S2 — Canonical tree simplification.** A single `simplify()` pass enforcing: no single-child
  internal nodes (flatten), no same-family nesting (`V[V…]→V…`, `H[H…]→H…`, `Tabbed[Tabbed…]` likewise
  per family), and the perpendicular squash `H[V[H..]]→H[..]`. Run after every structural mutation,
  replacing the current ad-hoc `collapse_redundant_root_wrapper`/`collapse_all_single_child`.
- **S3 — sway-faithful split & layout commands.** `split h/v/tabbed/stacked`, `layout toggle`,
  retag-lone-parent semantics; map them to actions + default keybinds.
- **S4 — sway-faithful drag.** Reimplement `insert_position` with the three regions (titlebar / edge
  band / body closest-edge + center-swap) and matching hints.
- **S5 — spatial focus/move.** Directional focus/move that resolve screen directions through the tree
  (and across the strip / to adjacent outputs), plus reworked default keybindings that are spatial
  rather than logical.

Stages land in order; later stages may be partial if time runs out, but each commit keeps the tree
green (build + tests + clippy + fuzzer).

## Status

- **S0 — done.** `TileNode` unified to `Leaf | Internal{ layout, .. }`; `Layout` enum with axis/family
  helpers. Adversarial-reviewed; fixed a born-tabbed un-tab bug it found.
- **S2 — done.** `simplify()` (flatten + same-family merge), wired into remove/add; `verify_structure`
  enforces no same-family nesting (fuzzer-checked). Reviewed clean.
- **S1 — done.** `Stacked` layout (N title rows) + `set-column-layout`. Reviewed; fixed a root
  leaf-count-vs-child-count geometry desync and a config-reload flag drop it found.
- **S3 — done.** `set-column-layout splith|splitv|tabbed|stacked` and `toggle-split-layout` actions,
  wired across the crates; sway-themed config (`resources/config-niriad.kdl`). Reviewed clean.
- **S5 — done.** Spatial `focus/move-left|right|up|down` resolving screen directions per monitor
  orientation; sway config binds hjkl/arrows to them. Reviewed — 8-way mapping correct both
  orientations.
- **S4 — done.** sway-style drag region map: the tile interior splits toward the closest of its four
  edges (left/right side-by-side, top/bottom stacked, with beside-the-whole-stack escalation), and
  the centre groups the dragged window with the target into a `Tabbed` container; dropping on a
  tabbed section's body adds a tab. New `InsertPosition::InsertTab` + `add_tile_as_tab` (wrap a fresh
  `Tabbed[target, dragged]`, or join an existing tab/stack group). Adversarially reviewed — clean on
  same-family-nesting and active_idx; fixed a tabbed-body target-index bug and a wrap-path leaf-path
  bug it (and self-review) surfaced.
  - *Adaptation:* sway *swaps* the two windows on a centre-drop, but niri detaches the dragged tile
    during a move (it's no longer in the layout), so there's nothing to swap in place — centre→tab
    is the natural insertion-model fit. Literal swap would need keeping the source in place during
    the drag.

### Known limitations / follow-ups
- Spatial focus/move stops at a screen edge rather than crossing to the adjacent output (sway
  crosses). The strip still scrolls; only directional edge-crossing is missing.
- The new `move-*` actions don't special-case the screenshot UI (the older `move-column-*` do), so
  pressing them with the screenshot selector open moves a window instead of nudging the selection.
- ~~A tabbed/stacked root with a nested non-leaf child takes its per-tab title/size from one leaf.~~
  **Fixed:** `Section::tab_children` now computes each tab from its *direct child* — the union
  bounding box of all leaves under it (size) and, for a group child, the sway/i3 tree
  representation (`TileNode::tree_repr`: layout glyph `H`/`V`/`T`/`S` + the children's titles in
  brackets, recursive — e.g. `H[Firefox V[term1 term2]]`) instead of a leaf title. (Identifiers are
  window titles, since that's what the layout layer exposes; sway uses class/app_id.) Also fixed a
  Stacked cross-axis double-subtraction that shrank the active child from the bottom.
- The node enum is unified, but the root tab header still has a dedicated render path (kept to avoid
  changing the common-case visuals); folding it into the nested-header walk is a possible cleanup.

## S6 — sway-literal in-place drag

Make the interactive *tiling* drag match sway: the window stays in the tree during the drag (an
indicator + a translucent following ghost show the drop), so placements are computed by real cursor
hit-testing and a centre-drop performs a true **swap**. This is the default; the current
detach-and-follow stays available via config.

Grounded in the two sources (`../sway/sway/input/seatop_move_tiling.c`, `src/layout/mod.rs`):
- **sway**: tiling and floating drags are *separate* seatops chosen at grab time — a tiling drag is
  always tiling (you can't float by dragging). It's in-place the whole time (the container detaches
  only at release, and not at all for swap), and its target is `node_at_coords` over sway's single
  global tree — so it spans every workspace/output and can swap with any container anywhere.
- **niri**: one unified detach-and-follow move (`Starting` keeps the window in place and rubberbands;
  past `INTERACTIVE_MOVE_START_THRESHOLD` it detaches into `Moving` and follows the cursor). It can
  cross outputs, and can float a tiled window mid-drag — but only via the explicit
  `toggle_window_floating` keybind, **not** a spatial zone. Layout is per-output separate trees.

Design (since the float trigger is an explicit toggle, the in-place phase needs no spatial boundary):
- **In-place covers every tiling target** — same workspace, other workspace, other output — plus
  swap anywhere. "In-place" = keep the source in its tree until release; ghost follows the cursor;
  hit-test whichever tree is under the pointer; apply move/swap at release (cross-tree when needed).
- **The only handoff to detach-and-follow is the explicit float toggle** (niri's extra capability,
  which genuinely pulls the window out of tiling). One-way: once detached, stay detached for the
  drag.
- Config `tiling-drag "sway" | "niri"`, default `sway`. Translucent following ghost for feedback.

Stages (each builds/tests/reviews/commits):
- **S6.1 — done.** Family-aware `toggle-tabbed` (vstack→Stacked, hstack→Tabbed), as a warm-up.
- **S6.2 — done (behaviour; rendering deferred to S6.3).** New `InteractiveMoveState::InPlace`: past
  the threshold a sway-mode tiling drag keeps the source in the tree (no detach); on release a
  same-workspace centre-drop performs a true swap (`swap_tiles` → content-swap, slots fixed,
  occupants exchange — matching sway), a centre-drop on self is a no-op, and everything else
  (non-swap move, or a drop on another workspace/output) detaches and runs the shared apply, landing
  identically to detach mode. The shared hit-test now prefers the *visible* leaf so a centre-drop on
  a nested tabbed container targets the shown tab. Built by a worktree subagent, then reviewed: fixed
  a two-`&mut` raw-pointer UB (→ safe `mem::replace`), the hidden-tab targeting bug, and the
  same-vs-cross-parent swap-size inconsistency. Opt-in via `tiling-drag "in-place"`; default stays
  `detach` until the indicator/ghost land. Fuzzer now randomises `tiling_drag`.
  - *Deferred:* the drop indicator + translucent ghost are NOT rendered during an in-place drag yet
    (the update arm only tracks the pointer), so in-place is behaviour-correct but visually blind —
    hence default `detach`. Minor: a cross-section swap teleports the two principals (no animation);
    `detach_and_apply` drops `workspace_config` (masked by re-config-on-insert).
- **S6.3 — implemented; visual verification pending.** Drop indicator: `update_insert_hint` now
  handles the `InPlace` state (`update_insert_hint_in_place`) — same target resolution as the detach
  path, suppressing the self-hover highlight; the generic paint path is reused unchanged. Ghost:
  `render_in_place_ghost` (called from `render_interactive_move_for_output`, so both `niri.rs`
  push-points are covered) renders the live source tile a second time at the cursor
  (`InPlaceMoveData::ghost_render_location`) composited through a persistent `OffscreenBuffer` with a
  constant `INTERACTIVE_MOVE_GHOST_ALPHA = 0.4` (dimmer than the 0.75 detach drag), the real window
  staying opaque in its slot. A render smoke test covers no-panic; the *pixels* can't be unit-tested,
  so this needs a human visual check (ghost tracks the grab point, alpha/layering, positioning under
  overview zoom, damage trails) before the default flips to `in-place`.
- **S6.5 — done (region map); visual verification pending.** Precise per-window targeting: every
  drop targets the single *visible* window under the cursor — its 4 edges split it (left/right →
  Main, top/bottom → Cross), the centre swaps (in-place) or tabs (detach). The three aggregating
  escalations are gone: `InSplitStack` (whole-stack — variant + apply chain `add_tile_beside_stack`
  fully deleted), the above/below-whole-row `InSection`, and the tabbed-body "add a tab". Tab-add now
  happens *only* over a tabbing container's titlebar **header band** (`Section::header_band_target`,
  reusing `collect_nested_tabbed` + `TabHeader::extra_size`/`content_offset`, deepest container wins)
  — over the content, the per-window map applies, so you can split a tab's window into a stack.
  Built by a worktree agent, reviewed (header band = full-rect-minus-content; interior is clean).
  *Visual check:* the header-band thickness boundary, a literal nested horizontal `Tabbed`, the
  side-split-vs-new-section-gap zone width, and the corner split tiebreak.
- **Indicator colours — done.** The drop hint is tinted by kind: split/move/new-section (blue),
  swap (green), tab-add (purple). Implemented by repurposing the `FocusRing`'s three colour slots
  (active/inactive/urgent → split/swap/tab), selected at render time by a `HintKind` derived from the
  `InsertPosition` — no per-frame reconfig. Configurable via new `swap-color`/`swap-gradient` and
  `tab-color`/`tab-gradient` keys in the `insert-hint` block (distinct defaults, so zero-config), with
  a documented example in `config-niriad.kdl`.
- **S6.4 — cross-output / cross-workspace** in-place (cross-tree move + swap) and the float-toggle
  handoff to the existing `Moving` flow.
