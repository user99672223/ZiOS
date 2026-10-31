//! Strict Simulator mode configuration (CLAUDE.md section 12a).
//!
//! The Simulator is a macOS process and allows things an iPhone forbids. Strict
//! mode is compiled into Simulator builds only (the `strict` feature) and makes
//! those things fail loudly. The process-spawn and sandbox checks live in the
//! interposer dylib (`ios/Strict/zios_strict.c`); this module holds the settings
//! that Rust code consults.

use std::str::FromStr;

/// Whether this binary was built with strict Simulator mode.
pub const ENABLED: bool = cfg!(feature = "strict");

#[cfg(all(
    feature = "strict",
    target_os = "ios",
    not(any(target_env = "sim", target_abi = "sim"))
))]
compile_error!("strict Simulator mode must never be compiled into device builds");

#[cfg(all(feature = "strict", target_os = "ios", target_vendor = "apple"))]
mod interposer {
    unsafe extern "C" {
        fn zios_strict_interposer_loaded() -> std::ffi::c_int;
        fn zios_strict_allow_root(root: *const std::ffi::c_char);
    }

    pub fn loaded() -> bool {
        unsafe { zios_strict_interposer_loaded() == 1 }
    }

    pub fn allow_root(root: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt as _;
        match std::ffi::CString::new(root.as_os_str().as_bytes()) {
            Ok(root) => unsafe { zios_strict_allow_root(root.as_ptr()) },
            Err(error) => log::error!("cannot allow sandbox root {}: {error}", root.display()),
        }
    }
}

/// Whether the spawn/sandbox interposer is active in this process.
pub fn interposer_loaded() -> bool {
    #[cfg(all(feature = "strict", target_os = "ios", target_vendor = "apple"))]
    {
        interposer::loaded()
    }
    #[cfg(not(all(feature = "strict", target_os = "ios", target_vendor = "apple")))]
    {
        false
    }
}

/// Marks a folder the user picked as reachable, as the device sandbox would after
/// the document picker grants access.
pub fn allow_sandbox_root(root: &std::path::Path) {
    #[cfg(all(feature = "strict", target_os = "ios", target_vendor = "apple"))]
    interposer::allow_root(root);
    #[cfg(not(all(feature = "strict", target_os = "ios", target_vendor = "apple")))]
    let _ = root;
}

/// `ZED_IOS_FORCE_JIT=off|on|auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForceJit {
    /// Act as if JIT were unavailable (Wasmtime on Pulley, Node `--jitless`).
    Off,
    /// Act as if JIT were available.
    On,
    /// Use real detection.
    #[default]
    Auto,
}

impl FromStr for ForceJit {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" => Ok(Self::Off),
            "on" | "1" | "true" => Ok(Self::On),
            "auto" | "" => Ok(Self::Auto),
            other => Err(format!("ZED_IOS_FORCE_JIT must be off, on, or auto, not {other:?}")),
        }
    }
}

/// Simulated thermal state, used to exercise throttling in CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalOverride {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl FromStr for ThermalOverride {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "nominal" => Ok(Self::Nominal),
            "fair" => Ok(Self::Fair),
            "serious" => Ok(Self::Serious),
            "critical" => Ok(Self::Critical),
            other => Err(format!("unknown thermal state {other:?}")),
        }
    }
}

/// iPhone 15 (6 GB) per-app memory limits used as the default simulated caps.
/// See docs/DECISIONS.md for the sources and their uncertainty.
pub const IPHONE_15_LIMIT_MB: u64 = 3072;
pub const IPHONE_15_INCREASED_LIMIT_MB: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictConfig {
    pub force_jit: ForceJit,
    /// Simulated jetsam limit. `None` disables the watchdog.
    pub memory_cap_mb: Option<u64>,
    pub thermal_override: Option<ThermalOverride>,
    pub low_power_override: Option<bool>,
}

impl StrictConfig {
    /// Reads the configuration from the environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Parses the configuration from any variable source. Invalid values are
    /// logged and replaced by defaults so a typo never changes behavior silently.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let force_jit = lookup("ZED_IOS_FORCE_JIT")
            .map(|value| {
                value.parse().unwrap_or_else(|error: String| {
                    log::error!("{error}; using auto");
                    ForceJit::Auto
                })
            })
            .unwrap_or_default();

        let memory_cap_mb = match lookup("ZED_IOS_MEMORY_CAP_MB") {
            Some(value) if matches!(value.trim(), "0" | "off" | "none") => None,
            Some(value) => match value.trim().parse::<u64>() {
                Ok(megabytes) => Some(megabytes),
                Err(error) => {
                    log::error!("invalid ZED_IOS_MEMORY_CAP_MB {value:?}: {error}");
                    ENABLED.then_some(IPHONE_15_LIMIT_MB)
                }
            },
            // Strict builds always enforce the device's real limit by default.
            None => ENABLED.then_some(IPHONE_15_LIMIT_MB),
        };

        let thermal_override = lookup("ZED_IOS_THERMAL_STATE").and_then(|value| {
            value
                .parse()
                .map_err(|error: String| log::error!("{error}"))
                .ok()
        });

        let low_power_override = lookup("ZED_IOS_LOW_POWER_MODE").and_then(|value| {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "on" | "true" => Some(true),
                "0" | "off" | "false" => Some(false),
                other => {
                    log::error!("invalid ZED_IOS_LOW_POWER_MODE {other:?}");
                    None
                }
            }
        });

        Self {
            force_jit,
            memory_cap_mb,
            thermal_override,
            low_power_override,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(variables: &[(&str, &str)]) -> StrictConfig {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        StrictConfig::from_lookup(|name| variables.get(name).cloned())
    }

    #[test]
    fn parses_force_jit() {
        assert_eq!(config(&[]).force_jit, ForceJit::Auto);
        assert_eq!(config(&[("ZED_IOS_FORCE_JIT", "off")]).force_jit, ForceJit::Off);
        assert_eq!(config(&[("ZED_IOS_FORCE_JIT", "ON")]).force_jit, ForceJit::On);
        assert_eq!(config(&[("ZED_IOS_FORCE_JIT", "bogus")]).force_jit, ForceJit::Auto);
    }

    #[test]
    fn parses_memory_cap() {
        assert_eq!(
            config(&[("ZED_IOS_MEMORY_CAP_MB", "4096")]).memory_cap_mb,
            Some(4096)
        );
        assert_eq!(config(&[("ZED_IOS_MEMORY_CAP_MB", "off")]).memory_cap_mb, None);
        let default = config(&[]).memory_cap_mb;
        if ENABLED {
            assert_eq!(default, Some(IPHONE_15_LIMIT_MB));
        } else {
            assert_eq!(default, None);
        }
    }

    #[test]
    fn parses_thermal_and_low_power_overrides() {
        let parsed = config(&[
            ("ZED_IOS_THERMAL_STATE", "serious"),
            ("ZED_IOS_LOW_POWER_MODE", "1"),
        ]);
        assert_eq!(parsed.thermal_override, Some(ThermalOverride::Serious));
        assert_eq!(parsed.low_power_override, Some(true));
        assert_eq!(config(&[("ZED_IOS_THERMAL_STATE", "hot")]).thermal_override, None);
    }
}
