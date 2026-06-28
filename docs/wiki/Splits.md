# Splits

Niri supports horizontal splits within columns, allowing you to place windows side-by-side within a single column. This is useful when you want small windows next to each other without expanding rightward into separate columns.

## Split-then-open

The `split-window` action sets a pending split direction on the focused column. The next window opened in that column will be placed side-by-side with the currently-focused window in a split, rather than stacked vertically.

```kdl
// Default keybinding
binds {
    Mod+Shift+S { split-window; }
}
```

After pressing `Mod+Shift+S`, open a new window (e.g. a terminal). It will appear side-by-side with the focused window, each taking half the column width.

## Consume into Split

The `consume-window-into-split` action takes the focused window from an adjacent column and places it side-by-side with the focused window in the current column.

```kdl
// Default keybinding
binds {
    Mod+Shift+Comma { consume-window-into-split; }
}
```

## Focus Navigation

Within a main-axis split, `focus-left` and `focus-right` navigate between split children. When at the edge of a split, navigation falls through to column-level movement (moving to the adjacent column).

## Resizing

Within a main-axis split, `set-column-width` (or interactive resize via mouse) adjusts the focused child's width proportion within the split, rather than the column width.

## Swapping

`swap-window-left` and `swap-window-right` swap tiles within a main-axis split. When at the edge of a split, swapping falls through to column-level swap.

## Split Direction

Both `split-window` and `consume-window-into-split` accept an optional direction parameter:

- `main` — Split along the main axis (horizontal in normal monitors, vertical on vertical monitors). This is the default.
- `cross` — Split along the cross axis (vertical in normal monitors). This is the existing column stacking behavior.

```kdl
// Split along the cross axis
binds {
    Mod+Shift+S { split-window "cross"; }
}
```

## Empty Split Collapse

When a window is removed from a split and only one window remains, the split collapses back to a single window. This matches i3/sway behavior where empty containers automatically disappear.
