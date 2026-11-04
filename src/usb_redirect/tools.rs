//! The external USB/IP tools, all blocking: call them off the async runtime.
//!
//! Controlled side (attaches): `usbip` from usbip-win2 on Windows, from the
//! distribution's usbip package on Linux (needs the vhci-hcd module).
//! Controller (shares): `usbipd` from usbipd-win on Windows, `usbip` plus a
//! running `usbipd` on Linux (needs the usbip-host module).

use hbb_common::{anyhow::anyhow, bail, log, ResultType};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct LocalDevice {
    pub bus_id: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub description: String,
    /// Exported by the local USB/IP server, so a peer may import it.
    pub bound: bool,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn first_existing(name: &str, fallbacks: &[PathBuf]) -> Option<PathBuf> {
    find_in_path(name).or_else(|| fallbacks.iter().find(|p| p.is_file()).cloned())
}

#[cfg(windows)]
fn program_files(rel: &str) -> PathBuf {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    Path::new(&base).join(rel)
}

/// The USB/IP client the controlled side attaches with.
pub fn usbip_client() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        first_existing("usbip.exe", &[program_files("USBip\\usbip.exe")])
    }
    #[cfg(target_os = "linux")]
    {
        first_existing(
            "usbip",
            &[PathBuf::from("/usr/sbin/usbip"), PathBuf::from("/usr/bin/usbip")],
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// The tool the controller lists and binds devices with.
pub fn usbip_server() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        first_existing("usbipd.exe", &[program_files("usbipd-win\\usbipd.exe")])
    }
    #[cfg(not(windows))]
    {
        usbip_client()
    }
}

fn run(exe: &Path, args: &[&str]) -> ResultType<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    log::debug!("usb redirection: {} {:?}", exe.display(), args);
    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        bail!("{} failed: {}", exe.display(), msg);
    }
    Ok(stdout)
}

// ---- controlled side ----

/// Attaches `bus_id` exported behind `127.0.0.1:port`; returns the hub port.
pub fn attach(port: u16, bus_id: &str) -> ResultType<i32> {
    let exe = usbip_client().ok_or_else(|| anyhow!("USB/IP client not installed"))?;
    let port_s = port.to_string();
    #[cfg(windows)]
    {
        // `--once`: no automatic re-attach attempts once the session is gone.
        let out = run(
            &exe,
            &[
                "--tcp-port", &port_s, "attach", "--remote", "127.0.0.1", "--bus-id", bus_id,
                "--terse", "--once",
            ],
        )?;
        out.trim()
            .parse::<i32>()
            .map_err(|_| anyhow!("unexpected usbip output: {}", out.trim()))
    }
    #[cfg(not(windows))]
    {
        run(
            &exe,
            &["--tcp-port", &port_s, "attach", "-r", "127.0.0.1", "-b", bus_id],
        )?;
        let ports = run(&exe, &["port"])?;
        linux_hub_port(&ports, port, bus_id)
            .ok_or_else(|| anyhow!("attached {} but its port is not listed", bus_id))
    }
}

/// `usbip port` lists "Port 00: <Port in Use> ..." followed, a few lines on,
/// by "... -> usbip://127.0.0.1:PORT/BUSID".
#[cfg(not(windows))]
fn linux_hub_port(out: &str, port: u16, bus_id: &str) -> Option<i32> {
    let needle = format!("usbip://127.0.0.1:{}/{}", port, bus_id);
    let mut current = None;
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Port ") {
            current = rest.split(':').next().and_then(|n| n.trim().parse().ok());
        } else if line.ends_with(&needle) {
            return current;
        }
    }
    None
}

pub fn detach(hub_port: i32, port: u16, bus_id: &str) -> ResultType<()> {
    let exe = usbip_client().ok_or_else(|| anyhow!("USB/IP client not installed"))?;
    let hub = hub_port.to_string();
    let res = run(&exe, &["detach", "--port", &hub]).map(|_| ());
    #[cfg(windows)]
    {
        // Also cancel any pending re-attach attempt for this device.
        let port_s = port.to_string();
        let _ = run(
            &exe,
            &["--tcp-port", &port_s, "attach", "--remote", "127.0.0.1", "--bus-id", bus_id, "--stop"],
        );
    }
    #[cfg(not(windows))]
    let _ = (port, bus_id);
    res
}

// ---- controller ----

pub fn list_local() -> ResultType<Vec<LocalDevice>> {
    let exe = usbip_server().ok_or_else(|| anyhow!("USB/IP server not installed"))?;
    #[cfg(windows)]
    return Ok(parse_usbipd_list(&run(&exe, &["list"])?));
    #[cfg(not(windows))]
    return Ok(run(&exe, &["list", "-p", "-l"])?
        .lines()
        .filter_map(parse_linux_list_line)
        .collect());
}

/// Parses `usbipd list` (usbipd-win 4/5), its "Connected:" section only.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_usbipd_list(out: &str) -> Vec<LocalDevice> {
    const STATES: &[&str] = &["Not shared", "Shared (forced)", "Shared", "Attached"];
    let mut devices = Vec::new();
    let mut connected = false;
    for line in out.lines() {
        let t = line.trim();
        if t.ends_with(':') && !t.contains(' ') {
            connected = t == "Connected:";
            continue;
        }
        if !connected || t.is_empty() || t.starts_with("BUSID") {
            continue;
        }
        let mut parts = t.split_whitespace();
        let (Some(bus_id), Some(ids)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(pos) = t.find(ids) else {
            continue;
        };
        let rest = &t[pos + ids.len()..];
        let Some((vid, pid)) = ids.split_once(':') else {
            continue;
        };
        let (Ok(vendor_id), Ok(product_id)) =
            (u16::from_str_radix(vid, 16), u16::from_str_radix(pid, 16))
        else {
            continue;
        };
        let rest = rest.trim();
        let state = STATES.iter().find(|s| rest.ends_with(*s)).copied().unwrap_or("");
        let description = rest[..rest.len() - state.len()].trim().to_owned();
        devices.push(LocalDevice {
            bus_id: bus_id.to_owned(),
            vendor_id,
            product_id,
            description,
            bound: state.starts_with("Shared") || state == "Attached",
        });
    }
    devices
}

/// `usbip list -p -l` prints "busid=1-1#usbid=1050:0407#".
#[cfg(not(windows))]
fn parse_linux_list_line(line: &str) -> Option<LocalDevice> {
    let mut bus_id = None;
    let mut ids = None;
    for field in line.trim().split('#') {
        if let Some(v) = field.strip_prefix("busid=") {
            bus_id = Some(v.to_owned());
        } else if let Some(v) = field.strip_prefix("usbid=") {
            ids = v.split_once(':');
        }
    }
    let bus_id = bus_id?;
    let (vid, pid) = ids?;
    let sys = Path::new("/sys/bus/usb/devices").join(&bus_id);
    let read = |f: &str| {
        std::fs::read_to_string(sys.join(f))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    let description = format!("{} {}", read("manufacturer"), read("product"))
        .trim()
        .to_owned();
    Some(LocalDevice {
        bound: Path::new("/sys/bus/usb/drivers/usbip-host")
            .join(&bus_id)
            .exists(),
        bus_id,
        vendor_id: u16::from_str_radix(vid, 16).ok()?,
        product_id: u16::from_str_radix(pid, 16).ok()?,
        description,
    })
}

/// Exports `bus_id` through the local USB/IP server. Needs admin rights, so
/// it asks for them (UAC on Windows, polkit on Linux) unless already root.
pub fn bind(bus_id: &str) -> ResultType<()> {
    let exe = usbip_server().ok_or_else(|| anyhow!("USB/IP server not installed"))?;
    #[cfg(windows)]
    {
        let arg = format!("bind --busid {}", bus_id);
        if !crate::platform::windows::run_uac(&exe.to_string_lossy(), &arg)? {
            bail!("elevation was refused");
        }
        // `runas` does not wait for the elevated process.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if list_local()?.iter().any(|d| d.bus_id == bus_id && d.bound) {
                return Ok(());
            }
        }
        bail!("{} was not shared", bus_id);
    }
    #[cfg(not(windows))]
    {
        let exe = exe.to_string_lossy().into_owned();
        if crate::platform::is_root() {
            run(Path::new(&exe), &["bind", "-b", bus_id]).map(|_| ())
        } else {
            run(Path::new("pkexec"), &[&exe, "bind", "-b", bus_id]).map(|_| ())
        }
    }
}

/// Linux only: a bound device is taken from its local driver, so give it
/// back. usbipd-win leaves a shared device usable until it is attached.
pub fn unbind(bus_id: &str) -> ResultType<()> {
    #[cfg(not(windows))]
    {
        let exe = usbip_server().ok_or_else(|| anyhow!("USB/IP server not installed"))?;
        if crate::platform::is_root() {
            return run(&exe, &["unbind", "-b", bus_id]).map(|_| ());
        }
        let exe = exe.to_string_lossy().into_owned();
        run(Path::new("pkexec"), &[&exe, "unbind", "-b", bus_id]).map(|_| ())
    }
    #[cfg(windows)]
    {
        let _ = bus_id;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usbipd_win_list() {
        let out = "Connected:\r\n\
BUSID  VID:PID    DEVICE                                                        STATE\r\n\
1-7    8087:0026  Intel(R) Wireless Bluetooth(R)                                Not shared\r\n\
2-3    1050:0407  USB Input Device, Microsoft Usbccid Smartcard Reader (WUDF)   Shared\r\n\
\r\n\
Persisted:\r\n\
GUID                                  DEVICE\r\n";
        let d = parse_usbipd_list(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].bus_id, "1-7");
        assert!(!d[0].bound);
        assert_eq!(d[0].description, "Intel(R) Wireless Bluetooth(R)");
        assert_eq!((d[1].vendor_id, d[1].product_id), (0x1050, 0x0407));
        assert!(d[1].bound);
    }

    #[cfg(not(windows))]
    #[test]
    fn finds_the_linux_hub_port() {
        let out = "Imported USB devices\n====================\n\
Port 00: <Port in Use> at High Speed(480Mbps)\n       unknown vendor : unknown product (1050:0407)\n\
       3-1 -> usbip://127.0.0.1:41234/1-2\n           -> remote bus/dev 001/004\n";
        assert_eq!(linux_hub_port(out, 41234, "1-2"), Some(0));
        assert_eq!(linux_hub_port(out, 41234, "1-3"), None);
    }
}
