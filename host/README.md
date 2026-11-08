# Host libraries: libhydrasdr and libairspy with the sweep API

Two libraries, one API: libhydrasdr for the RFOne (below) and libairspy for the Airspy R2 (at the
end). Both add the same five functions under their own prefix, with the same parameters and tags.

`hydrasdr-host/` is an import of the upstream host library and tools (libhydrasdr 1.1.4,
BSD-3-Clause, see `hydrasdr-host/libhydrasdr/LICENSE.md`) with five functions added for the Corvus
sweep firmware. `git log -- host/hydrasdr-host` shows the unmodified import first, then the change.

It's a strict superset: existing callers and the stock `hydrasdr_*` tools behave the same with it,
and the version still reports 1.1.4. Detect the feature with `HYDRASDR_FEATURE_SWEEP` at compile time,
or by the symbols at load time.

| Function | |
|---|---|
| `hydrasdr_sweep_supported(dev)` | `HYDRASDR_TRUE` if the firmware version contains `-sweep` |
| `hydrasdr_sweep_fw_revision(dev)` | N from `-sweepN` (0 = no sweep mode) |
| `hydrasdr_sweep_start(dev, &params)` | sends the plan (request 0x70). Refuses with `INVALID_PARAM` unless the stream is RAW and unpacked and the plan is sane, with `STREAMING_STOPPED` unless streaming, and with `UNSUPPORTED` on stock firmware. Calling it again replaces the plan without a stop. `params.flags = HYDRASDR_SWEEP_FLAG_VCO_ORDER` (sweep4+, refused with `UNSUPPORTED` on older firmware) visits the steps in VCO-continuous order |
| `hydrasdr_sweep_stop(dev)` | request 0x71. The stream continues in normal mode. `hydrasdr_stop_rx()` also ends a sweep |
| `hydrasdr_sweep_parse_block(block, &info)` | checks one 8192-sample RAW block. Returns `VALID`, `UNTAGGED` or `TORN`, and fills in step, block, sweep, frequency, DMA count and lock info |
| (on open) | `hydrasdr_open*` sends SWEEP_STOP to sweep firmware before device init, ending a sweep left behind by a host that died mid-sweep |

`hydrasdr_sweep_params_t` and the block format are documented in `hydrasdr.h` (the `sweep_api`
group) and in `../firmware/README.md`. Use sweep3 or later: on sweep2, restarting a sweep can
mislabel blocks.

A client's sequence: open, set the sample type to RAW and packing off, set the rate, set the RF port
and gain, start streaming, then `hydrasdr_sweep_start`. In the RX callback, pass each block to
`hydrasdr_sweep_parse_block` and drop anything that isn't `VALID`. To change gain or port, stop the
sweep, change it, and start again. `../app/src/hydra.rs` does all of this.

## Sample rates in sweep mode

Sweeps run at the 10 or 12 MSPS setting. Set the sample type to RAW first, then the rate. Under RAW, a
rate given by value means the **real ADC rate**, not the IQ rate. This is upstream behaviour.

| Setting | `hydrasdr_set_samplerate()` under RAW | ADC (real) | IF | Nyquist band |
|---|---|---|---|---|
| 10 MSPS | `20000000` (`10000000` also works) | 20 MHz | 5 MHz | 0–10 MHz |
| 12 MSPS | `24000000` | 24 MHz | 6 MHz | 0–12 MHz |

- `10000000` works under RAW only because it matches the IQ rate table, which ignores the sample type.
- `12000000` under RAW selects the 6 MSPS IQ setting (12 MHz ADC) and still returns success.

The firmware tunes with the IF of the current setting, so `freq_hz` in the tags is the RF centre at
either rate. `HYDRASDR_SWEEP_ADC_RATE_HZ` and `HYDRASDR_SWEEP_IF_HZ` are the 10 MSPS values; at 12 MSPS
use 24 MHz and 6 MHz.

## Building

```
host/build/build.sh [linux|windows|all]
```

- Linux: `ubuntu:24.04` with the same dependencies as upstream CI. Output in `build/out/linux/`:
  `libhydrasdr.so.1.1.4`, the tools, `hydrasdr.h` and `hydrasdr_commands.h`.
- Windows x64: `fedora:42` with MinGW-w64 and Fedora's libusb. Upstream builds Windows with MSVC.
  Output in `build/out/windows/`: `hydrasdr.dll` (the name apps load; `libhydrasdr.dll` is the
  same file under the name the bundled tools import), `libusb-1.0.dll`, the import library, headers
  and tools. The DLLs import only `KERNEL32`, `msvcrt` and `libusb-1.0.dll`.

## Using it

**Linux.** Point the loader at the build, so a system-wide libhydrasdr stays untouched:

```
LD_LIBRARY_PATH=host/build/out/linux <app>
```

or dlopen `host/build/out/linux/libhydrasdr.so.1.1.4` directly, as Corvus Sweep does. Replacing the
system library would work for every app, but a package update would overwrite it.

**Windows.** Put `hydrasdr.dll` and `libusb-1.0.dll` next to the application. No driver install is
needed: the firmware has Microsoft OS (WCID) descriptors, so Windows binds WinUSB itself. Check
with `hydrasdr_info.exe` that the version line ends in `-sweep5`. The Windows build has had its exports
checked but **hasn't been tested on hardware yet**.

## Tests

On-device tests, which need a board running the Corvus firmware. Pass its serial, since some of
these restart the stream:

```
uv run --no-project python host/tests/test_sweep_api.py --serial <hex>
uv run --no-project python host/tests/test_stale_sweep.py --serial <hex>
```

- `test_sweep_api.py`: the API's error paths, stop and restart, VCO-order coverage, and the STALL
  behaviour mid-sweep.
- `test_stale_sweep.py`: SIGKILLs a sweeping child process, then opens the board with the system
  library and with this one. On sweep5 both open.

## libairspy (Airspy R2)

`airspyone-host/` is an import of the upstream Airspy host library and tools (libairspy 1.0.10,
BSD-3-Clause) with the same API added under the `airspy_` prefix: `airspy_sweep_supported`,
`airspy_sweep_fw_revision`, `airspy_sweep_start`, `airspy_sweep_stop`, `airspy_sweep_parse_block`,
`airspy_sweep_params_t` and `airspy_sweep_block_info_t` (same layouts as the libhydrasdr ones), plus
`AIRSPY_ERROR_UNSUPPORTED` (-12). `airspy_open*` sends SWEEP_STOP to sweep firmware first. Everything
else is unchanged and the version still reports 1.0.10.

- Build: `host/build/build.sh airspy-linux` -> `build/out/airspy-linux/` (`libairspy.so.1.0.10`, tools, headers).
- Rate: the R2 sweeps at its 10 MSPS setting. Under RAW, `airspy_set_samplerate(10000000)` (or index 0)
  selects it: ADC 20 MHz real, IF 5 MHz.
- `git log -- host/airspyone-host` shows the unmodified import first, then the change.
- Test: `uv run --no-project --with numpy python host/tests/test_airspy_sweep_api.py --serial <hex>` (error
  paths, tuner requests refused mid-sweep, stop and restart, 40 rapid plan switches with every block on its
  plan's grid). Passes on an R2 with the Corvus firmware.

