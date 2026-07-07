# Splits

In this fork, a **section** (upstream niri calls it a "column") is no longer a fixed vertical stack of windows. Each section is a recursive *tree*: its leaves are windows, and its internal nodes are containers, each carrying one of four layouts. This lets you nest splits and tab groups arbitrarily, i3/sway-style, while keeping niri's scrollable strip of sections along the main axis.

## The four layouts

Every internal node has one of four layouts. They fall into two *families* by axis:

| Layout | Family | Shows | Sway name |
| --- | --- | --- | --- |
| `splith` | horizontal (main axis) | children side-by-side, all visible | `splith` |
| `splitv` | vertical (cross axis) | children stacked, all visible | `splitv` |
| `tabbed` | horizontal (main axis) | one child; a single row of side-by-side tab titles | `tabbed` |
| `stacked` | vertical (cross axis) | one child; one title row per child, stacked | `stacking` |

Within a family the plain split (`splith`/`splitv`) shows every child, while the tabbing layout (`tabbed`/`stacked`) shows one child at a time. The family decides how a tabbing container is sized and which plain split it un-tabs to — it does *not* decide how you focus through it. Focus navigation through *any* tabbing container (both `tabbed` and `stacked`) is cross-axis: `focus-up`/`focus-down` cycle its tabs, matching niri's original tabbed-section navigation. `focus-left`/`focus-right` do not cycle tabs. Tabbing layouts are covered in more detail on the [Tabs](./Tabs.md) page.

## Setting a container's layout

`set-section-layout` sets the layout of the container holding the focused window. If the focused window sits directly under the section root, this sets the whole section's layout; if it is inside a nested container, only that container changes.

On a **lone window** there is no container to re-lay, so `set-section-layout "splith"` / `"splitv"` instead arm a pending split in that direction — exactly like `split-window` (see [Split-then-open](#split-then-open) below), so the *next* window opens beside this one. This mirrors sway, where `splith`/`splitv` on a single window decide which way the next window opens. `set-section-layout "tabbed"` / `"stacked"` still have no effect on a lone window.

```kdl
// Default keybindings
binds {
    Mod+B { set-section-layout "splith"; }
    Mod+V { set-section-layout "splitv"; }
    Mod+S { set-section-layout "stacked"; }
    Mod+W { set-section-layout "tabbed"; }
}
```

`toggle-split-layout` flips the focused container between horizontal and vertical split (`splith` ⇄ `splitv`). A tabbing container converts to the plain split of its family.

```kdl
// Default keybinding
binds {
    Mod+E { toggle-split-layout; }
}
```

## New windows and split-then-open

By default a new window opens as its own **new section** in the strip (the niri model) — it does *not* join the focused window. To open the next window *into* the focused container instead, arm a **pending split** first, with either `split-window` or (on a lone window) `set-section-layout "splith"`/`"splitv"`.

The `split-window` action sets a pending split direction on the focused section. The next window opened in that section is placed side-by-side with the currently-focused window in a split, rather than appended to the section.

```kdl
// Default keybinding (commented out in the default config)
binds {
    Mod+Shift+S { split-window; }
}
```

After pressing the bind, open a new window (e.g. a terminal). It will appear side-by-side with the focused window, each taking half the width.

The pending split follows a few rules so it behaves predictably:

- **Toggle:** re-arming the *same* direction cancels it (a sway-like toggle); arming a *different* direction switches to it.
- **Cleared on focus change:** if you move focus to another section, workspace, or output before opening a window, the pending split is discarded (it won't ambush a window you open much later somewhere else).
- **Aimed at the next *new* window:** pulling an existing window in with `consume-window-into-section` does *not* consume the pending split — the consumed window is appended, and the split still fires for the next new window you open.

There is currently **no on-screen indicator** for an armed pending split (it is compositor-internal state and not yet exposed over IPC); this is a known limitation.

## Consume into split

The `consume-window-into-split` action takes the focused window from an adjacent section and places it side-by-side with the focused window in the current section.

```kdl
// Default keybinding (commented out in the default config)
binds {
    Mod+Shift+Comma { consume-window-into-split; }
}
```

## Split direction

Both `split-window` and `consume-window-into-split` accept an optional direction:

- `main` — Split along the main axis (horizontal on normal monitors, vertical on vertical monitors). This is the default and creates a `splith`-style side-by-side pair.
- `cross` — Split along the cross axis (vertical on normal monitors). This stacks the windows, matching niri's original column behavior.

```kdl
// Split along the cross axis
binds {
    Mod+Shift+S { split-window "cross"; }
}
```

## Focus navigation

Within a split, `focus-left`/`focus-right` (and `focus-up`/`focus-down` for cross-axis splits) navigate between the children. When at the edge of a container, navigation falls through to the parent, and ultimately to section-level movement across the scrollable strip.

## Resizing

Tile-level resize routes to the **nearest ancestor split of the matching axis**, so it resizes the slot the focused window actually occupies:

- `set-window-width` (Mod+Minus/Equal by default) walks up to the nearest `splith` ancestor and resizes the focused window's slot within it. With no `splith` ancestor the window already spans the section's main extent, so it falls back to resizing the whole section width.
- `set-window-height` walks up to the nearest cross-axis (`splitv`/tabbing) ancestor and resizes the focused window's slot there.

`switch-preset-section-width` (the preset width cycle) uses the *same* nearest-`splith`-ancestor routing: when the focused window is in a nested `splith`, the preset is applied to its slot (as a proportion of the section); otherwise it resizes the whole section, as before. `set-section-width` still targets the whole section directly.

Interactive resize via the mouse adjusts the focused child's slot within its container.

## Swapping

The shipped default binds `Mod+Shift+H/J/K/L` (and `Mod+Shift+Arrow`) run `move-left`/`move-right`/`move-up`/`move-down`. Despite the `move-` name, these **swap** the focused window's subtree with its adjacent sibling at the nearest axis ancestor — they never reparent the window into a different container. When there is no sibling in that direction within the current section, the movement falls through to moving the whole section along the strip. `swap-window-left`/`swap-window-right` are the explicit swap actions along the main axis. To actually restructure the tree (reparent a window into another container), use the [interactive in-place drag](#interactive-drag-in-place) or `expel-window-from-section` / `consume-window-into-split`.

## Interactive drag (in-place)

Dragging a tiled window is "in-place" by default: the source stays in the tree for the whole drag, and where you drop it decides what happens.

- **Centre** of a window — swaps the dragged window with that tile (sway's centre-drop).
- **Left/right edge** — places the dragged window beside that window as a main-axis (`splith`) split.
- **Top/bottom edge** — stacks the dragged window above/below that window as a cross-axis (`splitv`) split.
- **Tab header band** of a tabbing container — adds the dragged window as a new tab of that container.

## Normalization (simplify)

After every structural change the section tree is canonicalized bottom-up (`simplify()`), so it never accumulates redundant nesting. The invariants, inspired by sway:

- Empty internal containers are removed.
- A single-child container is flattened into its only child (no single-child wrappers).
- A plain-split child of the *same family* as a plain-split parent is spliced in, so `V[a V[b c] d]` becomes `V[a b c d]` and `H[H[…]]` becomes `H[…]`. A section never directly contains a section, and a row never directly contains a row.

Tabbing layouts (`tabbed`/`stacked`) are never merged this way — a tab group wrapping a tab group, or a tab group wrapping a split, is meaningful structure and is preserved. The section root also keeps a lone-leaf wrapper rather than collapsing to nothing.

This is why removing a window from a split that leaves a single window behind collapses the split back to that lone window, matching i3/sway behavior where empty and single-child containers disappear.

## Fullscreen and maximize

Fullscreening or maximizing a window that shares a section with others (a multi-window split, or a tabbed section whose active tab is itself a nested split) expels it into its own section for the duration, so it doesn't overlap its neighbors. **Unfullscreening/unmaximizing restores it in place** (sway behavior): the window returns beside the neighbor it left, in the same container, along the original axis. If that neighbor has since closed, or moved to another workspace/output, the window stays where it is as a stray section rather than restoring — restore never crosses workspaces. A plain all-leaf tabbed section is *not* expelled (only its active tab is visible anyway), so it simply fullscreens and un-fullscreens in place.

## Known divergences from sway

A few sway behaviors are intentionally not (yet) implemented here, to avoid conflicting with niri's scrollable-strip model:

- **No focus-parent / focus-child** container selection — you focus windows, not arbitrary ancestor containers.
- **`move-*` swaps rather than reparents** — as described under [Swapping](#swapping), the directional move binds swap with a sibling instead of moving the window into/out of neighboring containers. Reparenting is done via the in-place drag or expel/consume.
- **No "open in focused container" mode** — new windows open as a new section by default; opening into the focused container is opt-in per-window via the pending split (`split-window` / `set-section-layout` on a lone window), not a global default.
