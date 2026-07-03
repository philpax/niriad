### Overview

<sup>Since: 25.02</sup>

You can present the windows in a container as tabs, rather than as visible tiles. Tabs share the same window size, so this is useful to get more space. In this fork tabbing is one of the four container layouts (see [Splits](./Splits.md)), and comes in two family-matched flavors:

- **`tabbed`** (horizontal family) — one child visible, with a single row of side-by-side tab titles.
- **`stacked`** (vertical family) — one child visible, with one full-width title row per child, stacked downward.

![Terminal with a tab indicator on the left.](https://github.com/user-attachments/assets/0e94ac0d-796d-4f85-a264-c105ef41c13f)

Set either layout on the container holding the focused window with `set-section-layout`:

```kdl
binds {
   Mod+W { set-section-layout "tabbed"; }
   Mod+S { set-section-layout "stacked"; }
}
```

If the focused window is directly under the section root, this tabs the whole section; if it is inside a nested container, only that container becomes tabbed.

To toggle the whole focused section between normal and tabbed display, use `toggle-section-tabbed-display`:

```kdl
binds {
   Mod+W { toggle-section-tabbed-display; }
}
```

All other binds remain the same: switch tabs with `focus-window-down`/`focus-window-up` (tabbing containers navigate along the cross axis). You can add or remove windows with `consume-window-into-section` / `expel-window-from-section` — these are **not bound by default**; add binds for them yourself if you want them (see the commented "classic niri" block in the default config). `expel-window-from-section` expels the *focused* window from the section.

Unlike plain splits, tabbed and stacked sections can go full-screen with multiple windows (the section stays intact; only the active tab is shown).

### Tab indicator

Tabbed sections show a tab indicator on the side.
You can click on the indicator to switch tabs.

See the [`tab-indicator` section in the layout section](./Configuration:-Layout.md#tab-indicator) to configure it.

By default, the indicator draws "outside" the section, so it can overlay other windows or go off-screen.
The `place-within-column` flag puts the indicator "inside" the section, adjusting the window size to make space for it.
This is especially useful for thicker tab indicators, or when you have very small gaps.

| Default | `place-within-column` |
| --- | --- |
| ![A screenshot showing 4 windows, with the middle column being focused. The tab indicator overflows onto the left column](https://github.com/user-attachments/assets/c2f51f50-3d87-403a-8beb-cbbe5ec5c880) | ![A screenshot showing 4 windows, with the middle column being focused. The tab indicator is contained within its respective column](https://github.com/user-attachments/assets/f1797cd0-d518-4be6-95b4-3540523c4370) |

### Tab titles and nested groups

A tab's title depends on what the tab holds. A leaf tab shows its window's title. A tab that is itself a container shows a sway/i3-style tree representation: the container's layout glyph followed by its children's compact identifiers (app ids, falling back to titles), space-separated, in brackets — recursively.

The glyphs are `H` (`splith`), `V` (`splitv`), `T` (`tabbed`), and `S` (`stacked`). For example, a tab holding a horizontal split of Firefox next to a vertical stack of two terminals reads:

```
H[firefox V[kitty kitty]]
```

### Generalized tabbing

Tabbing works at any level of the tree, not just the section root. `toggle-tabbed` toggles the container holding the focused window between a plain split and its family's tabbing layout:

```kdl
binds {
    Mod+Shift+W { toggle-tabbed; }
}
```

The toggle is family-aware, matching sway's two title styles: a vertical (cross-axis) split tabs into `stacked` (one full-width title row per child, stacked downward), while a horizontal (main-axis) split tabs into `tabbed` (a single row of side-by-side titles). Un-tabbing remembers and returns to the previous split layout.

Nested tabbing layouts are preserved rather than merged away — a tab group inside a tab group, or a tab group wrapping a split, is meaningful structure (see the normalization notes in [Splits](./Splits.md)).

### Tab reordering

Tabs can be reordered within their container using `move-tab-left` and `move-tab-right`:

```kdl
binds {
    Mod+Shift+BracketLeft  { move-tab-left; }
    Mod+Shift+BracketRight { move-tab-right; }
}
```

This moves the focused tab left or right among its container's children (each of which may itself be a nested split), and follows the moved tab.
