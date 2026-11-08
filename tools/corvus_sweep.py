"""Spectrum sweep from the command line, using the Corvus sweep firmware (v1.0.2-sweepN, see firmware/README.md).

The device retunes itself and sends only tagged 16 KB blocks of raw 20 MSPS real samples captured
after the tuner settled. This tool configures streaming through libhydrasdr (RAW, unpacked) and starts
the sweep with hydrasdr_sweep_start() from the Corvus libhydrasdr (host/build/out/linux, see
host/README.md). With a stock libhydrasdr it falls back to sending vendor request 0x70 on a second
USB handle (pyusb, Linux only). It FFTs every block on the host and reports sweep rate and integrity.
Optionally writes hackrf_sweep-style CSV.

Usage:
  uv run --no-project --with numpy --with pyusb python tools/corvus_sweep.py \
      [--serial HEX] --start 24 --stop 1800 [--step 8] [--skip 1] [--dwell 1] [--gain 10] [--seconds 5] \
      [--bin-khz 10] [--csv out.csv]
  ... corvus_sweep.py --selftest     (offline check of block parsing and RF mapping)
  --lib PATH picks the libhydrasdr to load (default: the Corvus build if present, else the system one).
  --serial HEX picks the board (default: env CORVUS_SERIAL, else the first RFOne found).
"""
import argparse
import ctypes as C
import math
import os
import queue
import struct
import sys
import time
from datetime import datetime

import numpy as np

SERIAL = int(os.environ.get("CORVUS_SERIAL", "0"), 16)  # 0 = the first RFOne found
FS_REAL = 20_000_000          # raw ADC rate at the 10 MSPS IQ setting
F_IF = 5_000_000              # R82x IF for that setting
BLOCK_WORDS = 8192            # 16 KB
TAG_WORDS = 8                 # 16-byte header and trailer
BLOCK_S = BLOCK_WORDS / FS_REAL
NFFT = 8192
SWEEP_START, SWEEP_STOP = 0x70, 0x71
FLAG_VCO_ORDER = 0x01         # firmware sweep4+: VCO-continuous visiting order
IF_HZ = 5_000_000             # R82x IF at the 10 MSPS setting (LO = RF + IF)


def vco_divider(rf_hz):
    """Divider r82x_set_pll picks for this RF (6 = none)."""
    lo = (rf_hz + IF_HZ) & 0xFFFFFFFF
    for d in range(6):
        vco = (lo << (d + 1)) & 0xFFFFFFFF          # uint32 like the firmware
        if 1_770_000_000 <= vco <= 3_900_000_000:
            return d
    return 6


class Order:
    """The firmware's visiting order, to predict the next (sweep, step) key and spot gaps.
    Plain: 0..n-1 then the next sweep. VCO order (sweep4): runs of equal divider, odd runs backwards,
    every pass reversed; a pass starts by re-measuring the step the previous one ended on."""

    def __init__(self, f0, step_hz, n, vco):
        self.n, self.vco, self.s0 = n, vco, None
        self.seq = list(range(n))
        if vco:
            runs = []
            for i in range(n):
                d = vco_divider(f0 + i * step_hz)
                if not runs or runs[-1][0] != d:
                    runs.append((d, []))
                runs[-1][1].append(i)
            if len(runs) <= 8:
                self.seq = [i for r, (_, idx) in enumerate(runs) for i in (idx[::-1] if r & 1 else idx)]
            else:
                self.vco = False
        self.pos = {s: i for i, s in enumerate(self.seq)}

    def next(self, key):
        swp, step = key
        if self.s0 is None:
            self.s0 = swp
        i = self.pos[step]
        if not self.vco:
            return (swp, self.seq[i + 1]) if i + 1 < self.n else (swp + 1, self.seq[0])
        if (swp - self.s0) % 2 == 0:
            return (swp, self.seq[i + 1]) if i + 1 < self.n else (swp + 1, step)
        return (swp, self.seq[i - 1]) if i > 0 else (swp + 1, step)
PATCHED_LIB = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../host/build/out/linux/libhydrasdr.so.1")
LIB = PATCHED_LIB if os.path.exists(PATCHED_LIB) else "libhydrasdr.so.1"


class SweepParams(C.Structure):
    _fields_ = [("f_start_hz", C.c_uint32), ("f_step_hz", C.c_uint32), ("n_steps", C.c_uint16),
                ("skip_blocks", C.c_uint8), ("dwell_blocks", C.c_uint8), ("lock_reads", C.c_uint8),
                ("flags", C.c_uint8), ("lock_timeout", C.c_uint16)]


class SweepBlockInfo(C.Structure):
    _fields_ = [("freq_hz", C.c_uint32), ("sweep", C.c_uint32), ("dma_count", C.c_uint32), ("step", C.c_uint16),
                ("block", C.c_uint16), ("lock_polls", C.c_uint16), ("lock_timed_out", C.c_uint8),
                ("reserved", C.c_uint8)]


def parse_block(w):
    """Return (step, block, sweep, dma_count, freq_hz) for a valid tagged block, else None.
    Header and trailer must agree; a mismatch means the ADC DMA overwrote the block mid-transfer."""
    h, t = w[:TAG_WORDS], w[-TAG_WORDS:]
    if h[0] != 0xA55A or h[1] != 0x5357 or t[0] != 0x5AA5:
        return None
    if h[2] != t[2] or h[3] != t[3] or h[6] != t[6] or h[7] != t[7]:
        return None
    return int(h[2]), int(h[3]), int(h[4]) | int(h[5]) << 16, int(t[4]) | int(t[5]) << 16, int(h[6]) | int(h[7]) << 16


def lock_info(w):
    """PLL lock polls the step's retune needed (firmware sweep2+), and whether it timed out."""
    v = int(w[-TAG_WORDS + 1])
    return v & 0x7FFF, bool(v & 0x8000)


class Spectrum:
    """Raw block -> power on an RF offset grid. The raw IF spectrum is inverted:
    RF = fc - (f_if - F_IF) (measured against the library's IQ output on stock firmware, correlation 0.995)."""

    def __init__(self, half_span_hz, bin_hz):
        n = BLOCK_WORDS - 2 * TAG_WORDS
        self.win = np.hanning(n).astype(np.float32)
        self.win_norm = (self.win ** 2).sum() * FS_REAL
        f_if = np.fft.rfftfreq(NFFT, 1 / FS_REAL)
        off = -(f_if - F_IF)
        sel = np.where(np.abs(off) <= half_span_hz)[0]
        order = np.argsort(off[sel])
        self.idx = sel[order]                      # FFT bins in ascending RF order
        self.off = off[self.idx]
        g = max(1, int(round(bin_hz / (FS_REAL / NFFT))))
        self.group = g
        m = (len(self.idx) // g) * g
        self.idx, self.off = self.idx[:m], self.off[:m]
        self.bin_hz = g * FS_REAL / NFFT
        self.out_off = self.off.reshape(-1, g).mean(1)

    def power(self, words):
        x = (words[TAG_WORDS:-TAG_WORDS] & 0x0FFF).astype(np.float32)
        x -= x.mean()
        X = np.fft.rfft(x * self.win, n=NFFT)
        p = (np.abs(X[self.idx]) ** 2) / self.win_norm
        return p.reshape(-1, self.group).mean(1)


def selftest():
    """Synthetic tagged blocks with a tone at a known RF offset -> parse + map back."""
    fc, tone_off = 500_000_000, +1_234_567
    n = np.arange(BLOCK_WORDS - 2 * TAG_WORDS)
    f_if = F_IF - tone_off                         # inverted spectrum
    x = (2048 + 800 * np.cos(2 * np.pi * f_if / FS_REAL * n)).astype(np.uint16)
    hdr = struct.pack("<HHHHII", 0xA55A, 0x5357, 3, 0, 7, fc)
    trl = struct.pack("<HHHHII", 0x5AA5, 0x8005, 3, 0, 1234, fc)
    w = np.frombuffer(hdr + x.tobytes() + trl, dtype=np.uint16)
    assert parse_block(w) == (3, 0, 7, 1234, fc), parse_block(w)
    assert lock_info(w) == (5, True)
    bad = w.copy()
    bad[-6] ^= 1                                    # corrupt trailer step
    assert parse_block(bad) is None
    sp = Spectrum(4e6, 10e3)
    p = sp.power(w)
    got = sp.out_off[np.argmax(p)]
    assert abs(got - tone_off) < sp.bin_hz, (got, tone_off)
    print(f"selftest OK: tag parse, corruption detect, tone at {tone_off/1e3:+.1f} kHz -> {got/1e3:+.1f} kHz "
          f"(bin {sp.bin_hz/1e3:.2f} kHz)")
    lib = C.CDLL(LIB)
    if hasattr(lib, "hydrasdr_sweep_parse_block"):
        f = lib.hydrasdr_sweep_parse_block
        f.argtypes = [C.POINTER(C.c_uint16), C.POINTER(SweepBlockInfo)]
        info = SweepBlockInfo()
        ptr = lambda a: np.ascontiguousarray(a).ctypes.data_as(C.POINTER(C.c_uint16))  # noqa: E731
        assert f(ptr(w), C.byref(info)) == 0
        assert (info.step, info.block, info.sweep, info.dma_count, info.freq_hz) == parse_block(w)
        assert (info.lock_polls, bool(info.lock_timed_out)) == lock_info(w)
        assert f(ptr(bad), C.byref(info)) == 2
        assert f(ptr(np.full(BLOCK_WORDS, 2048, np.uint16)), None) == 1
        print(f"selftest OK: hydrasdr_sweep_parse_block in {LIB} agrees (valid / torn / untagged)")


class Transfer(C.Structure):
    _fields_ = [("device", C.c_void_p), ("ctx", C.c_void_p), ("samples", C.c_void_p),
                ("sample_count", C.c_int), ("dropped_samples", C.c_uint64), ("sample_type", C.c_int)]


class Sweeper:
    """Context manager: configures the RFOne, starts the firmware sweep, iterates raw blocks.
    Iterating yields (tag, words) for every 16 KB block; tag is None for untagged/torn blocks."""

    def __init__(self, f0, step_hz, n_steps, skip=0, dwell=1, gain=10, port=0, lock_reads=2, lock_timeout=200,
                 lib=None, flags=0):
        self.params = struct.pack("<IIHBBBBH", f0, step_hz, n_steps, skip, dwell, lock_reads, flags, lock_timeout)
        self.sp = SweepParams(f0, step_hz, n_steps, skip, dwell, lock_reads, flags, lock_timeout)
        self.lib_path = lib or LIB
        self.gain, self.port = gain, port
        self.q = queue.Queue()

    def __enter__(self):
        CB = C.CFUNCTYPE(C.c_int, C.POINTER(Transfer))
        lib = self.lib = C.CDLL(self.lib_path)
        self.native = hasattr(lib, "hydrasdr_sweep_start")
        if self.native:
            lib.hydrasdr_sweep_start.argtypes = [C.c_void_p, C.POINTER(SweepParams)]
            lib.hydrasdr_sweep_stop.argtypes = [C.c_void_p]
            lib.hydrasdr_sweep_supported.argtypes = [C.c_void_p]
        lib.hydrasdr_open_sn.argtypes = [C.POINTER(C.c_void_p), C.c_uint64]
        for fn in ("hydrasdr_set_samplerate", "hydrasdr_set_sample_type", "hydrasdr_set_linearity_gain",
                   "hydrasdr_set_rf_port", "hydrasdr_set_packing"):
            getattr(lib, fn).argtypes = [C.c_void_p, C.c_uint32]
        for fn in ("hydrasdr_stop_rx", "hydrasdr_close"):
            getattr(lib, fn).argtypes = [C.c_void_p]
        lib.hydrasdr_start_rx.argtypes = [C.c_void_p, CB, C.c_void_p]
        lib.hydrasdr_version_string_read.argtypes = [C.c_void_p, C.c_char_p, C.c_uint8]

        def on_rx(t):
            tr = t.contents
            self.q.put(np.ctypeslib.as_array(C.cast(tr.samples, C.POINTER(C.c_uint16)), shape=(tr.sample_count,)).copy())
            return 0

        self.cb = CB(on_rx)
        self.dev = C.c_void_p()
        assert lib.hydrasdr_open_sn(C.byref(self.dev), SERIAL) == 0, "open failed"
        self.h = None
        try:
            ver = C.create_string_buffer(255)
            lib.hydrasdr_version_string_read(self.dev, ver, 255)
            self.version = ver.value.decode()
            if "sweep" not in self.version:
                raise SystemExit(f"not a sweep firmware: {self.version}")
            assert lib.hydrasdr_set_samplerate(self.dev, FS_REAL // 2) == 0
            assert lib.hydrasdr_set_packing(self.dev, 0) == 0
            assert lib.hydrasdr_set_rf_port(self.dev, self.port) == 0
            assert lib.hydrasdr_set_linearity_gain(self.dev, self.gain) == 0
            assert lib.hydrasdr_set_sample_type(self.dev, 5) == 0          # RAW
            assert lib.hydrasdr_start_rx(self.dev, self.cb, None) == 0
            if self.native:
                assert lib.hydrasdr_sweep_supported(self.dev) == 1
                rc = lib.hydrasdr_sweep_start(self.dev, C.byref(self.sp))
                assert rc == 0, f"hydrasdr_sweep_start: {rc}"
            else:
                import usb.core
                self.h = usb.core.find(idVendor=0x38AF, idProduct=0x0001)
                time.sleep(0.05)
                self.h.ctrl_transfer(0x40, SWEEP_START, 0, 0, self.params)
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def blocks(self, seconds):
        t_end = time.monotonic() + seconds
        while time.monotonic() < t_end:
            try:
                buf = self.q.get(timeout=0.5)
            except queue.Empty:
                continue
            for w in buf.reshape(-1, BLOCK_WORDS):
                yield parse_block(w), w

    def __exit__(self, *exc):
        try:
            if self.native:
                self.lib.hydrasdr_sweep_stop(self.dev)
            elif self.h is not None:
                self.h.ctrl_transfer(0x40, SWEEP_STOP, 0, 0)
            self.lib.hydrasdr_stop_rx(self.dev)
        finally:
            if self.h is not None:
                import usb.util
                usb.util.dispose_resources(self.h)
            self.lib.hydrasdr_close(self.dev)


def run(a):
    step_hz = int(a.step * 1e6)
    n_steps = max(1, math.ceil((a.stop - a.start) * 1e6 / step_hz))
    f0 = int(a.start * 1e6 + step_hz / 2)
    sp = Spectrum(step_hz / 2, a.bin_khz * 1e3)
    csv = open(a.csv, "w") if a.csv else None
    st = dict(blocks=0, valid=0, bad=0, untagged=0, steps=0, gaps=0, sweeps=0)
    last = None                      # (sweep, step) of last completed step
    acc, acc_key, acc_n, fhz_prev = None, None, 0, 0
    first_t = last_t = None
    first_cnt = last_cnt = None
    step_cnts = []                   # device DMA count at each step's first block
    sweep_power = np.full((n_steps, len(sp.out_off)), np.nan, dtype=np.float32)
    locks = []
    order = Order(f0, step_hz, n_steps, a.vco_order)
    flags = FLAG_VCO_ORDER if a.vco_order else 0
    first_swp = last_swp = None
    with Sweeper(f0, step_hz, n_steps, a.skip, a.dwell, a.gain, a.port, a.lock_reads, a.lock_timeout, a.lib,
                 flags) as sw:
        print(f"firmware: {sw.version}; sweep control: "
              f"{'libhydrasdr ' + sw.lib_path if sw.native else 'pyusb fallback (stock libhydrasdr)'}")
        for tag, w in sw.blocks(a.seconds):
            st["blocks"] += 1
            if tag is None:
                if w[0] == 0xA55A:
                    st["bad"] += 1
                else:
                    st["untagged"] += 1
                continue
            step, blk, swp, cnt, fhz = tag
            st["valid"] += 1
            now = time.monotonic()
            first_t = first_t or now
            last_t = now
            first_cnt = cnt if first_cnt is None else first_cnt
            first_swp = swp if first_swp is None else first_swp
            last_swp = swp
            last_cnt = cnt
            key = (swp, step)
            if key != acc_key:
                if acc_key is not None:
                    flush(acc_key, acc / acc_n, acc_n, fhz_prev, sp, csv, sweep_power, n_steps, st)
                    if last is not None:
                        if acc_key != order.next(last):
                            st["gaps"] += 1
                    last = acc_key
                acc, acc_key, acc_n = None, key, 0
                step_cnts.append(cnt)
                locks.append(lock_info(w))
            p = sp.power(w)
            acc = p if acc is None else acc + p
            acc_n += 1
            fhz_prev = fhz
    if csv:
        csv.close()
    st["sweeps"] = (last_swp - first_swp) if first_swp is not None else 0   # completed passes

    host_s = (last_t - first_t) if first_t and last_t else 0
    dev_s = ((last_cnt - first_cnt) * BLOCK_S) if first_cnt is not None else 0
    d = np.diff(step_cnts) if len(step_cnts) > 1 else np.array([0])
    span = n_steps * step_hz
    print(f"plan: {n_steps} steps x {step_hz/1e6:g} MHz from {f0/1e6:.3f} MHz centre, skip {a.skip}, dwell {a.dwell}, order {'VCO-continuous' if a.vco_order else 'ascending'}")
    print(f"blocks: {st['blocks']} total, {st['valid']} valid, {st['bad']} torn (header/trailer mismatch), "
          f"{st['untagged']} untagged (pre-sweep)")
    print(f"steps completed: {st['steps']}, sequence gaps: {st['gaps']}, full sweeps: {st['sweeps']}")
    if locks and a.lock_reads:
        lp = np.array([p for p, _ in locks])
        print(f"PLL lock polls/step: median {np.median(lp):.0f}, p90 {np.percentile(lp, 90):.0f}, max {lp.max()}, "
              f"timeouts {sum(t for _, t in locks)}")
    if st["steps"] and dev_s > 0:
        hop = np.median(d) * BLOCK_S
        print(f"device clock: {dev_s:.2f} s for {st['steps']} steps -> {st['steps']/dev_s:.0f} steps/s, "
              f"median hop {hop*1e3:.3f} ms ({np.median(d):.0f} blocks), "
              f"{st['steps']/dev_s*step_hz/1e9:.2f} GHz/s, {st['steps']/dev_s/n_steps:.2f} sweeps/s of "
              f"{span/1e6:.0f} MHz")
        print(f"host clock:   {host_s:.2f} s ({st['steps']/max(host_s,1e-9):.0f} steps/s)")
    if a.peaks:
        top = []
        P = 10 * np.log10(sweep_power)
        for i in range(n_steps):
            if np.all(np.isnan(P[i])):
                continue
            j = np.nanargmax(P[i])
            top.append((P[i, j] - np.nanmedian(P[i]), f0 + i * step_hz + sp.out_off[j]))
        print("strongest peak per step (dB over step median), top 12:")
        for snr, f in sorted(top, reverse=True)[:12]:
            print(f"  {f/1e6:10.4f} MHz  +{snr:.1f} dB")
    if a.save:
        np.savez_compressed(a.save, f0=f0, step=step_hz, off=sp.out_off, power=sweep_power)
        print(f"last sweep saved to {a.save}")


def flush(key, p, n, fhz, sp, csv, sweep_power, n_steps, st):
    swp, step = key
    st["steps"] += 1
    sweep_power[step] = p
    if csv:
        now = datetime.now()
        lo = fhz + sp.out_off[0] - sp.bin_hz / 2
        hi = fhz + sp.out_off[-1] + sp.bin_hz / 2
        db = ", ".join(f"{v:.2f}" for v in 10 * np.log10(p + 1e-20))
        csv.write(f"{now:%Y-%m-%d}, {now:%H:%M:%S.%f}, {int(lo)}, {int(hi)}, {sp.bin_hz:.2f}, "
                  f"{n * (BLOCK_WORDS - 2 * TAG_WORDS)}, {db}\n")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--start", type=float, default=24.0, help="MHz")
    ap.add_argument("--stop", type=float, default=1800.0, help="MHz")
    ap.add_argument("--step", type=float, default=8.0, help="MHz between centres (<= ~9)")
    ap.add_argument("--skip", type=int, default=0, help="extra settle blocks after lock (409.6 us each)")
    ap.add_argument("--lock-reads", type=int, default=2, help="consecutive PLL-locked reads before capture (0=off)")
    ap.add_argument("--lock-timeout", type=int, default=200, help="max lock polls (~0.1 ms each)")
    ap.add_argument("--vco-order", action="store_true",
                    help="VCO-continuous visiting order (firmware sweep4+): ~16%% faster full-range passes")
    ap.add_argument("--dwell", type=int, default=1, help="blocks per step")
    ap.add_argument("--gain", type=int, default=10, help="linearity gain preset 0-21")
    ap.add_argument("--port", type=int, default=0, help="RF port (0=ANT)")
    ap.add_argument("--seconds", type=float, default=5.0)
    ap.add_argument("--bin-khz", type=float, default=10.0)
    ap.add_argument("--csv")
    ap.add_argument("--save", help="save the last full sweep as .npz")
    ap.add_argument("--peaks", action="store_true", help="print the strongest peaks of the last sweep")
    ap.add_argument("--lib", help="libhydrasdr to load (default: the Corvus build if present)")
    ap.add_argument("--serial", help="board serial in hex (default: env CORVUS_SERIAL, else the first RFOne)")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.serial:
        SERIAL = int(a.serial, 16)
    selftest() if a.selftest else run(a)
