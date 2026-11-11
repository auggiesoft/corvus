# corvus-pluto

The Corvus sweep for the PlutoSDR family (ADALM-Pluto and clones such as the Pluto+). Unlike the RFOne and
Airspy firmware, this isn't firmware: it's a small Linux program (one static ARM binary, about 480 KB, std
Rust, no libiio) that runs on the board's own processor next to whatever firmware the board has. It
retunes the AD936x step by step, captures a block right after each retune, and sends ready-made Corvus
sweep blocks to Corvus Sweep over TCP, tagged exactly like the RFOne firmware's (with two 16-bit words
per IQ sample). Corvus Sweep reaches it with `--pluto HOST`.

Measured on a Pluto+ (Zynq 7010, AD9363 run as an AD9364) over Gigabit Ethernet, 61.44 MSPS, 40 MHz steps,
70–6000 MHz: **about 450 steps/s (420–480), 17–19 GHz/s, about 3 full passes a second**, no torn
blocks or gaps. That's
more than twice the RFOne's 8.6 GHz/s, over a range more than three times wider.

> **Unofficial.** Not affiliated with, endorsed by or supported by Analog Devices or any Pluto clone's
> maker. Product names only say what hardware this works with.

## What the board needs

- **Firmware with ADI's capture path**: the `cf-ad9361-lpc` IIO device with a buffer (`scan_elements`).
  ADI's stock firmware has it, and so do the Pluto+ builds of F5OEO's PlutoDVB2 (tested: 0.5.16.7).
  **Maia SDR's firmware does not**: it keeps iiod for tuning, but its FPGA design has no RX buffer, so it
  can't hand samples to a program (Maia's own docs say a Pluto running Maia "can't send or receive IQ
  samples to a PC").
- A network link to the PC: Ethernet (Pluto+), or the USB network link every Pluto has (the board at
  192.168.2.1, the default here).
- SSH access (root; the stock password is `analog`) to install it.

## Build and install

```
firmware/pluto/build.sh                          # rustup target armv7-unknown-linux-musleabihf, rust-lld
PLUTO_PASS=analog firmware/pluto/install.sh 192.168.1.50    # or with no PLUTO_PASS, type the password
```

`install.sh` copies the binary to `/mnt/jffs2` (the board's small persistent flash area; about 480 KB of
its 896 KB) and starts it. On firmware that runs `/mnt/jffs2/backdoor.sh` at boot (PlutoDVB2), it adds a
line there so it also starts at every boot. Elsewhere it runs until the next reboot.

Then, on the PC: `corvus-sweep --pluto 192.168.1.50` (or `CORVUS_PLUTO=192.168.1.50`), or just `--board pluto`
for a Pluto on USB at 192.168.2.1.

## How it sweeps

- **Retune**: a write to the driver's `out_altvoltage0_RX_LO_frequency`, about 1.1 ms (the driver's
  synthesizer writes, VCO calibration and lock wait). Where the LO crosses about 1.3 GHz or 4 GHz the
  driver reloads its RX gain table, about 20 ms.
- **Flush**: the capture has two DMA blocks of 4088 samples each. After a retune both still hold the
  previous step (measured with a busy band and a quiet one: the third block is the new band), so each
  step reads and drops two blocks, plus any extra `skip` (Corvus Sweep's "Settle").
- **Capture**: 4088 IQ samples per block (66.5 µs at 61.44 MSPS), `dwell` blocks per step.
- **Back and forth**: odd passes walk the steps backwards and the turnaround step isn't retuned, so a pass
  never wraps from 6 GHz to 70 MHz (the wrap crosses both gain-table edges). +11 % over ascending.
- **Fixed settings**: manual gain, DC and quadrature tracking on, the TX LO powered down (its leakage
  lands in the RX band). RF bandwidth 56 MHz at 61.44 MSPS, 28 MHz at 30.72.

Tried and left out: the AD936x's fastlock profiles (calibrated synthesizer settings replayed without a
VCO calibration) only cut an in-octave retune from 1.09 to 0.77 ms through the driver, and moved step
levels by about 0.5 dB. Toggling the buffer instead of reading the stale blocks costs 1 ms per step
against 0.4.

At 61.44 MSPS the noise floor rises towards the band edges (the AD936x's noise-shaped ADC): within 1 dB
over ±20 MHz, +3.5 dB at ±25 MHz. Hence the 40 MHz default step (50 is offered); Corvus Sweep's learned
flatness correction takes the per-step ripple from about 3.7 dB to 0.5 dB peak to peak. The bins around
DC (the residual LO leakage after DC tracking) are bridged from their neighbours.

`corvus-pluto bench` on the board measures all of this: block timing, retunes by jump size, the stale
blocks, and full sweeps in both orders.

## Protocol

TCP port 30433. The PC sends text lines:

```
CONFIG rate=<Hz> bw=<Hz> gain=<dB>
SWEEP f0=<Hz> step=<Hz> n=<steps> [dwell=<blocks>] [skip=<blocks>] [order=bounce|asc]
GAIN <dB>
STOP
```

The board sends frames of `[u32 type][u32 length][payload]` (little-endian): type 1 is one 16384-byte
sweep block, type 2 a text line (`HELLO corvus-pluto <version> model="..." fw="..."` on connect, then
`OK ...` or `ERR ...` per command). The block's tags are the RFOne firmware's (`host/README.md`): step,
block within the step, a pass counter, the LO (low 32 bits), and a block clock (time since the sweep
started, in units of one block). One client at a time.

## Recovery note for Pluto+ boards

If you reflash a Pluto+, write only the firmware (`pluto.frm` or the `firmware.dfu` slot) unless you
have a way back. Writing a third-party bootloader (`boot.frm`/`boot.dfu`/`BOOT.bin`) that doesn't suit
the board leaves it with a steady LED and no USB at all. The way back is booting from a microSD card:
fit the board's SD-H jumper (3V3, or 1V8 on v1 boards), put a working `sdimg` on a FAT32 card (F5OEO's
PlutoDVB2 Pluto+ image worked), and hold the DFU button while powering up. On the board here the rescue
also needed the URST jumper moved from MIO46 to MIO52. Once it boots from the card, flash a firmware
whose bootloader suits the board, then remove the SD-H jumper.
