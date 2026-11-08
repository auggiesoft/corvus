# Corvus

Firmware sweep mode and a live wideband spectrum analyser for the HydraSDR RFOne.

The board retunes itself and streams only blocks captured after the tuner's PLL has locked, each
tagged with the step it belongs to. On the host, Corvus Sweep stitches those steps into one seamless
spectrum: **24–1800 MHz about five times a second (8.6 GHz/s), lock-verified, with 12-bit samples.**

![Corvus Sweep on the full 24–1800 MHz range, running on its simulator](docs/corvus-sweep.png)

> **Unofficial.** Corvus is an independent project. It is not affiliated with, endorsed by or
> supported by HydraSDR or Airspy. Product names are used only to say what hardware Corvus works
> with. For official firmware, tools and support, go to the manufacturer.

## What's here

| Directory | |
|---|---|
| [`firmware/`](firmware/) | the Corvus firmware: sweep mode on top of the RFOne firmware v1.0.2. Protocol, building, flashing |
| [`host/`](host/) | libhydrasdr with the sweep API added (a drop-in superset), Linux and Windows builds, on-device tests |
| [`app/`](app/) | **Corvus Sweep**, the spectrum analyser (Rust, egui) |
| [`tools/`](tools/) | `corvus_sweep.py`, a command-line sweep client (rates, integrity, CSV) |
| [`docs/`](docs/) | [measurements](docs/measurements.md): settling, PLL lock, visiting order, 12 MSPS, restarts |

## Try it

Corvus Sweep has a simulator that models the firmware's timing, so you can try it with no radio:

```
cd app
cargo run --release -- --sim
```

With an RFOne:

1. Build the library: `host/build/build.sh linux` (Docker).
2. Build and flash the firmware: `firmware/build/build.sh rfone`, then follow
   [firmware/README.md](firmware/README.md#flashing). **Back up the board first.**
3. Run `cargo run --release` in `app/`.

## Status

- The RFOne firmware (`v1.0.2-sweep5`), the library and Corvus Sweep are used daily on one RFOne
  under Linux. Every firmware version has passed the same regression checks: normal streaming is
  unchanged, and the factory calibration survives flashing.
- The Windows library builds but hasn't been tested on hardware, and the app hasn't been built for
  Windows.
- Planned: the same sweep mode for the Airspy R2, as another firmware target with the same protocol.

## Licensing

| Part | Licence |
|---|---|
| `firmware/rfone/` | GPL-2.0-or-later (`firmware/rfone/LICENSE`); libopencm3 inside it is LGPL-3.0-or-later |
| `host/hydrasdr-host/` | libhydrasdr: MIT, with `hydrasdr.c`/`hydrasdr.h` BSD-3-Clause (`libhydrasdr/LICENSE.md`); tools: GPL-2.0 (`hydrasdr-tools/LICENSE.md`) |
| `app/assets/fonts/` | SIL Open Font License 1.1 (`app/assets/fonts/`) |
| everything else (`app/`, `tools/`, the build scripts, the docs) | MIT ([`LICENSE`](LICENSE)) |

`firmware/rfone/` and `host/hydrasdr-host/` started as unmodified imports of the upstream sources
(see the first commits). The files Corvus changed carry a notice saying so, and `git log` shows every
change.

## Credits

Corvus is built on the RFOne firmware and host library by Benjamin Vernoux, which descend from the
Airspy and HackRF projects (Youssef Touil, Ian Gilmour, Jared Boone, Michael Ossmann and others), and
on libopencm3.
