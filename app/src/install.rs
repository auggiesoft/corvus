//! Putting corvus-pluto on a Pluto-family board from Corvus Sweep, over SSH.
//!
//! The board binary is the one in this Corvus Sweep's own release (`corvus-pluto-vX.Y.Z-armv7.tar.gz`), fetched
//! with curl, checked against that release's `SHA256SUMS`, unpacked with tar and cached. (A hash pinned in the
//! source, as rtl.rs does, can't work here: the archive is built after the source is tagged. The sums file
//! comes from the same release over HTTPS, so this catches a damaged or truncated download, not a hijacked
//! release.) `CORVUS_PLUTO_BIN` names a local binary instead (a build from `firmware/pluto/build.sh`), and
//! `CORVUS_PLUTO_URL` another release directory.
//!
//! The board's stock login (root / analog) is tried first. If the board says no, the window asks for one; the
//! password lives in memory for that attempt only, never in the settings or the log.
//!
//! Host keys aren't checked: Tezuka and PlutoDVB2 make new ones at every boot, so there's nothing stable to
//! pin, and the board is on the user's own network at an address they typed.
//!
//! By default the program goes to /tmp (RAM): nothing is written to the board's flash and a reboot removes
//! it, as `install.sh RAM=1` does. `keep` puts it in /mnt/jffs2 with a boot hook, as plain `install.sh` does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handler};
use russh::keys::PublicKeyOrCertificate;
use russh::{ChannelMsg, Disconnect};
use sha2::{Digest, Sha256};

pub const STOCK_USER: &str = "root";
pub const STOCK_PASS: &str = "analog";
const SSH_PORT: u16 = 22;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BASE_URL: &str = "https://github.com/auggiesoft/corvus/releases/download";

/// What the install is doing, and how the last one ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Idle,
    Running(String),
    Done(String),
    /// The board refused the login: why (the window asks for another).
    NeedsLogin(String),
    Failed(String),
}

static JOB: parking_lot::Mutex<Job> = parking_lot::const_mutex(Job::Idle);

pub fn job() -> Job {
    JOB.lock().clone()
}

/// The finished job's outcome, once. A refused login comes back as `Err(Outcome::Login(..))`.
pub fn take_finished() -> Option<Result<String, Outcome>> {
    let mut j = JOB.lock();
    match std::mem::replace(&mut *j, Job::Idle) {
        Job::Done(m) => Some(Ok(m)),
        Job::NeedsLogin(why) => Some(Err(Outcome::Login(why))),
        Job::Failed(e) => Some(Err(Outcome::Failed(e))),
        other => {
            *j = other;
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Login(String),
    Failed(String),
}

/// One install attempt.
#[derive(Clone)]
pub struct Request {
    /// The board's address (host or host:port; the port is corvus-pluto's, not SSH's).
    pub addr: String,
    pub user: String,
    pub pass: String,
    /// /mnt/jffs2 and a boot hook, instead of /tmp.
    pub keep: bool,
}

impl Request {
    /// The board's stock login, to RAM.
    pub fn stock(addr: &str) -> Request {
        Request { addr: addr.to_string(), user: STOCK_USER.into(), pass: STOCK_PASS.into(), keep: false }
    }
}

enum Fail {
    Login(String),
    Other(String),
}

impl From<String> for Fail {
    fn from(e: String) -> Fail {
        Fail::Other(e)
    }
}

/// Install in the background; the window picks the outcome up with `take_finished`.
pub fn start(req: Request) {
    let mut j = JOB.lock();
    if matches!(*j, Job::Running(_)) {
        return;
    }
    *j = Job::Running(format!("Installing corvus-pluto on {}", host_of(&req.addr)));
    drop(j);
    std::thread::spawn(move || {
        let r = run(req);
        *JOB.lock() = match r {
            Ok(m) => Job::Done(m),
            Err(Fail::Login(w)) => Job::NeedsLogin(w),
            Err(Fail::Other(e)) => Job::Failed(e),
        };
    });
}

/// `--install-pluto`: the same, now, without a window.
pub fn install_now(req: Request) -> Result<String, Outcome> {
    run(req).map_err(|f| match f {
        Fail::Login(w) => Outcome::Login(w),
        Fail::Other(e) => Outcome::Failed(e),
    })
}

fn run(req: Request) -> Result<String, Fail> {
    let bin = board_binary()?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("no async runtime: {e}"))?;
    rt.block_on(install(&req, &bin))
}

/// `host` of `host:port`, `[v6]:port` or a bare host.
pub fn host_of(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    match addr.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() && !h.contains(':') => h.to_string(),
        _ => addr.to_string(),
    }
}

// ---- the board binary ----

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("corvus-sweep").join("pluto").join(VERSION)
}

fn sha256_hex(path: &Path) -> Option<String> {
    let b = std::fs::read(path).ok()?;
    Some(Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect())
}

/// The hash `sha256sum` printed for `name` in a SHA256SUMS file (`HASH  name` or `HASH *name`).
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, n) = l.split_once(char::is_whitespace)?;
        (n.trim_start().trim_start_matches('*') == name && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
    })
}

fn curl(url: &str, to: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--retry", "2", "-o"]).arg(to).arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("couldn't run curl to download {url}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let _ = std::fs::remove_file(to);
        Err(format!("download of {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The corvus-pluto program for the board: `CORVUS_PLUTO_BIN`, else this release's, downloaded and checked once.
fn board_binary() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("CORVUS_PLUTO_BIN").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()) {
        return if p.is_file() { Ok(p) } else { Err(format!("CORVUS_PLUTO_BIN: {} isn't a file", p.display())) };
    }
    let dir = data_dir();
    let dst = dir.join("corvus-pluto");
    if dst.is_file() {
        return Ok(dst);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tag = format!("v{VERSION}");
    let base = std::env::var("CORVUS_PLUTO_URL").unwrap_or_else(|_| format!("{BASE_URL}/{tag}"));
    let base = base.trim_end_matches('/');
    let archive = format!("corvus-pluto-{tag}-armv7.tar.gz");
    let part = dir.join(format!("{archive}.part"));
    let sums = dir.join("SHA256SUMS");
    curl(&format!("{base}/SHA256SUMS"), &sums)?;
    curl(&format!("{base}/{archive}"), &part)?;
    let want = sum_for(&std::fs::read_to_string(&sums).unwrap_or_default(), &archive).ok_or_else(|| format!("{archive} isn't in this release's SHA256SUMS"))?;
    let got = sha256_hex(&part).unwrap_or_default();
    if got != want {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{archive} didn't match its checksum (got {got}); not used"));
    }
    let out = dir.join("unpacked");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut tar = std::process::Command::new("tar");
    tar.arg("-xzf").arg(&part).arg("-C").arg(&out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        tar.creation_flags(0x0800_0000);
    }
    let r = tar.output().map_err(|e| format!("couldn't run tar: {e}"))?;
    let _ = std::fs::remove_file(&part);
    if !r.status.success() {
        return Err(format!("couldn't unpack {archive}: {}", String::from_utf8_lossy(&r.stderr).trim()));
    }
    let inner = out.join(archive.trim_end_matches(".tar.gz")).join("corvus-pluto");
    std::fs::rename(&inner, &dst).map_err(|e| format!("{archive} has no corvus-pluto ({}): {e}", inner.display()))?;
    let _ = std::fs::remove_dir_all(&out);
    Ok(dst)
}

// ---- SSH ----

struct Hk;

impl Handler for Hk {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true) // see the module comment
    }
}

/// Run one command on the board: its exit status and its output (stdout and stderr together), with `input` on stdin.
async fn exec(h: &client::Handle<Hk>, cmd: &str, input: Option<&[u8]>) -> Result<(u32, String), String> {
    let e = |what: &str, e: russh::Error| format!("{what}: {e}");
    let mut ch = h.channel_open_session().await.map_err(|x| e("opening a channel", x))?;
    ch.exec(true, cmd).await.map_err(|x| e("running a command", x))?;
    if let Some(b) = input {
        for c in b.chunks(32 * 1024) {
            ch.data(c).await.map_err(|x| e("sending the program", x))?;
        }
        ch.eof().await.map_err(|x| e("sending the program", x))?;
    }
    let (mut out, mut code) = (Vec::new(), None);
    let done = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(m) = ch.wait().await {
            match m {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
    })
    .await;
    if done.is_err() {
        return Err(format!("`{cmd}` didn't finish in 60 s"));
    }
    Ok((code.unwrap_or(255), String::from_utf8_lossy(&out).trim().to_string()))
}

/// Step 1 stops a running copy and writes the program from stdin; step 2 makes it executable and starts it.
fn commands(keep: bool) -> (&'static str, &'static str) {
    if keep {
        (
            "killall corvus-pluto 2>/dev/null; rm -f /mnt/jffs2/corvus-pluto; cat > /mnt/jffs2/corvus-pluto",
            "set -e; chmod +x /mnt/jffs2/corvus-pluto; \
             if grep -qs /mnt/jffs2/backdoor.sh /etc/init.d/*; then \
               touch /mnt/jffs2/backdoor.sh; \
               grep -q corvus-pluto /mnt/jffs2/backdoor.sh || echo '/mnt/jffs2/corvus-pluto serve >/dev/null 2>&1 &' >> /mnt/jffs2/backdoor.sh; \
               chmod +x /mnt/jffs2/backdoor.sh; echo 'starts at boot (/mnt/jffs2/backdoor.sh)'; \
             else echo 'this firmware has no boot hook: it runs until the next reboot'; fi; \
             (/mnt/jffs2/corvus-pluto serve >/dev/null 2>&1 </dev/null &); sleep 0.5; \
             /mnt/jffs2/corvus-pluto --version; pidof corvus-pluto >/dev/null && echo running",
        )
    } else {
        (
            "killall corvus-pluto 2>/dev/null; rm -f /tmp/corvus-pluto; cat > /tmp/corvus-pluto",
            "set -e; chmod +x /tmp/corvus-pluto; (/tmp/corvus-pluto serve >/dev/null 2>&1 </dev/null &); sleep 0.5; \
             /tmp/corvus-pluto --version; pidof corvus-pluto >/dev/null && echo running",
        )
    }
}

async fn install(req: &Request, bin: &Path) -> Result<String, Fail> {
    let host = host_of(&req.addr);
    let program = std::fs::read(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let cfg = Arc::new(client::Config { inactivity_timeout: Some(Duration::from_secs(120)), ..Default::default() });
    let mut s = match tokio::time::timeout(Duration::from_secs(8), client::connect(cfg, (host.as_str(), SSH_PORT), Hk)).await {
        Err(_) => return Err(format!("{host} didn't answer on SSH (port {SSH_PORT}) within 8 s").into()),
        Ok(Err(e)) => return Err(format!("can't open an SSH connection to {host}: {e}").into()),
        Ok(Ok(s)) => s,
    };
    match s.authenticate_password(req.user.clone(), req.pass.clone()).await.map_err(|e| format!("login to {host}: {e}"))? {
        AuthResult::Success => {}
        AuthResult::Failure { .. } => {
            return Err(Fail::Login(format!("{host} didn't accept the login for {}", req.user)));
        }
    }
    let (write, start) = commands(req.keep);
    let (code, out) = exec(&s, write, Some(&program)).await?;
    if code != 0 {
        return Err(format!("copying corvus-pluto to {host} failed ({code}): {out}").into());
    }
    let (code, out) = exec(&s, start, None).await?;
    let _ = s.disconnect(Disconnect::ByApplication, "", "en").await;
    if code != 0 || !out.lines().any(|l| l == "running") {
        return Err(format!("corvus-pluto didn't start on {host} ({code}): {out}").into());
    }
    let version = out.lines().find(|l| l.starts_with("corvus-pluto ")).unwrap_or("corvus-pluto");
    Ok(if req.keep { format!("{version} installed on {host} and starts at boot where the firmware allows") } else { format!("{version} running on {host} (from RAM: gone at the next reboot)") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_come_out_of_addresses() {
        assert_eq!(host_of("10.2.2.151:30433"), "10.2.2.151");
        assert_eq!(host_of("10.2.2.151"), "10.2.2.151");
        assert_eq!(host_of("pluto.local:30433"), "pluto.local");
        assert_eq!(host_of("[fe80::1]:30433"), "fe80::1");
        assert_eq!(host_of("fe80::1"), "fe80::1");
    }

    #[test]
    fn sums_are_found_by_file_name() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let s = format!("{h1}  corvus-sweep-v1-linux-x86_64.tar.gz\n{h2} *corvus-pluto-v1-armv7.tar.gz\nnot a hash  corvus-pluto-v1-armv7.tar.gz.sig\n");
        assert_eq!(sum_for(&s, "corvus-pluto-v1-armv7.tar.gz"), Some(h2));
        assert_eq!(sum_for(&s, "corvus-sweep-v1-linux-x86_64.tar.gz"), Some(h1));
        assert_eq!(sum_for(&s, "missing"), None);
    }

    #[test]
    fn ram_install_never_touches_flash() {
        let (a, b) = commands(false);
        assert!(!a.contains("jffs2") && !b.contains("jffs2") && !b.contains("backdoor"));
        let (a, b) = commands(true);
        assert!(a.contains("/mnt/jffs2/corvus-pluto") && b.contains("backdoor.sh"));
    }
}
