//! The few USB/IP operations this feature looks at.
//! See https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html
//! Every field is big endian.

use hbb_common::{
    anyhow::anyhow,
    bail, timeout,
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    },
    ResultType,
};

pub const VERSION: u16 = 0x0111;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;

const OP_HEADER_LEN: usize = 8;
const BUSID_LEN: usize = 32;
const PATH_LEN: usize = 256;
const DEVICE_LEN: usize = PATH_LEN + BUSID_LEN + 24;
const INTERFACE_LEN: usize = 4;
/// More than any real server exports; bounds what a peer can make us read.
const MAX_DEVICES: u32 = 256;
const DEVLIST_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub busid: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub interfaces: Vec<Interface>,
}

/// What the first bytes of a client's stream ask the server for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Devlist,
    Import(String),
    Other,
}

pub fn devlist_request() -> [u8; OP_HEADER_LEN] {
    let mut req = [0u8; OP_HEADER_LEN];
    req[0..2].copy_from_slice(&VERSION.to_be_bytes());
    req[2..4].copy_from_slice(&OP_REQ_DEVLIST.to_be_bytes());
    req
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn busid(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// `None` while too few bytes have arrived to tell.
pub fn classify(buf: &[u8]) -> Option<Request> {
    if buf.len() < OP_HEADER_LEN {
        return None;
    }
    match be16(&buf[2..4]) {
        OP_REQ_DEVLIST => Some(Request::Devlist),
        OP_REQ_IMPORT => {
            if buf.len() < OP_HEADER_LEN + BUSID_LEN {
                return None;
            }
            Some(Request::Import(busid(
                &buf[OP_HEADER_LEN..OP_HEADER_LEN + BUSID_LEN],
            )))
        }
        _ => Some(Request::Other),
    }
}

/// Parses one `usbip_usb_device` record without its interfaces.
fn parse_device(b: &[u8]) -> Device {
    let o = PATH_LEN + BUSID_LEN;
    Device {
        busid: busid(&b[PATH_LEN..o]),
        vendor_id: be16(&b[o + 12..o + 14]),
        product_id: be16(&b[o + 14..o + 16]),
        class: b[o + 18],
        subclass: b[o + 19],
        protocol: b[o + 20],
        interfaces: Vec::new(),
    }
}

fn num_interfaces(b: &[u8]) -> usize {
    b[DEVICE_LEN - 1] as usize
}

/// Asks the USB/IP server behind `127.0.0.1:port` for its exported devices.
pub async fn fetch_devlist(port: u16) -> ResultType<Vec<Device>> {
    timeout(DEVLIST_TIMEOUT_MS, fetch_devlist_(port))
        .await
        .map_err(|_| anyhow!("USB/IP device list timed out"))?
}

async fn fetch_devlist_(port: u16) -> ResultType<Vec<Device>> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await?;
    s.write_all(&devlist_request()).await?;
    let mut header = [0u8; 12];
    s.read_exact(&mut header).await?;
    if be16(&header[2..4]) != OP_REP_DEVLIST {
        bail!("unexpected USB/IP reply {:#06x}", be16(&header[2..4]));
    }
    let status = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if status != 0 {
        bail!("USB/IP server refused the device list, status {}", status);
    }
    let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if count > MAX_DEVICES {
        bail!("USB/IP server reported {} devices", count);
    }
    let mut devices = Vec::with_capacity(count as usize);
    let mut record = [0u8; DEVICE_LEN];
    for _ in 0..count {
        s.read_exact(&mut record).await?;
        let mut dev = parse_device(&record);
        for _ in 0..num_interfaces(&record) {
            let mut i = [0u8; INTERFACE_LEN];
            s.read_exact(&mut i).await?;
            dev.interfaces.push(Interface {
                class: i[0],
                subclass: i[1],
                protocol: i[2],
            });
        }
        devices.push(dev);
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_waits_for_the_busid() {
        let mut req = vec![0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0];
        assert_eq!(classify(&req), None);
        let mut id = [0u8; BUSID_LEN];
        id[..3].copy_from_slice(b"1-2");
        req.extend_from_slice(&id);
        assert_eq!(classify(&req), Some(Request::Import("1-2".to_owned())));
        assert_eq!(classify(&devlist_request()), Some(Request::Devlist));
        assert_eq!(classify(&[1, 0x11, 0, 1, 0, 0, 0, 0]), Some(Request::Other));
    }

    #[test]
    fn parses_a_device_record() {
        let mut r = [0u8; DEVICE_LEN];
        r[PATH_LEN..PATH_LEN + 3].copy_from_slice(b"3-1");
        let o = PATH_LEN + BUSID_LEN;
        r[o + 12..o + 14].copy_from_slice(&0x1050u16.to_be_bytes());
        r[o + 14..o + 16].copy_from_slice(&0x0407u16.to_be_bytes());
        r[DEVICE_LEN - 1] = 3;
        let d = parse_device(&r);
        assert_eq!(d.busid, "3-1");
        assert_eq!((d.vendor_id, d.product_id), (0x1050, 0x0407));
        assert_eq!(num_interfaces(&r), 3);
    }
}
