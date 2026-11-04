//! USB redirection inside a remote session.
//!
//! The controller shares a USB device plugged into it through its own USB/IP
//! server (usbipd-win on Windows, `usbipd` on Linux). The controlled side
//! attaches it with its USB/IP client (usbip-win2 on Windows, vhci-hcd on
//! Linux), pointed at a loopback listener whose TCP streams are carried over
//! the session as `UsbRedirect` frames. Neither side opens a network port.
//!
//! The approach (tunnelling USB/IP bytes through the session, a loopback
//! listener on the attaching side, `usbip`/`usbipd` doing the USB work) is
//! adapted from RuDesktop's USB/IP forwarding, Copyright (C) RuDesktop
//! (https://rudesktop.ru, https://sourcecraft.dev/rudesktop/rudesktop-client),
//! licensed AGPL-3.0 like RustDesk.
//!
//! Third-party tools are runtime dependencies and are not bundled:
//! usbip-win2 (BSD-2-Clause, https://github.com/vadimgrn/usbip-win2) and
//! usbipd-win (GPL-3.0, https://github.com/dorssel/usbipd-win).

pub mod controlled;
pub mod controller;
pub mod policy;
pub mod tools;
pub mod usbip_proto;

/// The port every USB/IP server listens on.
pub const USBIP_PORT: u16 = 3240;
/// The largest payload of one `UsbStreamData` frame.
pub const MAX_FRAME: usize = 64 * 1024;
/// Frames queued towards one local socket before the stream is dropped.
pub const STREAM_QUEUE: usize = 1024;
/// Devices one session may have attached at once.
pub const MAX_ATTACHED: usize = 8;

use base::message_proto::*;
use hbb_common::bytes::Bytes;

pub fn wrap(union: usb_redirect::Union) -> Message {
    let mut msg = Message::new();
    msg.set_usb_redirect(UsbRedirect {
        union: Some(union),
        ..Default::default()
    });
    msg
}

pub fn data_msg(stream_id: u32, data: Bytes) -> Message {
    wrap(usb_redirect::Union::Data(UsbStreamData {
        stream_id,
        data,
        ..Default::default()
    }))
}

pub fn close_msg(stream_id: u32) -> Message {
    wrap(usb_redirect::Union::Close(UsbStreamClose {
        stream_id,
        ..Default::default()
    }))
}
