//! The controller side: shares local devices through the local USB/IP server.
//!
//! Streams the peer opens are relayed to `127.0.0.1:USBIP_TCP_PORT`, but only
//! once their first request is known to be a device list or an import of a
//! device this session shares, so the peer cannot reach other exported devices.

use super::{
    close_msg, data_msg,
    tools::{self, LocalDevice},
    usbip_proto::{self, Request},
    wrap, USB_MAX_FRAME, USB_STREAM_QUEUE, USBIP_TCP_PORT,
};
use crate::client::Data;
use base::message_proto::*;
use hbb_common::{
    bytes::Bytes,
    log, timeout,
    tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::mpsc,
    },
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const CONNECT_TIMEOUT_MS: u64 = 3000;
/// The longest first request we buffer: an import header plus its bus id.
const MAX_PENDING: usize = 64;

/// Whether this side can share at all.
pub(crate) fn is_supported() -> bool {
    tools::usbip_server().is_some()
}

#[derive(Default)]
struct Share {
    bound_by_us: bool,
    attached: bool,
    stopping: bool,
}

enum Stream {
    Pending(Vec<u8>),
    Open(mpsc::Sender<Bytes>),
}

#[derive(Default)]
struct State {
    local: Vec<LocalDevice>,
    shared: HashMap<String, Share>,
    streams: HashMap<u32, Stream>,
    error: String,
}

pub(crate) type UiCallback = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct UsbRedirectClient {
    state: Arc<Mutex<State>>,
    to_peer: mpsc::UnboundedSender<Data>,
    ui: UiCallback,
}

impl UsbRedirectClient {
    pub(crate) fn new(to_peer: mpsc::UnboundedSender<Data>, ui: UiCallback) -> Self {
        Self {
            state: Default::default(),
            to_peer,
            ui,
        }
    }

    fn send(&self, msg: Message) {
        self.to_peer.send(Data::Message(msg)).ok();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn push_ui(&self) {
        let s = self.lock();
        let devices: Vec<_> = s
            .local
            .iter()
            .map(|d| {
                let state = match s.shared.get(&d.bus_id) {
                    Some(sh) if sh.stopping => "stopping",
                    Some(sh) if sh.attached => "attached",
                    Some(_) => "sharing",
                    None => "idle",
                };
                json!({
                    "bus_id": d.bus_id,
                    "vid": format!("{:04x}", d.vendor_id),
                    "pid": format!("{:04x}", d.product_id),
                    "description": d.description,
                    "bound": d.bound,
                    "state": state,
                })
            })
            .collect();
        let v = json!({ "devices": devices, "error": s.error }).to_string();
        drop(s);
        (self.ui)(v);
    }

    fn set_error(&self, e: impl ToString) {
        self.lock().error = e.to_string();
    }

    /// `action` is "list", "share" or "stop", from the UI.
    pub(crate) fn handle_ui(&self, action: &str, bus_id: String) {
        self.lock().error.clear();
        let this = self.clone();
        match action {
            "list" => {
                tokio::task::spawn_blocking(move || {
                    this.refresh();
                    this.push_ui();
                });
            }
            "share" => {
                tokio::task::spawn_blocking(move || {
                    this.share(bus_id);
                    this.push_ui();
                });
            }
            "stop" => {
                let known = match self.lock().shared.get_mut(&bus_id) {
                    Some(sh) => {
                        sh.stopping = true;
                        true
                    }
                    None => false,
                };
                if known {
                    self.send(wrap(usb_redirect::Union::Detach(UsbDetachRequest {
                        bus_id,
                        ..Default::default()
                    })));
                }
                self.push_ui();
            }
            _ => log::warn!("usb redirection: unknown action {}", action),
        }
    }

    fn refresh(&self) {
        match tools::list_local() {
            Ok(local) => self.lock().local = local,
            Err(e) => self.set_error(e),
        }
    }

    /// Blocking: may wait for an elevation prompt.
    fn share(&self, bus_id: String) {
        self.refresh();
        let dev = self.lock().local.iter().find(|d| d.bus_id == bus_id).cloned();
        let Some(dev) = dev else {
            self.set_error(format!("{} is no longer connected", bus_id));
            return;
        };
        if self.lock().shared.contains_key(&bus_id) {
            return;
        }
        let mut bound_by_us = false;
        if !dev.bound {
            if let Err(e) = tools::bind(&bus_id) {
                self.set_error(e);
                return;
            }
            bound_by_us = true;
            self.refresh();
        }
        // Registered before the request goes out: the peer's import must pass the gate.
        self.lock().shared.insert(
            bus_id.clone(),
            Share {
                bound_by_us,
                ..Default::default()
            },
        );
        log::info!(
            "usb redirection: sharing {} ({:04x}:{:04x} {})",
            bus_id,
            dev.vendor_id,
            dev.product_id,
            dev.description
        );
        self.send(wrap(usb_redirect::Union::Attach(UsbAttachRequest {
            device: Some(UsbDeviceInfo {
                bus_id,
                vendor_id: dev.vendor_id as _,
                product_id: dev.product_id as _,
                description: dev.description,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })));
    }

    fn unshare(&self, bus_id: &str) {
        let share = self.lock().shared.remove(bus_id);
        if share.map_or(false, |s| s.bound_by_us) {
            let bus_id = bus_id.to_owned();
            std::thread::spawn(move || {
                if let Err(e) = tools::unbind(&bus_id) {
                    log::warn!("usb redirection: unbind {}: {}", bus_id, e);
                }
            });
        }
    }

    pub(crate) fn handle_peer(&self, frame: UsbRedirect) {
        match frame.union {
            Some(usb_redirect::Union::AttachResult(r)) => {
                if r.success {
                    if let Some(sh) = self.lock().shared.get_mut(&r.bus_id) {
                        sh.attached = true;
                    }
                    log::info!("usb redirection: peer attached {} at port {}", r.bus_id, r.port);
                } else {
                    log::warn!("usb redirection: peer refused {}: {}", r.bus_id, r.error);
                    self.set_error(format!("{}: {}", r.bus_id, r.error));
                    self.unshare(&r.bus_id);
                }
                self.push_ui();
            }
            Some(usb_redirect::Union::Detached(d)) => {
                log::info!("usb redirection: peer detached {}", d.bus_id);
                self.unshare(&d.bus_id);
                self.push_ui();
            }
            Some(usb_redirect::Union::Data(d)) => self.on_data(d.stream_id, d.data),
            Some(usb_redirect::Union::Close(c)) => {
                self.lock().streams.remove(&c.stream_id);
            }
            _ => {}
        }
    }

    fn refuse_stream(&self, s: &mut State, id: u32) {
        s.streams.remove(&id);
        self.send(close_msg(id));
    }

    fn on_data(&self, id: u32, data: Bytes) {
        let mut s = self.lock();
        let pending = match s.streams.get_mut(&id) {
            Some(Stream::Open(tx)) => {
                if tx.try_send(data).is_err() {
                    hbb_common::throttled_log!(
                        std::time::Duration::from_secs(10),
                        warn,
                        "usb redirection: stream {} overflowed, closing it",
                        id
                    );
                    self.refuse_stream(&mut s, id);
                }
                return;
            }
            Some(Stream::Pending(buf)) => {
                buf.extend_from_slice(&data);
                std::mem::take(buf)
            }
            None => {
                if s.shared.is_empty() {
                    self.refuse_stream(&mut s, id);
                    return;
                }
                data.to_vec()
            }
        };
        let allowed = match usbip_proto::classify(&pending) {
            None if pending.len() < MAX_PENDING => {
                s.streams.insert(id, Stream::Pending(pending));
                return;
            }
            None | Some(Request::Other) => false,
            Some(Request::Devlist) => true,
            Some(Request::Import(bus_id)) => s.shared.contains_key(&bus_id),
        };
        if !allowed {
            log::warn!("usb redirection: refused stream {} from the peer", id);
            self.refuse_stream(&mut s, id);
            return;
        }
        let (tx, rx) = mpsc::channel(USB_STREAM_QUEUE);
        s.streams.insert(id, Stream::Open(tx));
        drop(s);
        let this = self.clone();
        tokio::spawn(async move {
            this.pump(id, pending, rx).await;
            this.lock().streams.remove(&id);
            this.send(close_msg(id));
        });
    }

    async fn pump(&self, id: u32, first: Vec<u8>, mut from_peer: mpsc::Receiver<Bytes>) {
        let socket = match timeout(
            CONNECT_TIMEOUT_MS,
            TcpStream::connect(("127.0.0.1", USBIP_TCP_PORT)),
        )
        .await
        {
            Ok(Ok(s)) => s,
            res => {
                log::warn!("usb redirection: local USB/IP server unreachable: {:?}", res);
                return;
            }
        };
        let (mut rd, mut wr) = socket.into_split();
        if wr.write_all(&first).await.is_err() {
            return;
        }
        let reader = async {
            let mut buf = vec![0u8; USB_MAX_FRAME];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => self.send(data_msg(id, Bytes::copy_from_slice(&buf[..n]))),
                }
            }
        };
        let writer = async {
            while let Some(data) = from_peer.recv().await {
                if wr.write_all(&data).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = reader => {},
            _ = writer => {},
        }
    }

    /// Session end. The peer detaches on its own when the connection closes.
    pub(crate) fn close(&self) {
        let ids: Vec<String> = {
            let mut s = self.lock();
            s.streams.clear();
            s.shared.keys().cloned().collect()
        };
        for bus_id in ids {
            self.unshare(&bus_id);
        }
    }
}
