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

Within a family the plain split (`splith`/`splitv`) shows every child, while the tabbing layout (`tabbed`/`stacked`) shows one child at a time. For navigation a `tabbed` container behaves like `splith` and a `stacked` one like `splitv`. Tabbing layouts are covered in more detail on the [Tabs](./Tabs.md) page.

## Setting a container's layout

`set-section-layout` sets the layout of the container holding the focused window. If the focused window sits directly under the section root, this sets the whole section's layout; if it is inside a nested container, only that container changes.

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

## Split-then-open

The `split-window` action sets a pending split direction on the focused section. The next window opened in that section is placed side-by-side with the currently-focused window in a split, rather than appended to the section.

```kdl
// Default keybinding (commented out in the default config)
binds {
    Mod+Shift+S { split-window; }
}
```

After pressing the bind, open a new window (e.g. a terminal). It will appear side-by-side with the focused window, each taking half the width.

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

Within a split, `set-section-width` (or interactive resize via the mouse) adjusts the focused child's proportion within its container, rather than the section width.

## Swapping

`swap-window-left` and `swap-window-right` swap tiles within a split. When at the edge of a container, swapping falls through to the parent.

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
