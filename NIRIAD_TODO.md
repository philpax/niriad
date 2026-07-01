# niriad TODO

Known next steps for the fork — polish, refactors, and coverage gaps to pick up as time allows.

## In-place tiling drag

- The cross-output ghost re-renders the source tile's texture at the destination output's scale, so it resamples when the two outputs differ in scale. Logical size and position stay correct; only sharpness suffers.
- Give the in-place ghost and the per-window drop region map a visual once-over: header-band thickness, a literal nested horizontal `Tabbed`, split-vs-new-section zone widths, the corner tiebreak.

## Layout core

- The root tab header still has its own render path, kept to avoid disturbing the common-case visuals. Folding it into the nested-header walk is a possible cleanup with no behaviour change.
