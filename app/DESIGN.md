# Corvus Sweep design notes

Corvus Sweep follows the auggiesoft UI guide (an in-house design guide, read at `6020667`,
2026-11-06; not public). This page records what `ADOPTING.md` asks for: the look, how the colour roles are
filled, the project voice, and the exceptions to MUST rules with their reasons. Where this page and
the guide disagree, this page wins for Corvus Sweep, and the disagreement is listed below.

## Look

**Instrument** on every surface (`looks/instrument.md`): flat neutral chrome, black plot wells,
colour only on traces, the selection and alarms. Values are the guide's Instrument tokens,
unchanged, as Rust constants in `src/theme.rs`. There's no Plain look (see Exceptions).

## Colour roles

| Role | Value | Used for | Paired with |
|---|---|---|---|
| accent `#5aa9f0` | lit chips (selected rate, step, port…), the shift-drag selection | the lit fill with dark ink; the selection's outline |
| caution `#f0a830` | steps lost in the last 10 s, lock check off, the armed "Relearn" confirm, the frame-load bar over 80 % | an outlined triangle and a sentence; hatching on the confirm |
| alarm `#ff4d4d` | the RFOne isn't open, the radio refused a setting | a filled triangle, a sentence and a fix |
| ok `#3ccf6e` | the status tile's "All good" dot only | the words "All good" |
| live `#ff5cc8` | not used: Corvus Sweep never transmits or records | |
| info `#9fd3ff` | paused, simulator, VCO-switch cost, nothing learned yet | an outlined circle and a sentence |
| touch `#f2f2f2` | hover and drag rings, the plot cursor, the sweep head | |
| house `#c9a84c` | the AUGGIESOFT wordmark | |

- **Identity:** live trace `--trace` cyan `#5cc8e8`, peak hold sand `#d8c9a3`, average violet
  `#b08cff`. Each has a swatch next to its name in Display. The peak-hold trace used to be amber,
  which is caution's colour.
- **Colour maps:** Inferno (the look's map, default), Viridis, Grey. The old Turbo (rainbow) and
  Hydra maps are gone; saved settings naming them load as Inferno. Floor and ceiling are in
  Levels, with Auto levels.
- **Neutral overlays:** the sweep beam, step cells, VCO switch marks and peak leaders are ink at
  low opacity, not hues.

## Type

The house type, compiled in (`assets/fonts/`, see its README): Barlow Medium for UI text, Barlow
Semi Condensed for uppercase labels, Auggie Mono for every value, frequency and tick, and a
`tnum`-frozen Barlow SemiBold for the HUD numerals. Units are drawn in ink-2, one step smaller than
their value. Negative numbers use U+2212.

**Project voice:** none.

## The frame

- **Top bar:** wordmark, product name, then Run/Pause, Auto levels, Export CSV, Snapshot, Settings
  and `?` on the right.
- **Status bar** under it: the traffic-light tile ("All good", "1 problem"…) and conditions as
  sentences with their fixing actions. Conditions stay while they're true. Toasts only acknowledge
  your own actions (2.6 s), on the right of the status bar, never over the plot.
- **HUD:** eight KPI tiles in display wells. Each has a label, a value with a quieter unit, and a
  context line; the sparkline says its window ("last 60 s").
- **Sidebar:** Range, Sweep engine, Receiver, Display, then Levels (closed). Rarely used controls
  sit behind one labelled disclosure, and explanations are hover-then-pin hints on dotted-underlined
  labels. It fits 1920×1080 without scrolling.
- **Footer:** where the data comes from, frame rate against its cap, milliseconds per frame, and a
  load bar.

## Feel

- **Sliders never jump** (`theme::slider`). A click does nothing. Dragging moves relative to where
  the value was, after a 4 px dead zone, and Shift gives ¼ speed. The wheel steps it, and
  double-click types a value. Double-clicking the label resets it, and right-click offers reset,
  back and type. After a change, the old value stays as a dashed ghost with a ↶ button back to it.
- **Profiles:** four scan profiles head the Sweep engine card. The details they set (rate, order,
  dwell, lock reads, settle) sit behind its disclosure, and the chip that matches the current
  settings is lit (none when they match no profile). Applying one puts "↶ <what you had>" on
  the card's header line, which keeps the sidebar within 1080p.
- **Range back:** every range change (preset, key, selection, typing) leaves "↶ previous range"
  in the Range card, also on Backspace. Pressing it again returns.
- **Typed frequencies** apply on Enter, Tab or clicking away, and Esc cancels. The sweep waits
  until both fields are done.
- **Two-step confirm:** "Relearn the passband" arms a hatched caution button ("Click again: forget
  the learned shape") that disarms after 4 s.
- **Motion:** only data, the control under the pointer, and the sweep head move. The old pulsing
  status dot and trace glow are gone. The sweep head and beam show only when a full pass takes
  about a second or more (switching at 1.2 s, back below 0.8 s). A normal full-range pass is
  ~205 ms, and a head that lands somewhere new every frame reads as strobing. Faster sweeps get a
  steady strip instead: a cell stays lit while its step keeps arriving and fades out if it stops,
  so a dead region shows as a dark gap.

## Overlays (rules/overlays.md)

- Peak labels sit in a rail above the plot, with leaders down to their peaks (O1). The strongest
  peaks win the space, and the rest go into a "+N" pick list that can centre the view on any of
  them (O5). The levels are Full (12), Focus (4) and Minimal (none) (O4).
- The hover readout is a fixed chip in the plot's top-left corner (O7). Its cursor runs through the
  waterfall too (L7).
- VCO divider switch points are dashed lines in the plot. Their names (`VCO ÷8`) are on the strip
  under the axis, not over the data.
- Waterfall: relative time ticks in the gutter, the clock time at the top, and a hairline plus a
  `retune` tick for every gain, port or flatten change (O13).
- **Mode changes don't clear the waterfall.** A new range, profile, step, rate, order, dwell, lock
  reads, settle or bin width draws a hard break: a gap across the history with a chip saying what
  changed ("400–1000 MHz", "Quality", "23.4 kHz bins"). Labels of breaks close together sit side by
  side. Older rows are re-mapped onto the new frequency axis from each row's own data, so they sit
  at their true frequencies (coarser if they were measured wider) and are blank where they
  measured nothing.
- Graticule and trace brightness are separate settings (D9).

## Exceptions

| Rule | What Corvus Sweep does | Why |
|---|---|---|
| R1 (MUST): ship a redesign behind a setting | Switched outright, with no setting for the old look | The switch happened before the first public release, so no user had the old look. |
| I4 (MUST): hit areas ≥ 44 px | Controls are 28 px tall (24 px in Compact), with egui's `interact_radius` adding 8–10 px of reach on every side, so the effective target is ≥ 44 px | A mouse-driven desktop instrument. 44 px visuals would push the sidebar into scrolling (L1). |
| L1 (MUST): no vertical scroll | Fits at 1920×1080 with every disclosure closed. Opening disclosures, or a smaller window, scrolls the sidebar (never the plot). | The scroll is a fallback for small windows. Nothing that needs watching is in the sidebar. |
| L3 / gold thread: page bar with a gold underline | No page bar, so the gold appears only in the wordmark | Corvus Sweep has one page. |
| P5 (SHOULD): offer Plain | Instrument only | The app runs on one desktop GPU; nothing needs a lighter fallback. |
| L6 (SHOULD): overview strip with a viewport box | The plot shows the swept range, and "Sweep visible area" and Backspace move between ranges | Not built yet. |
| I6 (SHOULD): command palette | The shortcut sheet (`?`) lists every command with its key | Not built yet. |
| D4 meter ballistics, S3, S6, S8 | Not applicable | Corvus Sweep has no meters, mutes or stage mode. |
