# Measurements

What the sweep firmware's design rests on, measured on one RFOne in November 2026. Unless a section
says otherwise: ANT port, bias tee off, linearity gain 10, an indoor antenna, RAW unpacked samples,
24–1800 MHz in 8 MHz steps (222 steps), dwell 1 block, lock reads 2. Firmware versions are
`v1.0.2-sweepN` (see `../firmware/README.md`).

## Retune settling

How long samples are unusable after a retune, measured in normal streaming (host `set_freq`) before
the sweep firmware existed.

- **Power method** (two FM-band tunings with different total power; settle = the first 25 µs block
  from which the new level holds): median **~0.5 ms**, p90 ~0.85 ms, rare outliers 1.4–4 ms. This
  covers the I²C writes, PLL lock, the tuner's filters and the host DDC.
- **Pilot method** (an ATSC 1.0 pilot tracked with a sliding 1024-point FFT across a 4 MHz hop): the
  PLL is on frequency within ±2 kHz in a median **64 µs** (p90 ~120 µs). PLL settling after a small
  hop is ≲0.2 ms.
- `set_freq` itself takes 0.44 ms for a small hop and 0.69 ms for a big jump, because more registers
  change (VCO divider, tracking filter).

## PLL lock check

The R82x PLL's lock time depends on the frequency and the hop size, so a fixed settle time is either
wasteful or wrong. The firmware polls the lock bit (register 2, bit 6) until it reads locked
`lock_reads` times in a row, and only sends blocks that started after that.

| Mode | Steps/s | GHz/s | Full passes/s | Data quality |
|---|---|---|---|---|
| no lock check, skip 0 | 1218 | 9.74 | 5.5 | 40 steps off by up to −22 dB |
| **lock check (2 reads), skip 0** | **680** | **5.44** | **3.1** | matches a reference with 3.7 ms of extra settle |

- Lock times polled over USB (~0.4 ms resolution): most 8 MHz hops lock within 0.38 ms. The low band
  takes 1.5–2 ms, and the 1796 → 28 MHz wrap 7.2 ms. Every hop eventually locked.
- With lock polling, a strong ATSC pilot is at full power and on frequency from the first 102 µs of
  the block after 8, 100 and 400 MHz hops. Without it, after 100 and 400 MHz jumps the pilot is more
  than 5 kHz off in ~92 % of blocks.
- Per-step power against the reference: the null test (reference against itself) shows 11 steps
  differing by more than 1 dB, from bursty TV and LTE signals. Lock mode shows 12, mostly the same
  steps. No lock check shows 40.
- Writing the PLL registers 0x14–0x16 in one I²C burst also removed an intermediate-LO glitch in
  normal retunes (pilot settle median 64 µs → 0 µs).

A step costs 3 blocks at either sample rate: 1 measured block plus 2 blocks of retune, lock and
waiting for a fresh block to start. Retune and lock fit in less than 2 blocks, so the wait is
quantised by the block length, and a single lock read saves nothing over two.

## Visiting order

Hop times in ascending order, from the DMA counter in each block's trailer (409.6 µs per block at
10 MSPS):

| Hop | Blocks | Time |
|---|---|---|
| ordinary 8 MHz hop, up or down | 3 | 1.2 ms |
| low band, 28–60 MHz | 6–10 | 2.5–4 ms |
| **VCO divider switch**: between 44/52, 100/108, 212/220, 436/444, 876/884 MHz | **22–26** | **9–10.6 ms** |
| wrap 1796 → 28 MHz | 20 | 8.2 ms |

The R82x VCO covers 1.77–3.54 GHz, and the PLL picks a divider to bring the LO there. Crossing a
divider switch between adjacent steps moves the VCO across its whole range, and lock time is roughly
5.6 ms per GHz of VCO change. Normal tuning (host `set_freq`) takes the same 8–10 ms for these
crossings, so this isn't a sweep-firmware cost. The autotune clock is already at its fastest
setting; a higher VCO current speeds crossings up but then ordinary hops stop locking.

**VCO-continuous order** (flag 0x01, sweep4+) groups the steps by divider and walks every other
group backwards, so each switch becomes a big RF jump with almost no VCO change:

| Join | Lock time |
|---|---|
| 44 → 100 MHz | 1.1 ms |
| 52 → 108 MHz | 0.7 ms |
| 212 → 436 MHz | 0.4 ms |
| 220 → 444 MHz | 0.4 ms |
| 876 → 1796 MHz | 0.5 ms |
| 884 → 28 MHz | 3.0 ms |

Each pass reverses the whole order, so there's no wrap, and a pass starts by re-measuring the step the
last one ended on. Tags still carry the grid step and its frequency.

| 24–1800 MHz, 8 MHz, dwell 1, lock 2 | Steps/s | Passes/s | GHz/s | Max lock polls | Gaps / torn |
|---|---|---|---|---|---|
| ascending | 683 | 3.08 | 5.46 | 102 | 0 / 0 |
| **VCO-continuous** | **810** | **3.65** | **6.48** | 33 | 0 / 0 |

That's +18.6 %. Steps differing from the ascending reference by more than 1 dB: 13 in the null
test, 15 ascending, 18 in VCO order. The worst steps are the same bursty ones in every run, and the
join and turnaround steps differ from the reference no more than the null test does.

A short plan that straddles a switch (for example 100–124 MHz across the 106 MHz switch) pays a
crossing on every pass in either order, which is why such plans run far below the full-range rate.

## 12 MSPS

The 10 MSPS setting samples the tuner's 5 MHz IF at 20 MSPS real. The 12 MSPS setting uses 24 MSPS
real, a 6 MHz IF and the tuner's widest IF filter.

**Passband**, from a steady carrier at 855.7875 MHz stepped past in 50 kHz steps over ±18 MHz (level
relative to the step centre; the + edge is where the IF approaches DC, the − edge is the tuner's IF
low-pass):

| Setting | Flat ±1 dB | Flat ±2 dB | Flat ±3 dB |
|---|---|---|---|
| 10 MSPS | −4.70 … +4.20 MHz | −4.80 … +4.30 MHz | −4.90 … +4.50 MHz |
| 12 MSPS | −4.85 … +5.15 MHz | −5.00 … +5.25 MHz | −5.05 … +5.50 MHz |

At 12 MSPS a 10 MHz step sits within −2.6 / +0.8 dB of the centre. Corvus Sweep's learned
passband correction takes the per-step ripple to 0.27–0.32 dB p-p at either setting.

**Mirror leakage** (a carrier just outside the step folding back inside it):

| Setting | Source → where it shows | Level re in-band |
|---|---|---|
| 10 MSPS | −5.0 → −4.87 MHz | −8 dB |
| 10 MSPS | −5.5 → −4.52 MHz | −17 dB |
| 10 MSPS | −6.0 → −4.02 MHz | −32 dB |
| 12 MSPS | −6.0 → −5.87 MHz | −32 dB |
| 12 MSPS | −6.5 … −8.0 → −5.5 … −4.0 MHz | below the noise (< −33 dB) |

On the DC side both settings stay at or below −31 dB.

**Speed** (full range, 8 s each, 0 torn, 0 gaps at every setting):

| Setting | Step | Steps/s | Time per step | GHz/s | Passes/s |
|---|---|---|---|---|---|
| 10 MSPS | 8 MHz | 808 | 1.229 ms (3 blocks) | 6.46 | 3.64 |
| 12 MSPS | 8 MHz | 919 | 1.024 ms (3 blocks) | 7.35 | 4.14 |
| 12 MSPS | 9.5 MHz | 871 | 1.024 ms | 8.27 | 4.66 |
| 12 MSPS | 10 MHz | 859 | 1.024 ms | 8.59 | 4.83 |

12 MSPS gives about a third more GHz/s with no firmware change: shorter blocks, plus wider steps.
Going higher would buy little, because the tuner's IF filter is already wide open.

Under RAW, libhydrasdr reads a sample rate given by value as the real ADC rate, so the 12 MSPS setting
is `hydrasdr_set_samplerate(24000000)` and the 10 MSPS one `20000000`. `12000000` under RAW quietly
selects the 6 MSPS IQ setting. Normal unpacked streaming at 12 MSPS (48 MB/s) exceeds the ~45 MB/s
USB ceiling, but a sweep sends only the measured blocks and isn't affected.

## Restarts

Changing gain or port mid-sweep means SWEEP_STOP, the setting, then SWEEP_START, because the firmware
refuses tuner requests during a sweep.

**Restart race (fixed in sweep3).** On sweep2 the start ran inside the USB interrupt. If it landed
while the main loop was in the previous run's retune, the next block went out tagged step 0 but
captured at the old frequency. Every start also reset the sweep number, so a restart within the first
pass repeated tags. Since sweep3 the interrupt only flags the start, the main loop applies it between
retunes, and the sweep number only ever increases, so `(sweep, step)` is unique within a boot.

Test: a plan alternating 28 and 1796 MHz (every retune a slow lock), restarted every 5–30 ms.

| Firmware | Mode | Restarts | Frequency doesn't match step | Duplicate tags |
|---|---|---|---|---|
| sweep2 | stop, gain write, start | 150 | 34 | 28 |
| sweep2 | start over a running sweep | 150 | 67 | 21 |
| sweep3 | stop, gain write, start | 300 | 0 | 0 |
| sweep3 | start over a running sweep | 300 | 0 | 0 |

**Host killed mid-sweep (fixed in sweep5).** A host that dies without SWEEP_STOP leaves the firmware
sweeping. Every libhydrasdr open then sent SET_PACKING during device init, sweep ≤ 4 refused it, and
every later open failed with LIBUSB −1000 until a replug. Two fixes: the library's open sends
SWEEP_STOP to sweep firmware first, and sweep5 ends a sweep on SET_PACKING instead of refusing it,
which also covers the unmodified library.

| Firmware | Unmodified libhydrasdr | Corvus libhydrasdr |
|---|---|---|
| sweep4 | fails (−1000) | opens |
| sweep5 | opens | opens |

## Regression checks after every flash

On every sweep firmware so far: normal 10 MSPS streaming with no drops, a ~45 MB/s USB ceiling, and
the factory frequency calibration unchanged.
