"""A host killed mid-sweep must not strand the RFOne: after SIGKILL, can a fresh process open it?

For each libhydrasdr given (default: the system lib, then the Corvus build), strand the board
(child process starts a sweep, then is SIGKILLed), then try hydrasdr_open_sn with that lib.
If the open fails the board is recovered with a raw SWEEP_STOP (pyusb), so later cases start clean.
Firmware sweep5+ should pass with any lib (SET_PACKING during open ends the sweep);
the Corvus lib should pass on any sweep firmware (it sends SWEEP_STOP on open).
Usage: uv run --no-project --with numpy --with pyusb python host/tests/test_stale_sweep.py [--serial HEX] [lib ...]
--serial defaults to env CORVUS_SERIAL, else the first RFOne found.
"""
import ctypes as C
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PATCHED = os.path.join(HERE, "../build/out/linux/libhydrasdr.so.1.1.4")
STOCK = "libhydrasdr.so.1"  # whatever the system loader finds
ARGS = sys.argv[1:]
if "--serial" in ARGS:
    i = ARGS.index("--serial")
    os.environ["CORVUS_SERIAL"] = ARGS[i + 1]
    del ARGS[i:i + 2]
SERIAL = int(os.environ.get("CORVUS_SERIAL", "0"), 16)  # 0 = the first RFOne found
CHILD = f"""
import sys, time
sys.path.insert(0, {os.path.join(HERE, '../../tools')!r})
from corvus_sweep import Sweeper
sw = Sweeper(28_000_000, 8_000_000, 222, lib={PATCHED!r}).__enter__()
print("sweeping", flush=True)
time.sleep(30)
"""


def strand():
    p = subprocess.Popen([sys.executable, "-c", CHILD], stdout=subprocess.PIPE, text=True)
    assert p.stdout.readline().strip() == "sweeping"
    time.sleep(0.5)
    p.send_signal(signal.SIGKILL)
    p.wait()
    time.sleep(0.3)


def try_open(libpath):
    lib = C.CDLL(libpath)
    lib.hydrasdr_open_sn.argtypes = [C.POINTER(C.c_void_p), C.c_uint64]
    lib.hydrasdr_close.argtypes = [C.c_void_p]
    lib.hydrasdr_version_string_read.argtypes = [C.c_void_p, C.c_char_p, C.c_uint8]
    dev = C.c_void_p()
    rc = lib.hydrasdr_open_sn(C.byref(dev), SERIAL)
    ver = ""
    if rc == 0:
        b = C.create_string_buffer(255)
        lib.hydrasdr_version_string_read(dev, b, 255)
        ver = b.value.decode()
        lib.hydrasdr_close(dev)
    return rc, ver


def recover():
    import usb.core
    import usb.util
    h = usb.core.find(idVendor=0x38AF, idProduct=0x0001)
    h.ctrl_transfer(0x40, 0x71, 0, 0)
    usb.util.dispose_resources(h)


fails = 0
for libpath in (ARGS or [STOCK, PATCHED]):
    # each case in a fresh process, so the two libhydrasdr builds never share one address space
    if os.environ.get("STALE_CASE") is None:
        r = subprocess.run([sys.executable, __file__], env={**os.environ, "STALE_CASE": libpath}, text=True,
                           capture_output=True)
        print(r.stdout, end="")
        fails += r.returncode != 0
        continue
for_case = os.environ.get("STALE_CASE")
if for_case:
    strand()
    rc, ver = try_open(for_case)
    ok = rc == 0
    print(f"{'PASS' if ok else 'FAIL'}  open after SIGKILL mid-sweep with {for_case}: rc {rc} {ver}")
    if not ok:
        recover()
        print(f"      recovered with raw SWEEP_STOP: open rc {try_open(for_case)[0]}")
    sys.exit(0 if ok else 1)
print("ALL PASS" if not fails else f"{fails} FAILED")
sys.exit(1 if fails else 0)
