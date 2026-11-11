# corvus-pluto

The Corvus sweep for the PlutoSDR family (ADALM-Pluto and clones such as the Pluto+). Unlike the RFOne and
Airspy firmware, this isn't firmware: it's a small Linux program (one static ARM binary, about 530 KB, Rust,
no libiio) that runs on the board's own processor next to whatever firmware the board has. It retunes the
AD936x step by step, captures a block right after each retune, and sends ready-made Corvus sweep blocks to
Corvus Sweep over TCP, tagged exactly like the RFOne firmware's (with two 16-bit words per IQ sample).
Corvus Sweep reaches it with `--pluto HOST`.

Measured on a Pluto+ (Zynq 7010, AD9363 run as an AD9364) over Gigabit Ethernet, 61.44 MSPS, 40 MHz steps,
70–6000 MHz: **about 3,300 steps/s, 130 GHz/s, 22 full passes a second** in fast mode, no torn blocks, gaps
or unlocked steps. Over the board's USB network link, about 1,900 steps/s (76 GHz/s). The driver path (the
fallback) does about 650 steps/s.

> **Unofficial.** Not affiliated with, endorsed by or supported by Analog Devices or any Pluto clone's
> maker. Product names only say what hardware this works with.

## What the board needs

- **Firmware with ADI's capture path**: the `cf-ad9361-lpc` IIO device with a buffer (`scan_elements`).
  ADI's stock firmware has it, and so do the Pluto+ builds of F5OEO's PlutoDVB2 (tested: 0.5.16.7).
  **Maia SDR's firmware does not**: it keeps iiod for tuning, but its FPGA design has no RX buffer, so it
  can't hand samples to a program (Maia's own docs say a Pluto running Maia "can't send or receive IQ
  samples to a PC").
- A network link to the PC: Ethernet (Pluto+), or the USB network link every Pluto has (the board at
  192.168.2.1, the default here). Fast mode sends 40–50 MB/s, so Ethernet gets the most out of it.
- SSH access (root; the stock password is `analog`) to install it.
- For fast mode, also: root access to `/dev/mem`, the AD936x on the Zynq's SPI0 (as on every Pluto), the ADI
  kernel's IIO mmap block interface and the driver's fastlock attributes (both in ADI's kernels), and **nothing
  else using the AD936x while it sweeps** (see below). Without them it sweeps through the driver instead.

## Build and install

```
firmware/pluto/build.sh                          # rustup target armv7-unknown-linux-musleabihf, rust-lld
PLUTO_PASS=analog firmware/pluto/install.sh 192.168.1.50    # or with no PLUTO_PASS, type the password
STOP_DATV=1 firmware/pluto/install.sh 192.168.1.50           # on PlutoDVB2: also stop its DATV services (fast mode)
```

`install.sh` copies the binary to `/mnt/jffs2` (the board's small persistent flash area; about 530 KB of
its 896 KB) and starts it. On firmware that runs `/mnt/jffs2/backdoor.sh` at boot (PlutoDVB2), it adds a
line there so it also starts at every boot. Elsewhere it runs until the next reboot.

Then, on the PC: `corvus-sweep --pluto 192.168.1.50` (or `CORVUS_PLUTO=192.168.1.50`), or just `--board pluto`
for a Pluto on USB at 192.168.2.1. `CORVUS_PLUTO_MODE=driver` asks for the driver path.

## Fast mode

The AD936x itself retunes in about a microsecond when it doesn't have to recalibrate; the time goes in the
driver (~20 SPI transfers through the kernel at ~45 µs each, plus sleeps while it polls for lock) and in the
capture (blocks queued before a retune hold the previous band). Fast mode skips both:

- **Fastlock profiles.** On a new plan, each step's synthesizer settings are calibrated once through the
  driver (a normal retune, then its fastlock store and save: ~0.7 s for 149 steps, cached for 10 minutes).
  Sweeping, each step is one register write switching to the step's profile; the level is right within
  about a microsecond. The chip holds 8 profiles, so the one 7 steps ahead is written into the slot just
  left (only the words that differ, ~7 of 16) while the current block fills.
- **Direct SPI.** Registers are written over the Zynq's SPI0 controller directly (`/dev/mem`), ~2 µs per
  transfer. Fast mode only takes the bus when the controller is idle the way the kernel leaves it and no
  SPI interrupts arrive for 30 ms, and checks after every pass that the kernel hasn't used the bus since; if it
  has, it hands the radio back and carries on through the driver (`NOTE fast mode off: ...`).
- **Gain tables.** The driver loads a different RX gain table below 1.3 GHz, to 4 GHz and above; fastlock
  doesn't, so the sweep writes the table where a pass crosses a band edge (~0.5 ms), with the table index
  that gives the same gain in dB. The tables are Analog Devices' (from their no-OS driver, BSD-3-Clause:
  `LICENSE-no-OS.txt`); one read back from a Pluto matched them.
- **mmap capture blocks.** The ADI kernel's IIO block interface lets the sweep queue each block itself,
  after the switch, so nothing is captured before the retune and nothing needs dropping.
- **Two cores.** The sweep loop runs on CPU1 with the capture DMA interrupt; the network sending runs on
  CPU0 with the Ethernet interrupt. On a Pluto+ the network side is the limit: ~45 MB/s of TCP keeps CPU0 busy.
  Blocks travel 12-bit packed when the client asks (`pack=12`, as Corvus Sweep does): 25 % fewer bytes.
- **Handing back.** On STOP, a new CONFIG or plan, or a disconnect: the SPI controller's settings and the DMA
  interrupt restored, then two driver retunes in different bands, so the driver leaves fastlock mode and
  reloads the gain table it believes is loaded.
- **Checks.** Each step reads the synthesizer's lock bit; an unlocked step is flagged in its block's trailer
  (bit 15 of word 1, as the RFOne firmware does) and its profile calibrated again after the pass.

Measured levels against the driver path at the same frequencies (112 steady steps of 149): median +0.05 dB,
90 % within −0.03…+0.27 dB. Two fast runs agree to 0.01 dB.

**Nothing else may drive the AD936x while fast mode sweeps:** another program reading or writing the radio
through iiod or sysfs uses the same SPI bus. Fast mode notices after the pass and drops to the driver path,
but a transfer that collides with one of its own can misconfigure the chip until the next CONFIG. PlutoDVB2's
MQTT control daemon polls the chip ~24 times a second, which keeps fast mode off. `STOP_DATV=1 install.sh HOST`
also installs `plutodvb2-quiet.sh`, which stops PlutoDVB2's DATV services at every boot (the MQTT control daemon
and broker, the DATV streamer, its LO/TX watcher scripts and its NFS server) and keeps iiod, SSH, DHCP and the
web server. To undo it, delete its line from `/mnt/jffs2/backdoor.sh` and reboot.

## Tuning range

The driver tunes the RX LO over 46.88 MHz–6 GHz: the VCO's 6–12 GHz divided by 2 to 128. (It advertises
46,875,001 Hz as its minimum but refuses it.) corvus-pluto goes further two ways:

- **Past the driver's range:** it tunes the nearest edge through the driver (for the VCO settings and
  divider), then writes the synthesizer's integer and fractional words for the target itself through the
  driver's debugfs register access; writing integer byte 0 starts a VCO calibration, now outside 6–12 GHz.
  At start-up it checks how far the chip locks, from 46 MHz and 6150 MHz inwards, and reports the range in
  its greeting (`lo=46000000-6150000000`). On a Pluto+ the PLL locked 45–6170 MHz; at 46 MHz the board's
  clock spurs (harmonics of 61.44 MHz) sit exactly where they should, at 45 MHz they don't, so 46 is the
  floor, and 6150 leaves a margin under 6170.
- **Past the chip's range:** a step centred further out is taken at the nearest end and shifted digitally
  (each block multiplied by a complex exponential, its mean taken out first so the LO's leakage doesn't land
  off centre). The step's window has to stay inside the RF bandwidth around that LO, so the cover reaches
  half the RF bandwidth further: about 18–6178 MHz at 61.44 MSPS. Corvus Sweep takes the range from the
  greeting.

Neither is guaranteed by the chip's data sheet; expect less accuracy towards the ends.

## The live view

A one-step plan (Corvus Sweep's live view) streams: the board stays on the frequency (one profile switch, no
more), keeps half its capture blocks queued back to back, and sends every block. At 15.36 MSPS and below the
network keeps up and about 94 % of the signal is captured (each block still drops its first 256 samples);
at 61.44 and 30.72 MSPS it's ~3,800 blocks/s, network-bound. The lower rates (CONFIG rate=15360000, 7680000,
3840000; the driver accepts down to 2.083 MSPS) give narrower spans with finer bins.

## The driver path

What runs when fast mode can't (`mode=driver`, or a board without the above):

- **Retune**: a write to the driver's `out_altvoltage0_RX_LO_frequency`, about 1.1 ms. Where the LO crosses
  about 1.3 GHz or 4 GHz the driver reloads its RX gain table, about 20 ms.
- **Flush**: the capture has two DMA blocks of 4088 samples each, filled when they're queued. After a retune
  both still hold the previous step, so each step reads and drops them first, plus any extra `settle` blocks.
- **Back and forth**: odd passes walk the steps backwards and the turnaround step isn't retuned, so a pass
  never wraps from 6 GHz to 70 MHz (the wrap crosses both gain-table edges). +11 % over ascending.

Both paths: manual gain, DC and quadrature tracking on, the TX LO powered down (its leakage lands in the RX
band), RF bandwidth 56 MHz at 61.44 MSPS, 28 MHz at 30.72; 4088 IQ samples per block (66.5 µs at 61.44 MSPS).

At 61.44 MSPS the noise floor rises towards the band edges (the AD936x's noise-shaped ADC): within 1 dB
over ±20 MHz, +3.5 dB at ±25 MHz. Hence the 40 MHz default step (50 is offered); Corvus Sweep's learned
flatness correction takes the per-step ripple from about 3.7 dB to 0.5 dB peak to peak. The bins around
DC (the residual LO leakage after DC tracking) are bridged from their neighbours.

`corvus-pluto bench` on the board measures all of this: block timing, retunes by jump size, the stale
blocks, full sweeps through the driver in both orders, then fast mode (calibration, and steps/s with no
per-block work, with the copy out of the DMA block, and with the frame built).

## Protocol

TCP port 30433. The PC sends text lines:

```
CONFIG rate=<Hz> bw=<Hz> gain=<dB>
SWEEP f0=<Hz> step=<Hz> n=<steps> [dwell=<blocks>] [settle=<blocks>] [order=bounce|asc] [mode=fast|driver] [pack=12]
GAIN <dB>
STOP
```

`settle` counts extra blocks dropped after each retune, beyond what the board needs itself. corvus-pluto 0.1
took `skip=<2 + settle>` instead; 0.2 still accepts it, and Corvus Sweep sends both so it works with either.
`mode` defaults to fast.

The board sends frames of `[u32 type][u32 length][payload]` (little-endian): type 1 is one 16384-byte
sweep block, type 2 a text line (`HELLO corvus-pluto <version> model="..." fw="..." modes=fast|driver lo=<min>-<max>` on
connect, then `OK ...` or `ERR ...` per command, and `NOTE ...` when fast mode stops on its own), type 3 one
sweep block with 12-bit packed samples (12296 bytes: the 8 header words, 4088 I, Q pairs in 3 bytes each,
I bits 0–7, then I bits 8–11 | Q bits 0–3 << 4, then Q bits 4–11, then the 8 trailer words). `OK SWEEP` says
which `mode` it runs, and with `note="..."` why not fast when it was asked for. The block's tags are the RFOne
firmware's (`host/README.md`): step, block within the step, a pass counter, the LO (low 32 bits), a block
clock (time since the sweep started, in units of one block), and the lock flag. One client at a time.

## Recovery note for Pluto+ boards

If you reflash a Pluto+, write only the firmware (`pluto.frm` or the `firmware.dfu` slot) unless you
have a way back. Writing a third-party bootloader (`boot.frm`/`boot.dfu`/`BOOT.bin`) that doesn't suit
the board leaves it with a steady LED and no USB at all. The way back is booting from a microSD card:
fit the board's SD-H jumper (3V3, or 1V8 on v1 boards), put a working `sdimg` on a FAT32 card (F5OEO's
PlutoDVB2 Pluto+ image worked), and hold the DFU button while powering up. On the board here the rescue
also needed the URST jumper moved from MIO46 to MIO52. Once it boots from the card, flash a firmware
whose bootloader suits the board, then remove the SD-H jumper.
