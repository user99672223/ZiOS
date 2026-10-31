//! Thermal state, Low Power Mode, and network addresses.

use std::net::IpAddr;

use crate::strict::{StrictConfig, ThermalOverride};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalState {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl ThermalState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Fair => "fair",
            Self::Serious => "serious",
            Self::Critical => "critical",
        }
    }
}

impl From<ThermalOverride> for ThermalState {
    fn from(value: ThermalOverride) -> Self {
        match value {
            ThermalOverride::Nominal => Self::Nominal,
            ThermalOverride::Fair => Self::Fair,
            ThermalOverride::Serious => Self::Serious,
            ThermalOverride::Critical => Self::Critical,
        }
    }
}

/// How much background work to allow (CLAUDE.md section 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    /// Pause background indexing and other non-essential work.
    pub pause_background_work: bool,
    /// Also stop idle language servers.
    pub stop_idle_servers: bool,
}

pub fn throttle(thermal: ThermalState, low_power: bool) -> Throttle {
    Throttle {
        pause_background_work: low_power || thermal >= ThermalState::Serious,
        stop_idle_servers: thermal >= ThermalState::Critical,
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn system_thermal_state() -> ThermalState {
    use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
    match NSProcessInfo::processInfo().thermalState() {
        NSProcessInfoThermalState::Fair => ThermalState::Fair,
        NSProcessInfoThermalState::Serious => ThermalState::Serious,
        NSProcessInfoThermalState::Critical => ThermalState::Critical,
        _ => ThermalState::Nominal,
    }
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn system_thermal_state() -> ThermalState {
    ThermalState::Nominal
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn system_low_power_mode() -> bool {
    objc2_foundation::NSProcessInfo::processInfo().isLowPowerModeEnabled()
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn system_low_power_mode() -> bool {
    false
}

/// The thermal state, honoring the strict-mode override.
pub fn thermal_state(config: &StrictConfig) -> ThermalState {
    config
        .thermal_override
        .map(ThermalState::from)
        .unwrap_or_else(system_thermal_state)
}

/// Low Power Mode, honoring the strict-mode override.
pub fn low_power_mode(config: &StrictConfig) -> bool {
    config
        .low_power_override
        .unwrap_or_else(system_low_power_mode)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    WiFi,
    /// USB/Thunderbolt Ethernet adapters and USB-C hubs.
    Ethernet,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface: String,
    pub kind: InterfaceKind,
    pub address: IpAddr,
}

impl InterfaceAddress {
    /// Link-local IPv4 (169.254/16), typical for a direct cable to a laptop.
    pub fn is_link_local(&self) -> bool {
        match self.address {
            IpAddr::V4(address) => address.is_link_local(),
            IpAddr::V6(address) => address.segments()[0] & 0xffc0 == 0xfe80,
        }
    }
}

/// On iPhone `en0` is Wi-Fi; wired adapters appear as further `en` interfaces.
/// Cellular (`pdp_ip`), VPN (`utun`, `ipsec`), loopback and Apple-internal
/// interfaces are not useful for lan-mouse and are skipped.
pub fn classify_interface(name: &str) -> Option<InterfaceKind> {
    if name == "en0" {
        Some(InterfaceKind::WiFi)
    } else if name.starts_with("en") {
        Some(InterfaceKind::Ethernet)
    } else if name.starts_with("bridge") {
        Some(InterfaceKind::Other)
    } else {
        None
    }
}

/// IPv4 and link-local IPv6 addresses of Wi-Fi and Ethernet interfaces.
#[cfg(unix)]
pub fn interface_addresses() -> Vec<InterfaceAddress> {
    use std::{
        ffi::CStr,
        net::{Ipv4Addr, Ipv6Addr},
    };

    let mut addresses = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        log::warn!("getifaddrs failed: {}", std::io::Error::last_os_error());
        return addresses;
    }
    let mut cursor = list;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_flags & libc::IFF_UP as u32 == 0 {
            continue;
        }
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let Some(kind) = classify_interface(&name) else {
            continue;
        };
        let family = i32::from(unsafe { (*entry.ifa_addr).sa_family });
        let address = if family == libc::AF_INET {
            let socket = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
            IpAddr::V4(Ipv4Addr::from(u32::from_be(socket.sin_addr.s_addr)))
        } else if family == libc::AF_INET6 {
            let socket = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in6>() };
            let address = Ipv6Addr::from(socket.sin6_addr.s6_addr);
            if address.segments()[0] & 0xffc0 != 0xfe80 {
                continue;
            }
            IpAddr::V6(address)
        } else {
            continue;
        };
        addresses.push(InterfaceAddress {
            interface: name,
            kind,
            address,
        });
    }
    unsafe { libc::freeifaddrs(list) };
    addresses.sort_by_key(|address| (address.interface.clone(), address.address.is_ipv6()));
    addresses
}

#[cfg(not(unix))]
pub fn interface_addresses() -> Vec<InterfaceAddress> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_follows_thermal_state_and_low_power() {
        assert_eq!(
            throttle(ThermalState::Fair, false),
            Throttle {
                pause_background_work: false,
                stop_idle_servers: false
            }
        );
        assert!(throttle(ThermalState::Serious, false).pause_background_work);
        assert!(!throttle(ThermalState::Serious, false).stop_idle_servers);
        assert!(throttle(ThermalState::Critical, false).stop_idle_servers);
        assert!(throttle(ThermalState::Nominal, true).pause_background_work);
    }

    #[test]
    fn overrides_win() {
        let config = StrictConfig::from_lookup(|name| match name {
            "ZED_IOS_THERMAL_STATE" => Some("critical".into()),
            "ZED_IOS_LOW_POWER_MODE" => Some("on".into()),
            _ => None,
        });
        assert_eq!(thermal_state(&config), ThermalState::Critical);
        assert!(low_power_mode(&config));
    }

    #[test]
    fn classifies_interfaces() {
        assert_eq!(classify_interface("en0"), Some(InterfaceKind::WiFi));
        assert_eq!(classify_interface("en5"), Some(InterfaceKind::Ethernet));
        assert_eq!(classify_interface("pdp_ip0"), None);
        assert_eq!(classify_interface("utun2"), None);
        assert_eq!(classify_interface("lo0"), None);
    }

    #[test]
    fn link_local_detection() {
        let address = |text: &str| InterfaceAddress {
            interface: "en5".into(),
            kind: InterfaceKind::Ethernet,
            address: text.parse().unwrap(),
        };
        assert!(address("169.254.10.2").is_link_local());
        assert!(address("fe80::1").is_link_local());
        assert!(!address("192.168.1.20").is_link_local());
    }

    #[cfg(unix)]
    #[test]
    fn enumerating_interfaces_does_not_fail() {
        // CI hosts may have no en* interfaces; this only exercises the FFI.
        let _addresses = interface_addresses();
    }
}
