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
  wired across the crates; sway-themed config (`resources/sway-config.kdl`). Reviewed clean.
- **S5 — done.** Spatial `focus/move-left|right|up|down` resolving screen directions per monitor
  orientation; sway config binds hjkl/arrows to them. Reviewed — 8-way mapping correct both
  orientations.
- **S4 — pending.** sway three-region drag (titlebar→tab / edge-band→split / body→split-or-swap).
  niriad already has a working thirds + beside-stack drag; making it fully sway-faithful is the
  remaining refinement.

### Known limitations / follow-ups
- Spatial focus/move stops at a screen edge rather than crossing to the adjacent output (sway
  crosses). The strip still scrolls; only directional edge-crossing is missing.
- The new `move-*` actions don't special-case the screenshot UI (the older `move-column-*` do), so
  pressing them with the screenshot selector open moves a window instead of nudging the selection.
- A tabbed/stacked **root** with a nested non-leaf child renders one tab/row per direct child
  (geometry correct) but the per-tab titles are taken per-leaf; a tab whose content is a group shows
  a leaf title. Nested (non-root) headers already do this correctly. Fully unifying the root header
  through the nested-header path would resolve it.
- The node enum is unified, but the root tab header still has a dedicated render path (kept to avoid
  changing the common-case visuals); folding it into the nested-header walk is a possible cleanup.
