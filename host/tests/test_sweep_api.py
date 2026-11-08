"""On-device checks of the libhydrasdr sweep API (the Corvus build, on an RFOne running the Corvus firmware).

Error paths (not streaming, non-RAW, packing on, bad plan), stop -> normal stream -> restart with a
new plan, and that a tuner request is STALLed during a sweep and works again after stop.
Usage: uv run --no-project --with numpy python host/tests/test_sweep_api.py [--serial HEX] [--lib PATH]
--serial defaults to env CORVUS_SERIAL, else the first RFOne found.
"""
import ctypes as C
import os
import sys
import time

import numpy as np

if "--serial" in sys.argv:
    os.environ["CORVUS_SERIAL"] = sys.argv[sys.argv.index("--serial") + 1]
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../tools"))
from corvus_sweep import BLOCK_WORDS, LIB, SERIAL, SweepBlockInfo, SweepParams, Transfer  # noqa: E402

lib = C.CDLL(sys.argv[sys.argv.index("--lib") + 1] if "--lib" in sys.argv else LIB)
V = C.c_void_p
for fn, args in {"hydrasdr_open_sn": [C.POINTER(V), C.c_uint64], "hydrasdr_close": [V],
                 "hydrasdr_set_samplerate": [V, C.c_uint32], "hydrasdr_set_packing": [V, C.c_uint8],
                 "hydrasdr_set_sample_type": [V, C.c_int], "hydrasdr_set_linearity_gain": [V, C.c_uint8],
                 "hydrasdr_set_freq": [V, C.c_uint64], "hydrasdr_stop_rx": [V],
                 "hydrasdr_sweep_supported": [V], "hydrasdr_sweep_stop": [V], "hydrasdr_sweep_fw_revision": [V],
                 "hydrasdr_sweep_start": [V, C.POINTER(SweepParams)],
                 "hydrasdr_sweep_parse_block": [C.POINTER(C.c_uint16), C.POINTER(SweepBlockInfo)]}.items():
    getattr(lib, fn).argtypes = args
CB = C.CFUNCTYPE(C.c_int, C.POINTER(Transfer))
lib.hydrasdr_start_rx.argtypes = [V, CB, V]

blocks = []


@CB
def on_rx(t):
    tr = t.contents
    a = np.ctypeslib.as_array(C.cast(tr.samples, C.POINTER(C.c_uint16)), shape=(tr.sample_count,))
    blocks.extend(a.reshape(-1, BLOCK_WORDS).copy())
    return 0


def classify(seconds):
    blocks.clear()
    time.sleep(seconds)
    out = {0: 0, 1: 0, 2: 0}
    freqs = set()
    info = SweepBlockInfo()
    for b in list(blocks):
        r = lib.hydrasdr_sweep_parse_block(b.ctypes.data_as(C.POINTER(C.c_uint16)), C.byref(info))
        out[r] += 1
        if r == 0:
            freqs.add(info.freq_hz)
    return out, freqs


fails = 0


def check(name, cond, detail=""):
    global fails
    fails += not cond
    print(f"{'PASS' if cond else 'FAIL'}  {name} {detail}")


dev = V()
assert lib.hydrasdr_open_sn(C.byref(dev), SERIAL) == 0
try:
    plan_a = SweepParams(100_000_000, 8_000_000, 4, 0, 1, 2, 0, 200)    # 100..124 MHz (slow band, ~190 hops/s)
    plan_b = SweepParams(500_000_000, 8_000_000, 3, 0, 1, 2, 0, 200)    # 500..516 MHz
    check("sweep_supported", lib.hydrasdr_sweep_supported(dev) == 1)
    lib.hydrasdr_set_samplerate(dev, 10_000_000)
    lib.hydrasdr_set_linearity_gain(dev, 10)
    lib.hydrasdr_set_packing(dev, 0)
    lib.hydrasdr_set_sample_type(dev, 5)
    check("start while not streaming -> STREAMING_STOPPED", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == -1003)
    bad = SweepParams(100_000_000, 8_000_000, 0, 0, 1, 2, 0, 200)
    check("n_steps 0 -> INVALID_PARAM", lib.hydrasdr_sweep_start(dev, C.byref(bad)) == -2)
    huge = SweepParams(4_000_000_000, 100_000_000, 10, 0, 1, 2, 0, 200)
    check("u32 overflow plan -> INVALID_PARAM", lib.hydrasdr_sweep_start(dev, C.byref(huge)) == -2)
    lib.hydrasdr_set_sample_type(dev, 2)                                 # INT16_IQ
    check("non-RAW sample type -> INVALID_PARAM", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == -2)
    lib.hydrasdr_set_sample_type(dev, 5)
    lib.hydrasdr_set_packing(dev, 1)
    check("packing on -> INVALID_PARAM", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == -2)
    lib.hydrasdr_set_packing(dev, 0)

    assert lib.hydrasdr_start_rx(dev, on_rx, None) == 0
    c, _ = classify(0.3)
    check("normal stream is untagged", c[0] == 0 and c[1] > 100, str(c))
    check("start plan A", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == 0)
    c, f = classify(0.5)
    check("plan A blocks valid, 4 centres", c[0] > 30 and c[2] == 0 and f == {100e6, 108e6, 116e6, 124e6},
          f"{c} {sorted(x / 1e6 for x in f)}")
    check("set_freq STALLed during sweep", lib.hydrasdr_set_freq(dev, 200_000_000) != 0)
    check("restart with plan B (no stop)", lib.hydrasdr_sweep_start(dev, C.byref(plan_b)) == 0)
    time.sleep(0.05)
    c, f = classify(0.5)
    check("plan B blocks valid, 3 centres", c[0] > 100 and c[2] == 0 and f == {500e6, 508e6, 516e6},
          f"{c} {sorted(x / 1e6 for x in f)}")
    check("sweep_stop", lib.hydrasdr_sweep_stop(dev) == 0)
    time.sleep(0.05)
    c, _ = classify(0.3)
    check("after stop: stream continues untagged", c[0] == 0 and c[1] > 100, str(c))
    check("set_freq works again after stop", lib.hydrasdr_set_freq(dev, 200_000_000) == 0)
    check("start plan A again", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == 0)
    c, f = classify(0.3)
    check("plan A again valid", c[0] > 20 and f == {100e6, 108e6, 116e6, 124e6}, str(c))
    vco = SweepParams(28_000_000, 8_000_000, 222, 0, 1, 2, 0x01, 200)     # full range, VCO order
    unknown = SweepParams(28_000_000, 8_000_000, 222, 0, 1, 2, 0x80, 200)
    check("unknown flag -> UNSUPPORTED", lib.hydrasdr_sweep_start(dev, C.byref(unknown)) == -12)
    rev = lib.hydrasdr_sweep_fw_revision(dev)
    check(f"VCO order start (fw sweep{rev})", lib.hydrasdr_sweep_start(dev, C.byref(vco)) == (0 if rev >= 4 else -12))
    if rev >= 4:
        c, f = classify(1.0)
        check("VCO order: valid blocks, grid freqs only", c[0] > 400 and c[2] == 0 and
              all((x - 28e6) % 8e6 == 0 and 28e6 <= x <= 1796e6 for x in f), str(c))
        check("VCO order: whole grid within ~1 s", len(f) == 222, f"{len(f)} centres")
    lib.hydrasdr_sweep_stop(dev)
    lib.hydrasdr_stop_rx(dev)
    check("stop_rx ends the sweep (start now refused)", lib.hydrasdr_sweep_start(dev, C.byref(plan_a)) == -1003)
finally:
    lib.hydrasdr_stop_rx(dev)
    lib.hydrasdr_close(dev)
print(f"{'ALL PASS' if not fails else f'{fails} FAILED'}")
sys.exit(1 if fails else 0)
