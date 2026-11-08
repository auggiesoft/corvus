# Corvus firmware

Firmware sweep mode for SDR boards built on the LPC4370 and an R82x tuner. The board retunes itself
and streams only blocks captured after the tuner has settled, each tagged with the step it belongs
to. That's what lets Corvus Sweep stitch a 24–1800 MHz spectrum about five times a second.

| Board | Source | Status |
|---|---|---|
| HydraSDR RFOne | `rfone/`: the RFOne firmware v1.0.2, plus sweep mode | `v1.0.2-sweep5`, in daily use on one board |
| Airspy R2 (and Mini) | `airspy/`: upstream airspyone_firmware master (`cf1a374`, rc10-6 plus R820T2 tuning speedups), plus sweep mode | `v1.0.0-rc10-6-8-gcf1a374-sweep5`, first hardware tests in progress; Mini untested |

`rfone/` is an import of the upstream RFOne firmware (GPL-2.0-or-later, see `rfone/LICENSE`) with
the sweep mode on top. The new sweep files (`rfone/m0/sweep.c`, `rfone/m0/sweep.h`) are MIT; the
built firmware is GPL-2.0-or-later as a whole. `git log -- firmware/rfone` shows the unmodified
import first, then every change. `airspy/` is built the same way on the Airspy firmware, which the
RFOne firmware descends from, so the port is nearly line for line. Normal (non-sweep) operation is
unchanged on both, so the boards still work with the stock host tools and other software.

Both boards speak the same protocol below. Differences: the R2 has no 12 MSPS setting (10 MSPS is its
fastest) and one antenna input, and its host library is libairspy (`../host/airspyone-host/`).

## Versions

The version string keeps the base firmware's version and adds `-sweepN`. The host library
recognises sweep firmware by that suffix, so it must stay.

| Version | Changes |
|---|---|
| sweep1 | sweep mode: the M0 retunes the tuner itself, tagged blocks |
| sweep2 | PLL lock polling before capture; burst PLL register write |
| sweep3 | a start is applied by the main loop, not inside the USB interrupt; the pass counter no longer resets on a restart |
| sweep4 | opt-in VCO-continuous visiting order (flag 0x01), +18.6 % over the full range |
| **sweep5** | SET_PACKING during a sweep ends it instead of being refused, so a host that died mid-sweep can't lock out the next open |

The measurements behind each change are in `../docs/measurements.md`.

## Protocol

Two vendor requests, kept clear of the numbers upstream uses:

- **0x70 SWEEP_START**, with a 16-byte OUT data stage (`sweep_params_t`, little-endian):

  | Field | Type | |
  |---|---|---|
  | `f_start_hz` | u32 | RF centre of step 0 |
  | `f_step_hz` | u32 | centre spacing |
  | `n_steps` | u16 | steps per pass (≥ 1) |
  | `skip_blocks` | u8 | extra blocks dropped after lock (settle) |
  | `dwell_blocks` | u8 | blocks sent per step (≥ 1) |
  | `lock_reads` | u8 | consecutive PLL-locked reads required before capture; 0 = don't poll |
  | `flags` | u8 | 0x01 = VCO-continuous order (sweep4+) |
  | `lock_timeout` | u16 | max lock polls (~0.1 ms each) before capturing anyway |

  The board must already be streaming, in RAW unpacked mode. Sending SWEEP_START again replaces the
  plan without a stop.
- **0x71 SWEEP_STOP**: the stream continues in normal mode.

While a sweep runs, requests that touch the tuner or the sample path (frequency, gains, AGC, RF port,
sample rate, raw tuner register access) are refused with a STALL. Stop the sweep, change the
setting, and start again.

Each 16 KB block (8192 16-bit samples) carries a header in its first 16 bytes and a trailer in its
last 16:

```
header:  u16 0xA55A, u16 0x5357, u16 step, u16 block, u32 sweep, u32 freq_hz
trailer: u16 0x5AA5, u16 lock,   u16 step, u16 block, u32 dma_block_count, u32 freq_hz
```

- `step` is the grid index (`freq_hz = f_start_hz + step · f_step_hz`), `block` counts the dwell
  blocks within the step, and `sweep` is the pass number. It increases on every pass and every start
  and never resets until reboot, so `(sweep, step)` is unique within a boot.
- `lock` is the number of lock polls the step needed. Bit 15 set means it timed out without lock.
- `dma_block_count` is the ADC's block counter at capture, a device clock for timing (one block is
  409.6 µs at 10 MSPS, 341.3 µs at 12 MSPS).
- A block whose header and trailer disagree, or lack the magic numbers, was overwritten by the ADC DMA
  while USB was still reading it. Discard it.
- In VCO-continuous order the steps are visited grouped by tuner VCO divider, every other group
  backwards, and each pass reverses the whole order.

The raw IF spectrum is inverted. RF = `freq_hz` − (f − f_IF), where f_IF is a quarter of the ADC
rate (5 MHz at the 10 MSPS setting, 6 MHz at 12 MSPS).

The host side of all this is in `../host/` (the sweep API in libhydrasdr), and a full client is
`../app/`.

## Building

```
firmware/build/build.sh rfone
```

The build runs in Docker with GCC ARM 4.7-2013q3, the toolchain the upstream firmware's CI uses.
The script downloads it from Launchpad once and checks its MD5. Newer GCCs aren't validated for this
code. The output is `firmware/build/out/rfone/corvus-fw-<version>-rfone.bin`, with its `.elf`,
`.map` and SHA256SUMS.

The build is reproducible. The same toolchain builds upstream's v1.0.2 tag byte-identical to the
official release image, and this tree builds `corvus-fw-v1.0.2-sweep5-rfone.bin` with sha256
`10c3508dfe045587c09f1050a40718726e43d2e2dbc9479bf6f8272ba91ff824` (22860 bytes).

## Flashing

**Unofficial firmware, at your own risk.** A bad flash can leave the board needing DFU recovery,
which means opening the case. Read the upstream flashing guides first (in `rfone/`, or the Airspy
firmware wiki for the R2).

RFOne:

1. Back up what's on the board, and keep the file:
   `hydrasdr_spiflash -s <serial> -r backup.bin -l 1048576`
2. Note the frequency calibration: `hydrasdr_calibrate -s <serial> -r`
3. Flash and reset:
   `hydrasdr_spiflash -s <serial> -w corvus-fw-v1.0.2-sweep5-rfone.bin`, then `hydrasdr_reset -s <serial>`
4. Check that `hydrasdr_info -s <serial>` reports `v1.0.2-sweep5`, and that the calibration still
   reads the same. A firmware write should leave the calibration record (at 0x20000) alone. If it's
   gone, restore it with `hydrasdr_calibrate -s <serial> -w <ppb>`.

`hydrasdr_spiflash` and the other tools are built by `../host/build/build.sh`, or come with the
upstream host tools. Always pass `-s <serial>`: without it some tools act on every board attached.

To go back to stock, flash the upstream v1.0.2 release image (or build the tag with the same
toolchain) the same way.

Airspy R2: the same steps with the airspy tools (`airspy_spiflash -s <serial> -r backup.bin -l 1048576`,
then `-w corvus-fw-…-airspy.bin`), then unplug and replug it to boot the new image. `airspy_spiflash -w`
erases only the first 64 KB, so the calibration record at 0x20000 survives. Recovery is jumper P5 at
1-2 (Boot USB0) and NXP DFU; back to 2-3 afterwards. To go back to stock, flash the upstream release
image; our toolchain rebuilds rc10 and rc10-6 byte for byte.
