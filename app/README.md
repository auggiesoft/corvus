# Corvus Sweep

A live wideband spectrum analyser for an RFOne running the Corvus sweep firmware
(`../firmware/`). The firmware retunes itself and tags each block with its step. Corvus Sweep
writes each step into a stitched spectrum as soon as it arrives. A full 24–1800 MHz pass takes
about 205 ms (8.6 GHz/s).

Rust + egui/eframe (wgpu). It loads libhydrasdr with the Corvus sweep API at run time (built by
`../host/build/build.sh linux`), so a system-wide libhydrasdr stays untouched.

![Corvus Sweep on the full 24–1800 MHz range, running on its simulator](../docs/corvus-sweep.png)

## Run

```
cargo run --release -- --sim            # built-in simulator, no radio needed
cargo run --release                     # the first supported board plugged in
cargo run --release -- --serial 0123456789ABCDEF   # a particular board
cargo run --release -- --range 400:470 --order asc
cargo run --release -- --msps 10                 # the older 10 MSPS setting (8 MHz steps)
cargo run --release -- --headless 10 --csv /tmp/spectrum.csv   # no window: rates, seam ripple, CSV
cargo run --release -- --board airspy   # an Airspy R2 (--board mini, --board hackrf)
cargo run --release -- --pluto 192.168.1.50         # a Pluto+ or PlutoSDR running corvus-pluto
cargo run --release -- --usb lib        # through the board's C library instead (see below)
build/build.sh windows                  # corvus-sweep.exe for Windows (MinGW, in Docker)
```

**No library needed.** Corvus Sweep talks to every board through its **built-in USB driver**
(`src/usbdev.rs`, pure Rust via nusb): the requests the app uses from libhydrasdr, libairspy and
libhackrf, ported one for one. One binary, nothing to install, the same on Linux and Windows. Checked
side by side against each library on an RFOne, an R2, a Mini and a HackRF: the same noise floor
(within 0.1 dB), the same signal levels within run-to-run drift, the same steps/s, no integrity issues,
in firmware and PC-driven sweeps; `cargo test --release -- --ignored real_boards` (with
`CORVUS_TEST_SERIALS`; a Pluto as `pluto:<host>`) drives each board through gain changes, pause/resume, replans and reopening.
`--usb lib` (or `CORVUS_USB=lib`) uses the board's C library instead: libhydrasdr or libairspy with the
Corvus sweep API (`../host/build/build.sh linux` / `airspy-linux`), or the system libhackrf.

Without `--board`, the app picks the first supported board plugged in (the one with `--serial`, if
given). `--board airspy` (or `CORVUS_BOARD=airspy`) drives an Airspy R2. The R2 runs 10 MSPS only and has one antenna input, so the rate and port controls go away
and profiles with 10 MHz steps use 8 MHz. `--board mini` drives an Airspy Mini: 6 MSPS (12 MHz ADC,
3 MHz IF), steps of 2–4 MHz, profiles at 4 MHz. Each board's learned passband is cached separately.

`--board hackrf` drives a HackRF One on its **stock** firmware, with the same firmware sweep
`hackrf_sweep` uses: 20 MSPS complex, tunes 5 and 15 MHz past each 20 MHz base,
each tune giving the 5 MHz slice below its LO and the one above. The app treats the slices as steps of
a 5 MHz grid (visited 0, 2, 1, 3, 4, ...), so stitching, peaks, the waterfall and integrity work as for
the other boards, from 1 MHz to 6 GHz. While it's open, the PortaPack's screen (if one is fitted) is
turned off, because it redraws the frequency on every retune; the RF amp and the antenna port power
stay off. Gain presets and the manual sliders map onto the HackRF's LNA (0–40 dB) and VGA (0–62 dB).

`--pluto HOST[:PORT]` (or `CORVUS_PLUTO`, or `--board pluto` for 192.168.2.1, a Pluto's own USB network
link) drives a **Pluto+ or PlutoSDR** through `corvus-pluto`, a small program on the board
(`../firmware/pluto/`, installed with its `install.sh`). The board retunes, captures and tags the blocks
itself, two words per 12-bit IQ sample, and serves them over TCP; Corvus Sweep only sends the plan and
the gain (`src/pluto.rs`). 61.44 MSPS (steps up to 50 MHz, 40 by default) or 30.72 MSPS (up to 25 MHz),
plus 15.36, 7.68 and 3.84 MSPS for finer bins (3.75, 1.9, 0.94 kHz native); passes back and forth (no wrap).
**Range:** about 18 MHz to 6.18 GHz on a Pluto+. corvus-pluto pushes the synthesizer past the driver's
46.88 MHz–6 GHz where the chip locks (46–6150 MHz here; it reports its range, `lo=` in its greeting), and
steps beyond that are taken at the nearest end and shifted digitally, so the cover reaches half the RF
bandwidth further. Less accurate towards the ends (the analog filter's edge, a residual of the LO leakage).
**Live view** (Pluto, more boards to follow): the Range card's Live switch, or **L** over the spectrum
(centred where the cursor is). One step, Centre and Span (50, 25, 12, 6, 3, 1 MHz); the span picks the
lowest rate that covers it, so the bins are the finest the span allows. The board stays on the frequency and
streams: ~3,800 updates/s at 61.44 and 30.72 MSPS (network-bound), and at 15.36 MSPS and below about 94 % of
the signal captured, nearly gapless. Shift+arrows move it a span; L or Sweep goes back to the sweep it came
from. Every gain mode sets the AD936x's one manual gain
(0–71 dB; Gain 10 is 34 dB, shown under the slider), and it changes mid-sweep without stopping. **Input**
(Receiver card, or `--port RX2`) picks RX1 or RX2 (corvus-pluto 0.2.3 on a board in 1R1T); switching restarts
the sweep. The bins
around DC are bridged from their neighbours. corvus-pluto 0.2 sweeps in **fast mode** where the board
allows it (fastlock profiles over direct SPI; the device chip says "fast sweep", or "driver sweep" and why);
`CORVUS_PLUTO_MODE=driver` asks for the driver path. Blocks arrive 12-bit packed and are unpacked into the
usual layout. Over 70–6000 MHz on a Pluto+ over Ethernet: about 3,300 steps a second, 130 GHz/s, 22 passes a
second (18–6178 MHz: ~3,100 steps/s, 20 passes a second) (USB network link: ~1,900 steps/s; driver path: ~650). A new plan takes ~0.7 s while the board
calibrates its profiles (cached for 10 minutes). A Pluto seen on USB shows in the device picker; one given by
address shows there too.

**Stock RFOne and Airspy firmware** (no Corvus sweep mode) gets a PC-driven sweep automatically: the
app retunes with `set_freq`, skips two USB transfers for settling, and tags the next blocks itself, in
the same order and format as the firmware. It runs at 30–60 steps a second, which the status bar
explains. `--host-sweep` forces it on Corvus firmware too, for comparison.

Other options: `--board rfone|airspy|mini|hackrf|pluto` (or `CORVUS_BOARD`), `--pluto HOST` (or `CORVUS_PLUTO`), `--host-sweep`, `--lib PATH` (or `CORVUS_LIB`), `--serial HEX` (or `CORVUS_SERIAL`),
`--msps 10|12|61|31`, `--step MHZ`, `--bin-khz K`, `--profile NAME`, `--fresh` (ignore saved settings);
`--help` lists them all. Settings persist between runs via eframe storage
(`~/.local/share/corvus-sweep/`). The learned passband correction is stored in
`~/.cache/corvus-sweep/`.

Building needs a Rust toolchain (edition 2024) and, on Linux, the usual Wayland/X11 and Vulkan or
GL libraries that wgpu uses.

## What's on screen

The UI follows the auggiesoft UI guide's Instrument look (`DESIGN.md` has the details and exceptions).

- **Top bar:** a device chip, Run/Pause, Auto levels, Export CSV, Snapshot, Settings (density, text
  size, frame rate cap, grid and trace brightness) and `?` for the shortcut sheet.
  - The **device chip** (with a ▾) names the radio and how it sweeps: green for Corvus firmware or the
    HackRF's own sweep, amber for a PC-driven sweep on stock firmware, blue for the simulator, red
    when nothing is open. Hover for the firmware, serial and library. Click it to switch radios without restarting: it lists every RFOne, Airspy R2/Mini
    and HackRF on USB, plus the simulator. The sweep is re-planned for the new board (rates, steps
    and ports differ), and each board keeps its own learned passband.
- **Status bar:** one tile ("All good", or how many problems) and the current conditions as
  sentences with a button that fixes each one: RFOne not open, a refused setting, steps lost in the
  last 10 s, lock check off, paused, simulator, a range that can't avoid a VCO switch.
- **HUD:** steps/s (device clock, with a 60 s sparkline), GHz/s, full passes/s, span, visiting
  order, PLL lock polls, integrity (torn / gaps / timeouts / host drops) and USB MB/s. After any
  restart (plan, gain or port change, resume), blocks are dropped quietly until the new run's first
  block arrives, so the old plan's blocks still in flight don't count as misfiled or as gaps.
- **Overview strip:** the whole swept range as a thin trace, with a box for the part the plot
  shows. Drag the box to pan (zoomed all the way out: to move the swept range), drag its edges to
  zoom, click to centre there, double-click to show the full sweep. It can be turned off under Display.
- **Spectrum:** live trace (cyan), peak (sand) and average (violet) on a black well.
  - Peak labels sit in a rail above the plot with leaders to their peaks. The ones without room go
    into a "+N" list that can centre the view on them.
  - The hover readout follows the cursor: frequency and level next to the pointer, with a level
    line across the plot. Over the waterfall it gives the row's age and level. Display has a
    setting to pin it in the top-left corner instead.
  - The **sweep beam** faintly lights each step as the firmware visits it, but only when a full pass
    takes about a second or more (fine steps, slow settings). Faster than that it can only flash.
  - Dashed lines mark the tuner's **VCO divider switch points** (50.3, 105.6, 216.3, 437.5, 880 MHz).
- **Sweep strip:** the divider bands, named. When a pass is slow enough to follow, a head and
  flashing cells show the step being measured. In VCO-continuous order the head runs through one
  divider band at a time, every other band backwards, and each pass reverses. Faster sweeps show
  steady cells instead: lit while the step keeps arriving, fading out if it stops.
- **Waterfall:** one row per pass (narrow plans merge passes up to a max row rate), drawn from a GPU
  texture ring. The gutter has relative time, the clock time at the top, and a `retune` line
  wherever gain, port or flattening changed.
  - Drag the grip between the spectrum and the waterfall to share the height between them.
  - **3D view** (the button at the top right of the waterfall, **W**, or `--3d`): the history as a
    lit landscape on the GPU, newest row at the front, frequency left to right, height and colour
    from level. Drag to turn it, scroll to zoom, double-click to reset. It frames itself to fill the
    pane, and follows the plot's zoom and the waterfall's colour map and levels. It's drawn with 4×
    multisample anti-aliasing, and *3D smoothing* (Display, under the disclosure, while 3D is on) puts
    a Gaussian filter over the heights: Off, Light (the default) or Strong. The filter averages in
    power, not dB, so a narrow carrier keeps most of its height while the noise floor calms down.
  - Changing the range, profile, step, rate, order, dwell, lock reads, settle or bin width doesn't
    clear it. A hard break (a gap with a label such as "400–1000 MHz" or "Quality") marks the
    change, and older rows are re-mapped onto the new frequency axis from their own data: at their
    true frequencies, coarser if they were swept wider, blank where they weren't swept.
- **Footer:** the data source, the frame rate against its cap, and the drawing load.

### Controls

- **Range:** start/stop, step 4/6/8/10 MHz (4/6/8/9 at 10 MSPS), 14 band presets, *Sweep visible
  area*, *Full range*, and a ↶ button back to the previous range.
  - Typed values apply on Enter, Tab or clicking away, and Esc cancels. A typed start above the
    stop, or a stop below the start, keeps the old span.
  - Drag across the plot to select a range, then pick *Zoom in* (Z or Enter) or *Sweep this* (S)
    from the menu that opens. Shift-drag sweeps straight away, Ctrl-drag zooms straight away.
    Ranges smaller than one step become a single retuned step.
  - Zoomed all the way out, drag the frequency axis under the plot, or the box on the overview
    strip, to move the swept range itself (same width). The plot slides as a preview, a chip shows
    the new range, and it retunes when you let go; Esc cancels. The range stops at what the radio
    can tune, so a full-range sweep has nowhere to go.
- **Sweep engine:** a scan profile (F2–F5), with sample rate, order, dwell, lock reads and settle
  under a disclosure. A profile sets those (and the step), and the header then offers a ↶ button
  back to what you had. Range, receiver and display are left alone, except where noted.
  Pass rates are for 24–1800 MHz, measured on the simulator, which models the device's timing:

  | Profile | Step | Dwell | Lock reads | Settle | Also sets | Passes/s |
  |---|---|---|---|---|---|---|
  | Fastest (the default) | 10 MHz | 1 | 2 | 0 | | 5.2 |
  | Quality: flattest part of each step, averaged | 8 MHz | 4 | 3 | 1 | | 1.85 |
  | Weak signals: least noise per bin | 8 MHz | 8 | 2 | 0 | 2.9 kHz bins | 1.3 |
  | Bursts: listens 2/3 of the time, not 1/3 | 10 MHz | 4 | 2 | 0 | peak hold on, cleared | 2.7 |

  All of them use 12 MSPS and VCO-continuous order. A step costs 2 blocks of retune and lock plus
  settle + dwell, so taking lock reads below 2 saves nothing. Turning the lock check off (0) is
  faster, but some steps then read up to 22 dB low (`../docs/measurements.md`). A rate
  change stops streaming, sets the rate and restarts (about 8 ms).
- **Receiver:** RF port, linearity / sensitivity presets or manual LNA/mixer/VGA. Changing these
  stops the sweep, sets the value and restarts (the firmware STALLs tuner requests mid-sweep).
  The bias tee is never touched.
- **Display:** bin width 2.4–39 kHz at 10 MSPS, 2.9–47 kHz at 12 (Welch-averaged below 8192
  points), peak/mean detector, traces, flatten passband, and peak labels Full / Focus / Minimal.
  Beam, switch marks, *Clear peaks* and *Relearn the passband* (a two-step confirm) are under a
  disclosure.
  - **Average:** each bin's exponential average over the *Averaging* time (0.5–60 s, default 10 s;
    the caption says how many passes that is at the current speed). The time runs on the device
    clock, so it means the same on every board and profile. *Average in* dB (a video average: smooth,
    about 2.5 dB under the true mean on noise, little moved by bursts) or Power (the true mean
    power, so busy channels read higher). Changing either restarts the average.
  - **Peak trace** (under the disclosure): *Hold* keeps the highest level since the last clear.
    *Decay* rises at once, holds 1 s, then falls at 1–30 dB/s (default 6), so it shows where
    signals have been lately. *Percentile* tracks the level each bin is under 50/75/90/95 % of the
    time over a 1–120 s window: P90 ignores one-off spikes and rises on channels that are busy more
    than a tenth of the time. The trace's name in Traces follows the mode (Peak, Peak ↓, P90), and
    the CSV's peak column holds whichever it is. The Bursts profile switches to Hold.
- **Levels** (closed by default): spectrum and waterfall ranges, Inferno / Viridis / Grey colour
  maps, history length and row rate.
- **Sliders** never jump. Drag relative to the current value (Shift for fine), use the wheel, or
  double-click to type. Double-click the label to reset. A changed value leaves a dashed ghost and
  a ↶ button back to it.
- **Labels with a dotted underline** explain themselves on hover; click to keep the explanation
  open.
- **Mouse and keys:**
  - Scroll zooms, right- or middle-drag pans (as does dragging the frequency axis), plain drag
    selects (above), double-click resets the view.
  - Drag or scroll the dB gutter to move or stretch the scale.
  - **?** or **F1** shows every shortcut. The main ones:
    - Sweep: Space run/pause, **G** type a range (Start, Tab, Stop, Enter), **1–9** band presets,
      F full range, Backspace previous range, Shift ←/→ move the range by its own width,
      S sweep the visible area, [ / ] step size, M 10/12 MSPS, O visiting order.
    - Profiles: F2 Fastest, F3 Quality, F4 Weak signals, F5 Bursts.
    - View: ←/→ pan, + / − zoom, 0 or Home reset zoom, ↑/↓ move the dB scale.
    - Display: A auto levels, P clear peak, H peak hold, V average, E flatten passband, B beam,
      W 3D waterfall.
    - Files: Ctrl+E export CSV, Ctrl+S snapshot.
  - Keys are ignored while a text field has the cursor, the Zoom/Sweep menu is open or the radio
    picker is open (those take their own keys).
  - **Gesture hints:** with the pointer over the plot, a small note in the corner says what dragging,
    scrolling and panning do. Each part goes away once you've used that gesture, and the 3D view's
    instructions go once you've turned it. While dragging, the selection names what letting go will
    do. The separator, overview strip, frequency axis and dB gutter explain themselves on hover.
    Settings → *Gesture hints* turns them off, and *Show them again* brings them back.
- *Export CSV* and *Snapshot* write to `../captures/` if it exists (gitignored), otherwise to
  `~/.local/share/corvus-sweep/captures/`.

## Seamless stitching

- Each step contributes exactly the bins in [−step/2, +step/2) around its centre. The raw spectrum is
  inverted: RF = centre − (f_IF − IF), with the IF at a quarter of the ADC rate (5 or 6 MHz). Steps tile
  the range with no overlap and no gap.
- The step widths come from the measured passband (`../docs/measurements.md`). At 10 MSPS it
  is flat to ±1 dB over −4.70…+4.20 MHz, so steps go up to 9.3 MHz (8 by default). At 12 MSPS the tuner
  filter is wide open: ±1 dB over −4.85…+5.15 MHz, so steps go up to 10 MHz.
- The remaining seam is the per-step passband ripple: the edges sit 1.6–2.5 dB high and there is a
  small spur at fs/4. *Flatten passband* learns that shape as follows:
  - per bin, the median across all steps of each step's level relative to its own median, averaged over passes;
  - signals sit at different offsets in different steps, so the median rejects them;
  - it learns on any plan with ≥ 12 steps, is stored per sample rate and bin width, and is reused for narrow plans.
- Measured on the RFOne (full range, smoothed over 1 % of a step): **2.2–2.5 → 0.27–0.32 dB p-p** at 12 MSPS
  with 10 MHz steps, and **1.6 → 0.27 dB p-p** at 10 MSPS with 8 MHz steps. `--headless` prints this figure.

## Measured (2026-11-05, ANT, linearity 10, 24–1800 MHz, dwell 1, lock reads 2)

| Firmware | Rate, step | Order | Steps/s | GHz/s | Passes/s | Torn / gaps / misfiled |
|---|---|---|---|---|---|---|
| sweep4 | 10 MSPS, 8 MHz | VCO-continuous | 780–823 | 6.2–6.6 | 3.5–3.7 | 0 / 0 / 0 |
| sweep5 | 10 MSPS, 8 MHz | VCO-continuous | 790–840 | 6.3–6.7 | 3.6–3.8 | 0 / 0 / 0 |
| sweep5 | 12 MSPS, 10 MHz | VCO-continuous | 835–895 | 8.4–9.0 | 4.7–5.0 | 0 / 0 / 0 |
| simulator | 10 MSPS, 8 MHz | VCO / ascending | ~785 / ~671 | | | |

Each step takes exactly 3 blocks at either rate (1 measured, 2 for retune, lock and waiting for a fresh
block). 12 MSPS blocks are 341 µs instead of 410 µs, which is where the extra steps/s come from.

FM broadcast (87.5–108, 3 steps at 10 MSPS) runs at ~85 passes/s. It straddles the 105.6 MHz switch (104.6 at 12 MSPS), which
costs one ~10 ms crossing per pass in either order, and the status bar says so. The release build draws
at 60 fps by default; Settings caps it at 30, 20 or 15 to save power.

## Robustness

- Every block is checked: magic, header == trailer, freq == f0 + step·f_step, lock timeout. A
  `(sweep, step)` model of the firmware's visiting order counts gaps.
- SIGINT/SIGTERM/SIGHUP close the device cleanly. If the window can't close within 1.5 s, a signal
  path calls `sweep_stop` + `stop_rx` itself.
- Under RAW, libhydrasdr reads a sample rate given by value as the real ADC rate, so the device code asks
  for 20 or 24 MSPS. Asking for 12000000 under RAW quietly selects the 6 MSPS IQ setting (12 MSPS real).
- A host killed mid-sweep used to strand the board: every later open failed with LIBUSB −1000.
  It was found while building this tool and fixed on 2026-11-05 in the library (open sends SWEEP_STOP
  first) and in firmware sweep5 (SET_PACKING ends a sweep). Tested: SIGKILL of a sweeping GUI, then reopen, works.

## Layout

| File | |
|---|---|
| `src/hydra.rs` | libloading FFI to libhydrasdr/libairspy, block tags, `Device` (open, configure, sweep, stop → set → start) |
| `src/usbdev.rs` | the built-in USB driver for every board (libhydrasdr's, libairspy's and libhackrf's requests in Rust, nusb), and board discovery on any platform |
| `src/hackrf.rs` | the HackRF's stock sweep: setup and re-tagging its blocks as 5 MHz steps (built-in driver or libhackrf) |
| `src/pluto.rs` | the Pluto over the network: corvus-pluto's protocol (plan, gain, stop), its blocks into the engine |
| `build/` | `build.sh windows`: the Windows cross-build (MinGW in Docker) |
| `src/plan.rs` | sample rate, range → step grid, firmware visiting order, VCO divider model, presets |
| `src/dsp.rs` | raw block → dBFS power on the RF offset grid (rustfft, Hann, Welch) |
| `src/engine.rs` | worker thread: parse, accumulate dwell, stitch, peak/avg, flatness learner, waterfall rows, stats |
| `src/sim.rs` | synthetic band plan with the firmware's hop timing |
| `src/app.rs`, `waterfall.rs` | UI: frame, status, HUD, sidebar, plot, waterfall, keys |
| `src/view3d.rs` | 3D waterfall: a wgpu paint callback (WGSL, heights from a texture, no vertex buffer) and its orbit camera |
| `src/theme.rs` | Instrument tokens, house fonts, and the guide's widgets (never-jumping slider, hints, two-step confirm) |
| `assets/fonts/` | Barlow, Barlow Semi Condensed, Auggie Mono (SIL OFL), compiled in |
| `DESIGN.md` | how the UI follows the auggiesoft guide, and its exceptions |

`cargo test` runs offline checks: tag parsing, tone placement and level at every FFT size and rate (the Pluto's DC bridge too),
plan geometry, divider switch points (and their 1 MHz shift at 12 MSPS) and transitions, and the worker's
stitching and pass counting.

## Known gaps

- *Snapshot* uses eframe's wgpu screen capture. It never returned on a Hyprland headless output and
  is untested on a real display (`grim` works as a fallback).
- **Windows:** `build/build.sh windows` cross-builds `corvus-sweep.exe` (MinGW, in Docker), also in the
  release. It imports only Windows system DLLs and needs nothing else: every board's firmware asks
  Windows for WinUSB through Microsoft OS descriptors (if another driver such as libusbK took a board,
  Zadig can switch it to WinUSB). First tests on Windows hardware are under way.