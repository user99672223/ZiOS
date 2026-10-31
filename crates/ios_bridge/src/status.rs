//! A snapshot of everything the Device Status panel and diagnostics report
//! (CLAUDE.md sections 9.3 and 12b).

use std::sync::OnceLock;

use crate::{
    device::{self, InterfaceAddress, ThermalState},
    entitlements::{self, EntitlementReport, Grant},
    jit::{self, JitStatus},
    memory,
    strict::{self, StrictConfig},
};

/// The one line shown when JIT is off.
pub const NO_JIT_EXPLANATION: &str = "Without JIT, C/C++ (clangd), TypeScript type checking, and \
    installing extensions are slower. Everything else works normally.";

/// Free memory below this is treated as critical.
const LOW_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub jit: JitStatus,
    pub entitlements: EntitlementReport,
    pub footprint: Option<u64>,
    pub available: Option<u64>,
    pub thermal: ThermalState,
    pub low_power: bool,
    pub addresses: Vec<InterfaceAddress>,
}

impl Snapshot {
    pub fn collect() -> Self {
        Self::collect_with(strict::config())
    }

    pub fn collect_with(config: &StrictConfig) -> Self {
        static ENTITLEMENTS: OnceLock<EntitlementReport> = OnceLock::new();
        Self {
            jit: jit::current(),
            entitlements: ENTITLEMENTS.get_or_init(entitlements::current_report).clone(),
            footprint: memory::footprint_bytes(),
            available: memory::available_bytes(),
            thermal: device::thermal_state(config),
            low_power: device::low_power_mode(config),
            addresses: device::interface_addresses(),
        }
    }

    /// Mode used by Wasm modules compiled from now on.
    pub fn wasmtime_mode(&self) -> &'static str {
        if self.jit.enabled() {
            "JIT (Cranelift)"
        } else {
            "Interpreter (Pulley)"
        }
    }

    /// Mode used by Node instances started from now on.
    pub fn node_mode(&self) -> &'static str {
        if self.jit.enabled() { "JIT" } else { "JIT-less" }
    }

    pub fn severity(&self) -> Severity {
        if self.thermal >= ThermalState::Critical
            || self
                .available
                .is_some_and(|available| available < LOW_MEMORY_BYTES)
        {
            Severity::Critical
        } else if !self.jit.enabled()
            || self.entitlements.increased_memory_limit == Grant::NotGranted
            || self.thermal >= ThermalState::Serious
            || self.low_power
        {
            Severity::Warning
        } else {
            Severity::Ok
        }
    }

    pub fn grant_label(grant: Grant) -> &'static str {
        match grant {
            Grant::Granted => "Granted",
            Grant::NotGranted => "Not granted",
            Grant::Unknown => "Unknown (unsigned build)",
        }
    }

    /// Plain-text form for Copy Diagnostics Summary and exported logs.
    pub fn to_text(&self) -> String {
        let bytes = |value: Option<u64>| value.map_or_else(|| "unknown".to_string(), memory::format_bytes);
        let mut text = String::new();
        let jit = &self.jit;
        text.push_str(&format!(
            "JIT: {} (method {}{})\n",
            if jit.enabled() { "Enabled" } else { "Not enabled" },
            jit.method.label(),
            jit.forced.map_or_else(String::new, |force| format!(", forced {force:?}")),
        ));
        text.push_str(&format!("Wasmtime: {}\nNode: {}\n", self.wasmtime_mode(), self.node_mode()));
        text.push_str(&format!(
            "Increased Memory Limit: {}\nMemory: {} available, app uses {}\n",
            Self::grant_label(self.entitlements.increased_memory_limit),
            bytes(self.available),
            bytes(self.footprint),
        ));
        text.push_str(&format!(
            "Thermal: {}; Low Power Mode: {}\n",
            self.thermal.label(),
            if self.low_power { "on" } else { "off" }
        ));
        text.push_str(&format!(
            "Device: {} iOS {}\n",
            jit.machine,
            jit.os_version
                .map_or_else(|| "unknown".to_string(), |version| version.to_string())
        ));
        for address in &self.addresses {
            text.push_str(&format!(
                "Network {} ({:?}): {}\n",
                address.interface, address.kind, address.address
            ));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::{JitMethod, OsVersion};

    fn snapshot(method: JitMethod) -> Snapshot {
        Snapshot {
            jit: JitStatus {
                debugged: method != JitMethod::None,
                method,
                forced: None,
                os_version: OsVersion::parse("26.1"),
                machine: "iPhone15,4".into(),
                has_txm: true,
            },
            entitlements: EntitlementReport {
                increased_memory_limit: Grant::Granted,
                get_task_allow: Grant::Granted,
                has_provisioning_profile: true,
            },
            footprint: Some(400 * 1024 * 1024),
            available: Some(2 * 1024 * 1024 * 1024),
            thermal: ThermalState::Nominal,
            low_power: false,
            addresses: Vec::new(),
        }
    }

    #[test]
    fn severity() {
        assert_eq!(snapshot(JitMethod::Txm).severity(), Severity::Ok);
        assert_eq!(snapshot(JitMethod::None).severity(), Severity::Warning);
        let mut hot = snapshot(JitMethod::Txm);
        hot.thermal = ThermalState::Critical;
        assert_eq!(hot.severity(), Severity::Critical);
        let mut low = snapshot(JitMethod::Txm);
        low.available = Some(100 * 1024 * 1024);
        assert_eq!(low.severity(), Severity::Critical);
        let mut unentitled = snapshot(JitMethod::Txm);
        unentitled.entitlements.increased_memory_limit = Grant::NotGranted;
        assert_eq!(unentitled.severity(), Severity::Warning);
    }

    #[test]
    fn modes_follow_jit() {
        assert_eq!(snapshot(JitMethod::DualMapping).wasmtime_mode(), "JIT (Cranelift)");
        assert_eq!(snapshot(JitMethod::None).wasmtime_mode(), "Interpreter (Pulley)");
        assert_eq!(snapshot(JitMethod::None).node_mode(), "JIT-less");
    }

    #[test]
    fn text_summary_mentions_everything() {
        let text = snapshot(JitMethod::Txm).to_text();
        for expected in ["JIT: Enabled (method TXM)", "Wasmtime: JIT (Cranelift)", "Granted", "Thermal: nominal", "iPhone15,4 iOS 26.1.0"] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
    }
}
