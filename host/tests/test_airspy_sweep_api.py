"""On-device checks of the libairspy sweep API (the Corvus build, on an Airspy R2 running the Corvus firmware).

Error paths (not streaming, non-RAW, packing on, bad plan), tuner requests refused mid-sweep and accepted
after stop, stop -> normal stream -> restart with a new plan, and every valid block's frequency on its
plan's grid across restarts.
Usage: uv run --no-project --with numpy python host/tests/test_airspy_sweep_api.py [--serial HEX] [--lib PATH] [--msps 10|6]
--msps: the board's rate setting (R2 10, Mini 6; default 10).
--serial defaults to env CORVUS_SERIAL, else the first Airspy found.
"""
import ctypes as C
import os
import sys
import time

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
LIB = sys.argv[sys.argv.index("--lib") + 1] if "--lib" in sys.argv else os.path.join(HERE, "../build/out/airspy-linux/libairspy.so.1.0.10")
MSPS = int(sys.argv[sys.argv.index("--msps") + 1]) if "--msps" in sys.argv else 10
SERIAL = int(sys.argv[sys.argv.index("--serial") + 1] if "--serial" in sys.argv else os.environ.get("CORVUS_SERIAL", "0"), 16)
BLOCK = 8192
RAW, INT16_IQ = 5, 2
OK, TRUE, INVALID, STOPPED, UNSUPPORTED = 0, 1, -2, -1003, -12


class Params(C.Structure):
    _fields_ = [("f_start_hz", C.c_uint32), ("f_step_hz", C.c_uint32), ("n_steps", C.c_uint16), ("skip_blocks", C.c_uint8),
                ("dwell_blocks", C.c_uint8), ("lock_reads", C.c_uint8), ("flags", C.c_uint8), ("lock_timeout", C.c_uint16)]


class Info(C.Structure):
    _fields_ = [("freq_hz", C.c_uint32), ("sweep", C.c_uint32), ("dma_count", C.c_uint32), ("step", C.c_uint16),
                ("block", C.c_uint16), ("lock_polls", C.c_uint16), ("lock_timed_out", C.c_uint8), ("reserved", C.c_uint8)]


class Transfer(C.Structure):
    _fields_ = [("device", C.c_void_p), ("ctx", C.c_void_p), ("samples", C.c_void_p), ("sample_count", C.c_int),
                ("dropped_samples", C.c_uint64), ("sample_type", C.c_int)]


lib = C.CDLL(LIB)
V = C.c_void_p
for fn, args in {"airspy_open_sn": [C.POINTER(V), C.c_uint64], "airspy_close": [V], "airspy_set_samplerate": [V, C.c_uint32],
                 "airspy_set_packing": [V, C.c_uint8], "airspy_set_sample_type": [V, C.c_int], "airspy_set_linearity_gain": [V, C.c_uint8],
                 "airspy_set_freq": [V, C.c_uint32], "airspy_stop_rx": [V], "airspy_sweep_supported": [V], "airspy_sweep_stop": [V],
                 "airspy_sweep_fw_revision": [V], "airspy_sweep_start": [V, C.POINTER(Params)],
                 "airspy_sweep_parse_block": [C.POINTER(C.c_uint16), C.POINTER(Info)]}.items():
    getattr(lib, fn).argtypes = args
CB = C.CFUNCTYPE(C.c_int, C.POINTER(Transfer))
lib.airspy_start_rx.argtypes = [V, CB, V]
blocks = []


@CB
def on_rx(t):
    tr = t.contents
    a = np.ctypeslib.as_array(C.cast(tr.samples, C.POINTER(C.c_uint16)), shape=(tr.sample_count,))
    blocks.extend(a.reshape(-1, BLOCK).copy())
    return 0


fails = 0


def check(name, got, want):
    global fails
    ok = got == want if not callable(want) else want(got)
    fails += not ok
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {got}")


def collect(seconds, *plans):
    """Valid/torn/untagged counts, and valid blocks whose frequency is on none of the plans' grids at their step."""
    blocks.clear()
    time.sleep(seconds)
    out = {0: 0, 1: 0, 2: 0}
    off = 0
    info = Info()
    for b in list(blocks):
        r = lib.airspy_sweep_parse_block(b.ctypes.data_as(C.POINTER(C.c_uint16)), C.byref(info))
        out[r] += 1
        if r == 0 and not any(info.step < q.n_steps and info.freq_hz == q.f_start_hz + info.step * q.f_step_hz for q in plans):
            off += 1
    return out, off


dev = V()
assert lib.airspy_open_sn(C.byref(dev), SERIAL) == 0, "open failed"
try:
    check("sweep_supported", lib.airspy_sweep_supported(dev), TRUE)
    check("sweep_fw_revision", lib.airspy_sweep_fw_revision(dev), 5)
    full = Params(28_000_000, 8_000_000, 222, 0, 1, 2, 1, 200)
    narrow = Params(400_000_000, 8_000_000, 10, 0, 2, 2, 0, 200)
    lib.airspy_set_sample_type(dev, RAW)
    lib.airspy_set_samplerate(dev, MSPS * 1_000_000)
    lib.airspy_set_packing(dev, 0)
    lib.airspy_set_linearity_gain(dev, 10)
    check("start while not streaming", lib.airspy_sweep_start(dev, C.byref(full)), STOPPED)
    check("bad plan (0 steps)", lib.airspy_sweep_start(dev, C.byref(Params(28_000_000, 8_000_000, 0, 0, 1, 2, 0, 200))), INVALID)
    check("plan past 4.29 GHz", lib.airspy_sweep_start(dev, C.byref(Params(4_000_000_000, 8_000_000, 100, 0, 1, 2, 0, 200))), INVALID)
    check("unknown flag", lib.airspy_sweep_start(dev, C.byref(Params(28_000_000, 8_000_000, 10, 0, 1, 2, 0x80, 200))), lambda r: r in (UNSUPPORTED, STOPPED))
    lib.airspy_set_sample_type(dev, INT16_IQ)
    check("start with IQ samples", lib.airspy_sweep_start(dev, C.byref(full)), INVALID)
    lib.airspy_set_sample_type(dev, RAW)
    lib.airspy_set_packing(dev, 1)
    check("start with packing on", lib.airspy_sweep_start(dev, C.byref(full)), INVALID)
    lib.airspy_set_packing(dev, 0)

    check("start_rx", lib.airspy_start_rx(dev, on_rx, None), OK)
    check("sweep_start full range, VCO order", lib.airspy_sweep_start(dev, C.byref(full)), OK)
    counts, off = collect(2.0, full)
    check("full range: valid blocks", counts[0], lambda n: n > 1000)
    check("full range: torn / off-grid", (counts[2], off), (0, 0))
    check("set_freq mid-sweep is refused", lib.airspy_set_freq(dev, 100_000_000), lambda r: r != OK)
    check("sweep_stop", lib.airspy_sweep_stop(dev), OK)
    time.sleep(0.2)
    counts, _ = collect(0.5, full)
    check("after stop: normal untagged stream", counts[1] > 0 and counts[0] == 0, True)
    check("set_freq after stop works", lib.airspy_set_freq(dev, 100_000_000), OK)

    restarts, off_total, torn_total = 0, 0, 0
    for i in range(40):
        p = narrow if i % 2 else full
        lib.airspy_sweep_start(dev, C.byref(p))
        restarts += 1
        counts, off = collect(0.05 + 0.02 * (i % 4), full, narrow)  # the previous plan's blocks may still arrive
        torn_total += counts[2]
        off_total += off
    check(f"{restarts} restarts alternating plans: torn, off both grids", (torn_total, off_total), (0, 0))
    lib.airspy_sweep_start(dev, C.byref(narrow))
    time.sleep(0.2)
    counts, off = collect(1.0, narrow)
    check("narrow plan after restarts: valid, off-grid", (counts[0] > 100, off), (True, 0))
    lib.airspy_sweep_stop(dev)
    lib.airspy_stop_rx(dev)
finally:
    lib.airspy_close(dev)
print("ALL PASS" if not fails else f"{fails} FAILED")
sys.exit(1 if fails else 0)
