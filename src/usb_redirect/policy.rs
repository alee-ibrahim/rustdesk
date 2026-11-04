//! Which USB devices the controlled side refuses to attach.
//!
//! This guards against mistakes, not against a hostile controller: the
//! controller runs the USB/IP server and so chooses every descriptor the
//! controlled side sees. It cannot see vendor-specific classes either, so a
//! network adapter that reports class 0xff passes.

use super::usbip_proto::{Device, Interface};

pub const DEFAULT_DENY: &str = "hid-boot,net,wireless";

const HID: u8 = 0x03;
const CDC: u8 = 0x02;
const WIRELESS: u8 = 0xe0;
const MISC: u8 = 0xef;
const MASS_STORAGE: u8 = 0x08;

/// CDC subclasses that carry network traffic: ECM, ATM, EEM, NCM, MBIM.
const CDC_NET_SUBCLASSES: &[u8] = &[0x06, 0x07, 0x0c, 0x0d, 0x0e];

fn matches(rule: &str, i: &Interface) -> bool {
    match rule {
        // Boot keyboards and mice. Composite security keys often carry one
        // too (YubiKey OTP), so they are refused under the default.
        "hid-boot" => i.class == HID && i.subclass == 1 && (i.protocol == 1 || i.protocol == 2),
        "hid" => i.class == HID,
        "net" => {
            (i.class == CDC && CDC_NET_SUBCLASSES.contains(&i.subclass))
                // RNDIS
                || (i.class == WIRELESS && i.subclass == 1 && i.protocol == 3)
                || (i.class == MISC && i.subclass == 4 && i.protocol == 1)
        }
        "wireless" => i.class == WIRELESS,
        "mass-storage" => i.class == MASS_STORAGE,
        _ => false,
    }
}

/// `deny` is the option value; empty means `DEFAULT_DENY`, "none" refuses nothing.
/// Returns why `dev` is refused, if it is.
pub fn denied(dev: &Device, deny: &str) -> Option<String> {
    let deny = deny.trim();
    let deny = if deny.is_empty() { DEFAULT_DENY } else { deny };
    if deny == "none" {
        return None;
    }
    // A device-level class stands in when it is set; 0 defers to interfaces.
    let mut interfaces = dev.interfaces.clone();
    if dev.class != 0 {
        interfaces.push(Interface {
            class: dev.class,
            subclass: dev.subclass,
            protocol: dev.protocol,
        });
    }
    if interfaces.is_empty() {
        return Some("the USB/IP server did not report the device's interfaces".to_owned());
    }
    for rule in deny.split(',').map(str::trim).filter(|r| !r.is_empty()) {
        if let Some(i) = interfaces.iter().find(|i| matches(rule, i)) {
            return Some(format!(
                "device class {:02x}:{:02x}:{:02x} is refused by the \"{}\" rule",
                i.class, i.subclass, i.protocol, rule
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(interfaces: &[(u8, u8, u8)]) -> Device {
        Device {
            busid: "1-1".to_owned(),
            vendor_id: 0,
            product_id: 0,
            class: 0,
            subclass: 0,
            protocol: 0,
            interfaces: interfaces
                .iter()
                .map(|&(class, subclass, protocol)| Interface {
                    class,
                    subclass,
                    protocol,
                })
                .collect(),
        }
    }

    #[test]
    fn default_refuses_keyboards_and_network_adapters() {
        assert!(denied(&dev(&[(3, 1, 1)]), "").is_some());
        assert!(denied(&dev(&[(2, 6, 0), (0x0a, 0, 0)]), "").is_some());
        assert!(denied(&dev(&[(0xe0, 1, 1)]), "").is_some());
    }

    #[test]
    fn default_lets_smart_cards_and_storage_through() {
        assert!(denied(&dev(&[(0x0b, 0, 0)]), "").is_none());
        assert!(denied(&dev(&[(8, 6, 0x50)]), "").is_none());
        assert!(denied(&dev(&[(3, 0, 0)]), "").is_none());
    }

    #[test]
    fn none_and_custom_lists() {
        assert!(denied(&dev(&[(3, 1, 1)]), "none").is_none());
        assert!(denied(&dev(&[(8, 6, 0x50)]), "mass-storage").is_some());
        assert!(denied(&dev(&[]), "").is_some());
    }
}
