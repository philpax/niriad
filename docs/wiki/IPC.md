You can communicate with the running niri instance over an IPC socket.
Check `niri msg --help` for available commands.

The `--json` flag prints the response in JSON, rather than formatted.
For example, `niri msg --json outputs`.

> [!TIP]
> If you're getting parsing errors from `niri msg` after upgrading niri, make sure that you've restarted niri itself.
> You might be trying to run a newer `niri msg` against an older `niri` compositor.

### Event Stream

<sup>Since: 0.1.9</sup>

While most niri IPC requests return a single response, the event stream request will make niri continuously stream events into the IPC connection until it is closed.
This is useful for implementing various bars and indicators that update as soon as something happens, without continuous polling.

The event stream IPC is designed to give you the complete current state up-front, then follow up with updates to that state.
This way, your state can never "desync" from niri, and you don't need to make any other IPC information requests.

Where reasonable, event stream state updates are atomic, though this is not always the case.
For example, a window may end up with a workspace id for a workspace that had already been removed.
This can happen if the corresponding workspaces-changed event arrives before the corresponding window-changed event.

To get a taste of the events, run `niri msg event-stream`.
Though, this is more of a debug function than anything.
You can get raw events from `niri msg --json event-stream`, or by connecting to the niri socket and requesting an event stream manually.

You can find the full list of events along with documentation [here](https://niri-wm.github.io/niri/niri_ipc/enum.Event.html).

### Programmatic Access

`niri msg --json` is a thin wrapper over writing and reading to a socket.
When implementing more complex scripts and modules, you're encouraged to access the socket directly.

Connect to the UNIX domain socket located at `$NIRI_SOCKET` in the filesystem.
Write your request encoded in JSON on a single line, followed by a newline character, or by flushing and shutting down the write end of the connection.
Read the reply as JSON, also on a single line.

You can use `socat` to test communicating with niri directly:

```sh
$ socat STDIO "$NIRI_SOCKET"
"FocusedWindow"
{"Ok":{"FocusedWindow":{"id":12,"title":"t socat STDIO /run/u ~","app_id":"Alacritty","workspace_id":6,"is_focused":true}}}
```

The reply is an `Ok` or an `Err` wrapping the same JSON object as you get from `niri msg --json`.

For more complex requests, you can use `socat` to find how `niri msg` formats them:

```sh
$ socat STDIO UNIX-LISTEN:temp.sock
# then, in a different terminal:
$ env NIRI_SOCKET=./temp.sock niri msg action focus-workspace 2
# then, look in the socat terminal:
{"Action":{"FocusWorkspace":{"reference":{"Index":2}}}}
```

You can find all available requests and response types in the [niri-ipc sub-crate documentation](https://niri-wm.github.io/niri/niri_ipc/).

### niriad fork additions

This is the **niriad** fork, which adds a recursive window tree on top of niri's scrollable tiling. A few IPC-visible things differ from upstream niri.

**`column` is renamed to `section`.** Everywhere upstream niri says "column", niriad says "section" (the orientation-neutral name for a slot on the scrolling rail). This affects action variants and their `niri msg action` spellings, e.g. `FocusColumnLeft` → `FocusSectionLeft` (`focus-section-left`), `MoveColumnLeft` → `MoveSectionLeft`, `ConsumeWindowIntoColumn` → `ConsumeWindowIntoSection`, `CenterColumn` → `CenterSection`, `SetColumnWidth` → `SetSectionWidth`, and so on. Old `column`-named nodes in the *config* are still accepted via a backwards-compatibility shim, but the IPC `Action` variants use the new names.

**New actions.** The fork wires several sway-style actions through IPC (see the `Action` enum in `niri-ipc`):

- Spatial, screen-direction focus/move that resolve to the rail's main or cross axis depending on monitor orientation: `FocusLeft`/`FocusRight`/`FocusUp`/`FocusDown` and `MoveLeft`/`MoveRight`/`MoveUp`/`MoveDown` (`focus-left`, `move-down`, …).
- Splits: `SplitWindow { direction }` and `ConsumeWindowIntoSplit { direction, id }`, where `direction` is `Main` or `Cross` (`SplitDirection`).
- Container layout, sway's `layout`/`split`: `SetSectionLayout { layout }` where `layout` is `SplitH`/`SplitV`/`Tabbed`/`Stacked` (`SectionLayout`), and `ToggleSplitLayout` to flip the focused container between horizontal and vertical.
- Tabs/stacks: `ToggleSectionTabbedDisplay`, `ToggleTabbed` (works at any tree level, not just the section root), `MoveTab { direction }` with `direction` `Left`/`Right` (`TabDirection`), and `SetSectionDisplay { display }` with `display` `Normal`/`Tabbed` (`SectionDisplay`).
- Swaps: `SwapWindowLeft`/`SwapWindowRight`.

**The tree structure is not exposed over IPC.** This is a known limitation. `niri msg` / the event stream still report the flatter upstream model: `Windows` and `Workspaces` are flat lists, a `Window` only knows its `workspace_id`, and a `Workspace` only knows its `active_window_id` — there are no section/container/split/tab objects to enumerate, and no way to query the tree. The only place the tree surfaces is `WindowLayout::pos_in_scrolling_layout`, an `Option<(usize, Vec<usize>)>` giving a window's `(1-based section index, path of 1-based child indices from the section root to the leaf)`. The new actions above are wired through, but the structure they manipulate is otherwise invisible to IPC.

### Backwards Compatibility

The JSON output *should* remain stable, as in:

- existing fields and enum variants should not be renamed
- non-optional existing fields should not be removed

However, new fields and enum variants will be added, so you should handle unknown fields or variants gracefully where reasonable.

The formatted/human-readable output (i.e. without `--json` flag) is **not** considered stable.
Please prefer the JSON output for scripts, since I reserve the right to make any changes to the human-readable output.

The `niri-ipc` sub-crate (like other niri sub-crates) is *not* API-stable in terms of the Rust semver; rather, it follows the version of niri itself.
In particular, new struct fields and enum variants will be added.
