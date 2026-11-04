//! The controlled side: attaches devices the controller shares.
//!
//! The local USB/IP client connects to a loopback listener owned by this
//! connection; each accepted TCP stream is relayed to the controller, whose
//! own USB/IP server answers it.

use super::{
    close_msg, data_msg, policy, tools, usbip_proto, wrap, USB_MAX_ATTACHED, USB_MAX_FRAME, USB_STREAM_QUEUE,
};
use base::message_proto::*;
use hbb_common::{
    bytes::Bytes,
    log,
    tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        task::JoinHandle,
        time::Instant,
    },
    ResultType,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
};

/// Whether this side can attach at all, ignoring the option.
pub(crate) fn is_supported() -> bool {
    tools::usbip_client().is_some()
}

/// The connection's outbound queue, as `server::connection::UsbConnTx`.
pub(crate) type UsbConnTx = mpsc::UnboundedSender<(Instant, Arc<Message>)>;

type Streams = Arc<Mutex<HashMap<u32, mpsc::Sender<Bytes>>>>;

pub(crate) struct UsbRedirectServer {
    tx: UsbConnTx,
    conn_id: i32,
    listener: Option<(u16, JoinHandle<()>)>,
    streams: Streams,
    /// bus id -> hub port, shared with the attach tasks.
    attached: Arc<Mutex<HashMap<String, i32>>>,
}

fn send(tx: &UsbConnTx, msg: Message) {
    tx.send((Instant::now(), Arc::new(msg))).ok();
}

impl UsbRedirectServer {
    pub(crate) fn new(tx: UsbConnTx, conn_id: i32) -> Self {
        Self {
            tx,
            conn_id,
            listener: None,
            streams: Default::default(),
            attached: Default::default(),
        }
    }

    /// Refuses an attach without looking at it, for a peer not allowed to.
    pub(crate) fn refuse(tx: &UsbConnTx, frame: UsbRedirect, reason: &str) {
        if let Some(usb_redirect::Union::Attach(req)) = frame.union {
            let bus_id = req.device.bus_id.clone();
            send(tx, attach_result(bus_id, Err(reason.to_owned())));
        }
    }

    pub(crate) async fn handle(&mut self, frame: UsbRedirect, deny: String) {
        match frame.union {
            Some(usb_redirect::Union::Attach(req)) => self.on_attach(req, deny).await,
            Some(usb_redirect::Union::Detach(req)) => self.on_detach(req.bus_id),
            Some(usb_redirect::Union::Data(d)) => self.on_data(d.stream_id, d.data),
            Some(usb_redirect::Union::Close(c)) => {
                self.lock_streams().remove(&c.stream_id);
            }
            _ => {}
        }
    }

    fn lock_streams(&self) -> std::sync::MutexGuard<'_, HashMap<u32, mpsc::Sender<Bytes>>> {
        self.streams.lock().unwrap()
    }

    async fn ensure_listener(&mut self) -> ResultType<u16> {
        if let Some((port, _)) = &self.listener {
            return Ok(*port);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(accept_loop(listener, self.tx.clone(), self.streams.clone()));
        log::info!("#{} usb redirection listening on 127.0.0.1:{}", self.conn_id, port);
        self.listener = Some((port, task));
        Ok(port)
    }

    async fn on_attach(&mut self, req: UsbAttachRequest, deny: String) {
        let bus_id = req.device.bus_id.clone();
        let busy = {
            let attached = self.attached.lock().unwrap();
            if attached.contains_key(&bus_id) {
                Some("already attached")
            } else if attached.len() >= USB_MAX_ATTACHED {
                Some("too many attached devices")
            } else {
                None
            }
        };
        if let Some(reason) = busy {
            send(&self.tx, attach_result(bus_id, Err(reason.to_owned())));
            return;
        }
        let port = match self.ensure_listener().await {
            Ok(port) => port,
            Err(e) => {
                send(&self.tx, attach_result(bus_id, Err(e.to_string())));
                return;
            }
        };
        let tx = self.tx.clone();
        let attached = self.attached.clone();
        let conn_id = self.conn_id;
        tokio::spawn(async move {
            let res = attach(port, &bus_id, &deny).await;
            match &res {
                Ok(hub) => {
                    log::info!(
                        "#{} usb redirection: attached {} ({:04x}:{:04x} {}) at port {}",
                        conn_id,
                        bus_id,
                        req.device.vendor_id,
                        req.device.product_id,
                        req.device.description,
                        hub
                    );
                    attached.lock().unwrap().insert(bus_id.clone(), *hub);
                }
                Err(e) => log::warn!("#{} usb redirection: {} refused: {}", conn_id, bus_id, e),
            }
            send(&tx, attach_result(bus_id, res));
        });
    }

    fn on_detach(&mut self, bus_id: String) {
        let hub = self.attached.lock().unwrap().remove(&bus_id);
        let port = self.listener.as_ref().map(|(p, _)| *p).unwrap_or_default();
        let tx = self.tx.clone();
        let conn_id = self.conn_id;
        tokio::spawn(async move {
            if let Some(hub) = hub {
                let b = bus_id.clone();
                match tokio::task::spawn_blocking(move || tools::detach(hub, port, &b)).await {
                    Ok(Ok(())) => log::info!("#{} usb redirection: detached {}", conn_id, bus_id),
                    Ok(Err(e)) => log::warn!("#{} usb redirection: detach {}: {}", conn_id, bus_id, e),
                    Err(e) => log::warn!("#{} usb redirection: detach {}: {}", conn_id, bus_id, e),
                }
            }
            send(
                &tx,
                wrap(usb_redirect::Union::Detached(UsbDetached {
                    bus_id,
                    ..Default::default()
                })),
            );
        });
    }

    fn on_data(&self, stream_id: u32, data: Bytes) {
        let mut streams = self.lock_streams();
        let Some(s) = streams.get(&stream_id) else {
            return;
        };
        if s.try_send(data).is_err() {
            hbb_common::throttled_log!(
                std::time::Duration::from_secs(10),
                warn,
                "usb redirection: stream {} overflowed, closing it",
                stream_id
            );
            streams.remove(&stream_id);
            send(&self.tx, close_msg(stream_id));
        }
    }

    /// Session end: detach everything, then drop the listener and streams.
    pub(crate) fn close_all(&mut self) {
        let port = self.listener.as_ref().map(|(p, _)| *p).unwrap_or_default();
        let attached: Vec<(String, i32)> = self.attached.lock().unwrap().drain().collect();
        let conn_id = self.conn_id;
        if !attached.is_empty() {
            std::thread::spawn(move || {
                for (bus_id, hub) in attached {
                    match tools::detach(hub, port, &bus_id) {
                        Ok(()) => log::info!("#{} usb redirection: detached {} on close", conn_id, bus_id),
                        Err(e) => log::warn!("#{} usb redirection: detach {}: {}", conn_id, bus_id, e),
                    }
                }
            });
        }
        if let Some((_, task)) = self.listener.take() {
            task.abort();
        }
        self.lock_streams().clear();
    }
}

impl Drop for UsbRedirectServer {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn attach_result(bus_id: String, res: Result<i32, String>) -> Message {
    let (success, error, port) = match res {
        Ok(port) => (true, String::new(), port),
        Err(e) => (false, e, 0),
    };
    wrap(usb_redirect::Union::AttachResult(UsbAttachResult {
        bus_id,
        success,
        error,
        port,
        ..Default::default()
    }))
}

/// Checks the device against the class policy over the tunnel, then attaches it.
async fn attach(port: u16, bus_id: &str, deny: &str) -> Result<i32, String> {
    let devices = usbip_proto::fetch_devlist(port)
        .await
        .map_err(|e| format!("cannot list the shared devices: {}", e))?;
    let dev = devices
        .iter()
        .find(|d| d.busid == bus_id)
        .ok_or_else(|| format!("{} is not shared by the controlling side", bus_id))?;
    if let Some(reason) = policy::denied(dev, deny) {
        return Err(reason);
    }
    let b = bus_id.to_owned();
    tokio::task::spawn_blocking(move || tools::attach(port, &b))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

async fn accept_loop(listener: TcpListener, tx: UsbConnTx, streams: Streams) {
    let next_id = AtomicU32::new(1);
    loop {
        let (socket, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("usb redirection: accept: {}", e);
                return;
            }
        };
        if !addr.ip().is_loopback() {
            continue;
        }
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let (data_tx, data_rx) = mpsc::channel(USB_STREAM_QUEUE);
        streams.lock().unwrap().insert(id, data_tx);
        tokio::spawn(pump(id, socket, data_rx, tx.clone(), streams.clone()));
    }
}

/// Relays one local socket: its reads go to the peer as `data`, frames from
/// the peer are written to it. Ends with a `close` either way.
async fn pump(
    id: u32,
    socket: TcpStream,
    mut from_peer: mpsc::Receiver<Bytes>,
    tx: UsbConnTx,
    streams: Streams,
) {
    let (mut rd, mut wr) = socket.into_split();
    let reader = async {
        let mut buf = vec![0u8; USB_MAX_FRAME];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => send(&tx, data_msg(id, Bytes::copy_from_slice(&buf[..n]))),
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
    streams.lock().unwrap().remove(&id);
    send(&tx, close_msg(id));
}
