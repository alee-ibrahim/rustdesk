# USB redirection

A controlling user can plug a USB device (security key, smart card reader,
dongle, storage) into their own computer and use it on the remote computer
during a session.

## Attribution

The design — carrying USB/IP traffic inside the remote session, a loopback
listener on the attaching side, and the stock `usbip`/`usbipd` tools doing the
USB work — is adapted from the USB/IP forwarding in RuDesktop's client:

- RuDesktop client, Copyright (C) RuDesktop, <https://rudesktop.ru>,
  source: <https://sourcecraft.dev/rudesktop/rudesktop-client>
- License: GNU AGPL-3.0, the same license as RustDesk.

The code here was rewritten for RustDesk's current code base (Flutter UI, the
`libs/base` message proto, the server connection loop) and changed in several
ways listed below, so it is a derivative, not a copy.

## How it works

Sharing side (controller):

1. The UI lists local devices with `usbipd list` (Windows, usbipd-win) or
   `usbip list -l` (Linux).
2. Sharing a device binds it to the local USB/IP server if it is not already
   (`usbipd bind`, elevated through UAC; `usbip bind` through polkit on Linux)
   and sends `UsbAttachRequest` to the peer.
3. Streams the peer opens arrive as `UsbStreamData` frames and are relayed to
   `127.0.0.1:3240`, but only after their first request is seen to be a device
   list or an import of a device this session shares.

Attaching side (controlled):

1. Refuses unless the local option `allow-usb-redirection` is `Y` and the
   session has keyboard (control) permission. The option is off by default and
   is not implied by full access mode.
2. Opens a listener on `127.0.0.1:<random>` for the session.
3. Lists the controller's devices over the tunnel and checks the device's
   interface classes against `usb-redirection-deny` (default
   `hid-boot,net,wireless`; `none` refuses nothing; `hid`, `mass-storage` also
   accepted).
4. Runs `usbip --tcp-port <port> attach --remote 127.0.0.1 --bus-id <id>`
   (usbip-win2 on Windows with `--once`, usbip on Linux) and reports the hub
   port.
5. Detaches every device it attached when the session ends.

Attach requests are logged and posted to the connection audit (`usb_attach`,
`usb_detach`) when an audit server is configured.

## Dependencies (not bundled)

| Side | Windows | Linux |
|---|---|---|
| Sharing (controller) | [usbipd-win](https://github.com/dorssel/usbipd-win) (GPL-3.0) | `usbip` tools, `usbip-host` module, `usbipd` running |
| Attaching (controlled) | [usbip-win2](https://github.com/vadimgrn/usbip-win2) (BSD-2-Clause, WHLK-signed drivers) | `usbip` tools, `vhci-hcd` module |

usbipd-win opens TCP 3240 to the network and adds a firewall rule for it. This
feature only uses it through `127.0.0.1`; that rule can be removed.

macOS, Android, iOS and the web client cannot take part.

## Differences from RuDesktop

- Only the controller-to-controlled direction ("share my device with the
  remote computer"); RuDesktop also lets the controller import the remote
  computer's devices.
- Runs inside a normal remote session; no separate connection type.
- The listener binds `127.0.0.1`, not `0.0.0.0`.
- The controller only relays device-list and import requests for devices it
  shares in this session.
- Off by default on the controlled side; needs control permission; class deny list.
- External `usbip`/`usbipd` executables instead of a bundled FFI library and
  drivers.

## Known limits

- No flow control: frames are queued per stream (up to 1024 x 64 KiB) and a
  stream that overflows is closed. Fine for keys, card readers and dongles;
  bulk storage at full speed may hit it.
- The class check protects against mistakes, not against a hostile
  controller, which chooses every descriptor its USB/IP server reports.
  Vendor-specific (class 0xff) network adapters are not recognised.
- Composite security keys with a boot keyboard interface (for example YubiKey
  OTP) are refused by the default list; set `usb-redirection-deny` to
  `net,wireless` to allow them.
