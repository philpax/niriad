# Future Work

This document lists features explicitly deferred from the recursive tile tree implementation.

## Tab Header Bar (i3/sway-style)

A horizontal tab header bar with text labels (like i3/sway's `tabbed` layout) is not yet implemented. The existing niri-style colored gradient bar indicator remains the only tab display style. Implementing the `TabBar` would involve:

- Creating `src/layout/tab_bar.rs` with pangocairo text rendering
- Adding `TabHeader` enum (Indicator/Bar) to generalize tab display
- Adding `TabBarConfig` to `niri-config/src/appearance.rs`
- Caching title textures per-tab, invalidated on title/scale change

## Tab Scroll

When a tabbed container has too many tabs to fit in the header bar width, the bar should become scrollable. Not yet implemented since the header bar itself is not yet implemented.

## Nested Tabbed Containers at Arbitrary Depth

Currently, `ToggleTabbed` toggles the column root between split and tabbed. Fully nested tabbed containers (a Tabbed node inside a Split inside another Split) require recursive layout, rendering, and hit-testing that is not yet complete.

## Tab Close Buttons

Optional tab close buttons (config-driven) are not implemented.

## Tab Drag-and-Drop Reordering via Interactive Move

Tab reordering is currently keybind-only (`move-tab`). Drag-and-drop reordering via interactive move is not implemented.

## Configurable Tab Width Policies

Only the `equal` width policy (all tabs same width) is supported. `title-based` (width proportional to title length) and `fixed` (fixed width with scroll) are not implemented.

## Nested Scroll Viewports for Interior Splits

Interior splits are shrink-to-fit (all children visible). Nested scroll viewports are not supported.

## IPC Extensions for Full Tree Introspection

The IPC `WindowLayout` reports tree paths but does not expose the full tree structure (split axes, tabbed state). A full tree introspection API would make IPC consumers simpler.

## Per-Workspace Tab Style Overrides

Tab style configuration is global. Per-workspace overrides are not supported.

## Tab Indicator Style for Nested Tabbed Containers

The niri-style indicator (`TabStyle::Indicator`) only fully supports root-level tabbed containers. For nested tabbed containers, the `Bar` style is recommended.
