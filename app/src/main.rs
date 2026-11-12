//! Corvus Sweep: a live, seamlessly stitched spectrum for an RFOne or Airspy R2 running the Corvus
//! sweep firmware ("...-sweepN"), through the board's library with the Corvus sweep API.

// A window, not a console program, on Windows (started from a terminal, it reattaches to it: see main).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod dsp;
mod engine;
mod hackrf;
mod health;
mod hydra;
mod net;
mod plan;
mod pluto;
mod sim;
mod theme;
mod usbdev;
mod view3d;
mod waterfall;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set by SIGINT/SIGTERM/SIGHUP; the window and the headless loop poll it.
pub static QUIT: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    QUIT.store(true, Ordering::Relaxed);
}

/// On Windows the app is a GUI program, so it has no console of its own; when started from one (cmd,
/// PowerShell) it attaches to it, so --help, --headless and the log still print there.
#[cfg(windows)]
fn attach_parent_console() {
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    // SAFETY: plain Win32 call; ATTACH_PARENT_PROCESS = (DWORD)-1. Fails harmlessly with no parent console.
    unsafe { AttachConsole(u32::MAX) };
}

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

/// Ctrl-C / kill: let the app close the device properly; if it can't within 1.5 s (e.g. the window
/// is not being drawn), stop the sweep directly so the board isn't left sweeping.
fn install_signal_handlers() {
    unsafe {
        for sig in [1, 2, 15] {
            signal(sig, on_signal);
        }
    }
    std::thread::spawn(|| {
        while !QUIT.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for _ in 0..30 {
            if DONE.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("corvus-sweep: no clean shutdown, stopping the sweep directly");
        hydra::emergency_stop();
        std::process::exit(130);
    });
}

const USAGE: &str = "\
corvus-sweep [options]

  --board rfone|airspy|mini|hackrf|pluto  which board (default: env CORVUS_BOARD, else the
                        first one plugged in, else rfone):
                        airspy is the Airspy R2 (10 MSPS), mini the Airspy Mini (6 MSPS),
                        hackrf a HackRF One (stock firmware sweep, 1 MHz to 6 GHz),
                        pluto a PlutoSDR or Pluto+ running corvus-pluto (~18 MHz to 6.18 GHz)
  --pluto HOST[:PORT]   the Pluto's address (implies --board pluto; default: env
                        CORVUS_PLUTO, else 192.168.2.1, its USB network link; port 30433)
  --3d                  start with the 3D waterfall
  --host-sweep          RFOne/Airspy: let the PC retune each step even on Corvus firmware
                        (what stock firmware gets automatically; much slower)
  --usb native|lib      how to talk to the board: the built-in USB driver (native, the
                        default: no library needed) or its C library (lib: libhydrasdr,
                        libairspy or libhackrf, see --lib). Env CORVUS_USB
  --sim                 run the built-in simulator instead of the board
  --lib PATH            with --usb lib: the board's library (default: env CORVUS_LIB,
                        then one next to this program, then the repo's host/build/out
                        build, then the system loader)
  --serial HEX          open the board with this serial (default: env CORVUS_SERIAL,
                        else the first one found)
  --range START:STOP    sweep range in MHz, e.g. 88:108
  --order vco|asc       visiting order (vco needs firmware sweep4+)
  --port NAME           RF input: ANT, CABLE1, CABLE2 (RFOne) or RX1, RX2 (Pluto)
  --looks 1|2           the Pluto: every frequency seen from one LO or two (default 2: no images, half the speed)
  --gain N              gain 0-21 in linearity mode (the Pluto: 0-71 dB, Gain 10 is 34 dB)
  --fresh               ignore saved settings
  --headless SECS       no window: sweep for SECS, print rates and seam ripple, then exit
  --csv PATH            with --headless: write the stitched spectrum as CSV
  --csv-every SECS      with --csv: also a snapshot every SECS (PATH-1.csv, PATH-2.csv, ...)
  --smooth SECS         live trace smoothing time (default 1; 0 = off): noise averaged, bursts pass
  --bin-khz K           bin width: 2.4, 4.9, 9.8, 19.5 or 39 kHz at 10 MSPS (x1.2 at 12)
  --msps 10|12|61|31|15|7|3  sample rate (default 12: 24 MSPS real, wider steps; the Pluto:
                        61 = 61.44 MSPS, the default, 31 = 30.72, or 15.36, 7.68, 3.84 for finer bins)
  --step MHZ            step size (default 10 at 12 MSPS, 8 at 10 MSPS, 40 at 61.44)
  --profile NAME        scan profile: fastest, quality, weak or bursts
                        (sets rate, step, dwell, lock reads, settle; later options override it)
  --serve [ADDR][:PORT] no window: share the radio on the network until stopped (default all
                        interfaces, port 30434), sweeping all along; another corvus-sweep connects
                        with --connect or from its radio picker. Prints the PIN
  --pin PIN             with --serve or --share: the PIN clients need (default: a random one);
                        with --connect: the server's PIN
  --share               open the window and share its radio too (the picker's Share this radio)
  --connect HOST[:PORT] drive the radio of a corvus-sweep serving on HOST (needs --pin)
  --screenshot PATH     save a PNG of the window after --after seconds, then exit
  --after SECS          (default 4)
";

fn main() -> eframe::Result {
    #[cfg(windows)]
    attach_parent_console();
    install_signal_handlers();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,eframe=warn,egui_wgpu=warn")).init();
    let mut opts = app::Options { sim: false, lib: PathBuf::new(), serial: hydra::default_serial(), screenshot: None, overrides: Vec::new(), connect: None, share: None };
    let mut serve: Option<std::net::SocketAddr> = None;
    let mut pin: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut share = false;
    let env_board = hydra::env_board();
    let mut board = env_board.unwrap_or(hydra::Board::RFOne);
    let mut board_given = env_board.is_some();
    match std::env::var("CORVUS_USB").as_deref() {
        Ok("lib") => hydra::set_usb_mode(hydra::UsbMode::Lib),
        Ok("native") => hydra::set_usb_mode(hydra::UsbMode::Native),
        _ => {}
    }
    let mut lib: Option<PathBuf> = None;
    let mut fresh = false;
    let mut after = 4.0f32;
    let mut shot: Option<PathBuf> = None;
    let mut headless: Option<f32> = None;
    let mut csv: Option<PathBuf> = None;
    let mut csv_every: Option<f32> = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        if a == "--serve" {
            // an optional value: ADDR, :PORT, ADDR:PORT or PORT
            let v = args.next_if(|v| !v.starts_with("--")).unwrap_or_default();
            serve = Some(parse_bind(&v).unwrap_or_else(|| die("--serve [ADDR][:PORT]")));
            continue;
        }
        let mut val = |name: &str| args.next().unwrap_or_else(|| die(&format!("{name} needs a value")));
        match a.as_str() {
            "--sim" => {
                opts.sim = true;
                // the simulated Pluto tunes like the Pluto+ corvus-pluto 0.2.1 reported
                plan::set_pluto_lo(46e6, 6150e6, true);
            }
            "--host-sweep" => hydra::set_force_host_sweep(true),
            "--usb" => hydra::set_usb_mode(match val("--usb").as_str() {
                "lib" => hydra::UsbMode::Lib,
                "native" => hydra::UsbMode::Native,
                _ => die("--usb native|lib"),
            }),
            "--3d" => opts.overrides.push(Box::new(|c| c.wf3d = true)),
            "--lib" => lib = Some(PathBuf::from(val("--lib"))),
            "--board" => {
                board = hydra::Board::parse(&val("--board")).unwrap_or_else(|| die("--board rfone|airspy|mini|hackrf|pluto"));
                board_given = true;
            }
            "--pluto" => {
                pluto::set_addr(&val("--pluto"));
                board = hydra::Board::Pluto;
                board_given = true;
            }
            "--serial" => {
                let v = val("--serial");
                opts.serial = u64::from_str_radix(v.trim_start_matches("0x").trim_start_matches("0X"), 16).unwrap_or_else(|_| die("bad --serial"));
            }
            "--range" => {
                let v = val("--range");
                let (a, b) = v.split_once(':').unwrap_or_else(|| die("--range START:STOP"));
                let (a, b): (f64, f64) = (a.parse().unwrap_or_else(|_| die("bad --range")), b.parse().unwrap_or_else(|_| die("bad --range")));
                opts.overrides.push(Box::new(move |c| {
                    c.start_mhz = a;
                    c.stop_mhz = b;
                }));
            }
            "--order" => {
                let o = match val("--order").as_str() {
                    "vco" => plan::Order::VcoContinuous,
                    "asc" => plan::Order::Ascending,
                    _ => die("--order vco|asc"),
                };
                opts.overrides.push(Box::new(move |c| c.order = o));
            }
            "--fresh" => fresh = true,
            "--headless" => headless = Some(val("--headless").parse().unwrap_or_else(|_| die("bad --headless"))),
            "--csv" => csv = Some(PathBuf::from(val("--csv"))),
            "--csv-every" => csv_every = Some(val("--csv-every").parse().ok().filter(|&s: &f32| s > 0.0).unwrap_or_else(|| die("bad --csv-every"))),
            "--smooth" => {
                let v: f32 = val("--smooth").parse().ok().filter(|&s: &f32| s >= 0.0).unwrap_or_else(|| die("--smooth SECS (0 = off)"));
                opts.overrides.push(Box::new(move |c| {
                    c.smooth = v > 0.0;
                    if v > 0.0 {
                        c.smooth_secs = v;
                    }
                }));
            }
            "--bin-khz" => {
                let k: f64 = val("--bin-khz").parse().unwrap_or_else(|_| die("bad --bin-khz"));
                opts.overrides.push(Box::new(move |c| {
                    let bin = |n: usize| (dsp::bin_hz(n, c.rate) / 1e3 - k).abs();
                    c.nfft = dsp::NFFTS.into_iter().min_by(|a, b| bin(*a).total_cmp(&bin(*b))).unwrap();
                }));
            }
            "--msps" => {
                let r = match val("--msps").as_str() {
                    "10" => plan::Rate::M10,
                    "12" => plan::Rate::M12,
                    "6" => plan::Rate::M6,
                    "61" | "61.44" => plan::Rate::P61,
                    "31" | "30.72" => plan::Rate::P31,
                    "15" | "15.36" => plan::Rate::P15,
                    "7" | "7.68" => plan::Rate::P7,
                    "3" | "3.84" => plan::Rate::P3,
                    _ => die("--msps 10|12|6|61|31|15|7|3"),
                };
                opts.overrides.push(Box::new(move |c| c.set_rate(r)));
            }
            "--step" => {
                let v: f64 = val("--step").parse().unwrap_or_else(|_| die("bad --step"));
                opts.overrides.push(Box::new(move |c| c.step_mhz = v));
            }
            "--port" => {
                let p: u8 = match val("--port").to_ascii_uppercase().as_str() {
                    "ANT" | "RX1" | "0" => 0,
                    "CABLE1" | "RX2" | "1" => 1,
                    "CABLE2" | "2" => 2,
                    _ => die("--port ANT|CABLE1|CABLE2|RX1|RX2"),
                };
                opts.overrides.push(Box::new(move |c| c.port = p));
            }
            "--looks" => {
                let two = match val("--looks").as_str() {
                    "1" => false,
                    "2" => true,
                    _ => die("--looks 1|2"),
                };
                opts.overrides.push(Box::new(move |c| c.looks2 = two));
            }
            "--gain" => {
                let g: u8 = val("--gain").parse().ok().filter(|&g| g <= 21).unwrap_or_else(|| die("--gain 0-21"));
                opts.overrides.push(Box::new(move |c| c.gain = hydra::Gain::Linearity(g)));
            }
            "--profile" => {
                let v = val("--profile");
                let pr = app::Profile::by_name(&v).unwrap_or_else(|| die("--profile fastest|quality|weak|bursts"));
                opts.overrides.push(Box::new(move |c| pr.apply(c)));
            }
            "--pin" => pin = Some(val("--pin")).filter(|p| !p.is_empty()).or_else(|| die("--pin PIN")),
            "--connect" => connect = Some(val("--connect")),
            "--share" => share = true,
            "--screenshot" => shot = Some(PathBuf::from(val("--screenshot"))),
            "--after" => after = val("--after").parse().unwrap_or_else(|_| die("bad --after")),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ => die(&format!("unknown option {a}")),
        }
    }
    if connect.is_some() && (serve.is_some() || share) {
        die("--connect drives another machine's radio; it can't be shared from here");
    }
    if let Some(addr) = connect {
        opts.connect = Some((addr, pin.clone().unwrap_or_else(|| die("--connect needs --pin"))));
    }
    if share {
        opts.share = Some(pin.clone());
    }
    // no --board or CORVUS_BOARD: whichever supported board is plugged in (the one with --serial, if given)
    if !board_given && !opts.sim {
        let found = hydra::list_devices();
        if let Some(f) = found.iter().find(|f| opts.serial == 0 || f.serial == opts.serial) {
            board = f.board;
        }
    }
    hydra::set_board(board);
    opts.lib = lib.unwrap_or_else(|| hydra::default_lib_path(board));
    if fresh {
        opts.overrides.insert(0, Box::new(|c| *c = app::Config::default()));
    }
    opts.screenshot = shot.map(|p| (p, after));
    if let Some(bind) = serve {
        let r = app::serve(opts, bind, pin);
        DONE.store(true, Ordering::Relaxed);
        if let Err(e) = r {
            eprintln!("corvus-sweep: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(secs) = headless {
        let r = app::headless(opts, secs, csv, csv_every);
        DONE.store(true, Ordering::Relaxed);
        if let Err(e) = r {
            eprintln!("corvus-sweep: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let viewport = egui::ViewportBuilder::default()
        .with_title("Corvus Sweep")
        .with_app_id("corvus-sweep")
        .with_inner_size([1720.0, 1040.0])
        .with_min_inner_size([1100.0, 700.0]);
    // a depth buffer for the 3D waterfall (egui itself ignores it)
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, depth_buffer: 24, multisampling: view3d::MSAA as u16, ..Default::default() };
    let r = eframe::run_native("corvus-sweep", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, opts)))));
    DONE.store(true, Ordering::Relaxed);
    r
}

/// `--serve`'s address: empty (all interfaces, the default port), `ADDR`, `:PORT`, `PORT` or `ADDR:PORT`.
fn parse_bind(v: &str) -> Option<std::net::SocketAddr> {
    let v = v.trim();
    let (host, port) = match v.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => ("", v.parse().ok()?),
        None => (v, net::PORT),
    };
    let ip: std::net::IpAddr = if host.is_empty() { std::net::Ipv4Addr::UNSPECIFIED.into() } else { host.parse().ok()? };
    Some(std::net::SocketAddr::new(ip, port))
}

fn die(msg: &str) -> ! {
    eprintln!("corvus-sweep: {msg}\n\n{USAGE}");
    std::process::exit(2)
}
