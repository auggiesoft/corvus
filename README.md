# Corvus

Firmware sweep mode and a live wideband spectrum analyser for the HydraSDR RFOne, the Airspy R2 and
the Airspy Mini.

The board retunes itself and streams only blocks captured after the tuner's PLL has locked, each
tagged with the step it belongs to. On the host, Corvus Sweep stitches those steps into one seamless
spectrum: on the RFOne, **24–1800 MHz about five times a second (8.6 GHz/s), lock-verified, with
12-bit samples.**

| Board | Firmware | Rate | Steps | 24–1800 MHz | Corvus Sweep |
|---|---|---|---|---|---|
| HydraSDR RFOne | `firmware/rfone/`, `v1.0.2-sweep5` | 12 MSPS (or 10) | up to 10 MHz | 8.6 GHz/s, ~5 passes/s | `--board rfone` (default) |
| Airspy R2 | `firmware/airspy/`, `…-sweep5` | 10 MSPS | up to 9 MHz | 6.5 GHz/s, ~3.65 passes/s | `--board airspy` |
| Airspy Mini | the same image as the R2 | 6 MSPS | up to 4 MHz | 2.9 GHz/s, ~1.6 passes/s | `--board mini` |

All three speak the same sweep protocol, and the same API is added to their host libraries
(libhydrasdr for the RFOne, libairspy for the Airspys). The R2/Mini image detects which board it's on
at boot. For comparison, the Mini's stock firmware driven from the PC managed 0.2–0.4 GHz/s.

![Corvus Sweep on the full 24–1800 MHz range, running on its simulator](docs/corvus-sweep.png)

> **Unofficial.** Corvus is an independent project. It is not affiliated with, endorsed by or
> supported by HydraSDR or Airspy. Product names are used only to say what hardware Corvus works
> with. For official firmware, tools and support, go to the manufacturer.

## What's here

| Directory | |
|---|---|
| [`firmware/`](firmware/) | the Corvus firmware: sweep mode on top of the RFOne firmware v1.0.2 (`rfone/`) and the Airspy R2/Mini firmware (`airspy/`). Protocol, building, flashing, recovery |
| [`host/`](host/) | libhydrasdr (`hydrasdr-host/`) and libairspy (`airspyone-host/`) with the sweep API added (drop-in supersets), builds, on-device tests |
| [`app/`](app/) | **Corvus Sweep**, the spectrum analyser (Rust, egui) |
| [`tools/`](tools/) | `corvus_sweep.py`, a command-line sweep client (rates, integrity, CSV) |
| [`docs/`](docs/) | [measurements](docs/measurements.md): settling, PLL lock, visiting order, 12 MSPS, the Airspy Mini's passband, restarts |

## Try it

Corvus Sweep has a simulator that models the firmware's timing, so you can try it with no radio:

```
cd app
cargo run --release -- --sim
```

With a board (everything builds in Docker):

| | RFOne | Airspy R2 or Mini |
|---|---|---|
| 1. Build the library | `host/build/build.sh linux` | `host/build/build.sh airspy-linux` |
| 2. Build the firmware | `firmware/build/build.sh rfone` | `firmware/build/build.sh airspy` |
| 3. Flash it | [firmware/README.md](firmware/README.md#flashing) | the same section, with the airspy tools |
| 4. Run Corvus Sweep (in `app/`) | `cargo run --release` | `cargo run --release -- --board airspy` (R2) or `--board mini` |

**Back up the board's flash before step 3**, and keep the file. The firmware README has the commands,
and how to recover a board if a flash goes wrong (that means opening the case).

## Status

- The RFOne firmware (`v1.0.2-sweep5`), the library and Corvus Sweep are used daily on one RFOne
  under Linux. Every firmware version has passed the same regression checks: normal streaming is
  unchanged, and the factory calibration survives flashing.
- The Windows library builds but hasn't been tested on hardware, and the app hasn't been built for
  Windows.
- The Airspy R2 and Mini were added on 2026-11-08 and tested on one board each: firmware flashed from
  stock rc10, calibration intact, normal streaming unchanged with the stock tools, sweeps with clean
  integrity, `host/tests/test_airspy_sweep_api.py` passing, and a killed host never locking out the next
  open. The Mini's main limit is mirror leakage of about −20 dB at its 6 MSPS setting
  ([measurements](docs/measurements.md#airspy-mini)).
- The other R2 hasn't been flashed. Boards on the Corvus firmware still work as normal receivers with
  other software.

## Licensing

Corvus's own code is MIT ([`LICENSE`](LICENSE)). That file covers everything in this repository except the
parts below that keep their own licences:

| Part | Licence |
|---|---|
| `firmware/rfone/` | GPL-2.0-or-later (`firmware/rfone/LICENSE`), except the new sweep files `m0/sweep.c` and `m0/sweep.h`, which are MIT; libopencm3 inside it is LGPL-3.0-or-later. The built firmware is GPL-2.0-or-later as a whole |
| `firmware/airspy/` | GPL-2.0-or-later (`firmware/airspy/LICENSE`), except the new sweep files `airspy_m0/sweep.c` and `airspy_m0/sweep.h`, which are MIT; libopencm3 inside it is LGPL-3.0-or-later |
| `host/hydrasdr-host/` | libhydrasdr: MIT, with `hydrasdr.c`/`hydrasdr.h` BSD-3-Clause (`libhydrasdr/LICENSE.md`); tools: GPL-2.0 (`hydrasdr-tools/LICENSE.md`) |
| `host/airspyone-host/` | libairspy: BSD-3-Clause (file headers); tools: see their headers |
| `app/assets/fonts/` | SIL Open Font License 1.1 (`app/assets/fonts/`) |
| everything else (`app/`, `tools/`, the build scripts, the docs) | MIT ([`LICENSE`](LICENSE)) |

`firmware/rfone/`, `firmware/airspy/`, `host/hydrasdr-host/` and `host/airspyone-host/` each started as
an unmodified import of the upstream source (see the import commits). The files Corvus changed carry a notice saying so, and `git log` shows every
change.

## Credits

Corvus is built on the RFOne firmware and host library by Benjamin Vernoux, and on the Airspy R2/Mini
firmware and libairspy by Youssef Touil and Benjamin Vernoux. Both descend from the HackRF project
(Jared Boone, Michael Ossmann and others), with contributions from Ian Gilmour and others, and use
libopencm3.
