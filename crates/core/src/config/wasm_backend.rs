//! Which wasmtime backend runs contract and delegate Wasm.
//!
//! Cranelift compiles Wasm to native machine code, which the process then maps
//! as executable memory. Pulley is wasmtime's portable interpreter: Cranelift
//! compiles Wasm to Pulley bytecode and wasmtime interprets it, so no memory is
//! ever mapped executable. iOS apps may not map executable memory, so iOS runs
//! Pulley. So do the targets Cranelift has no native backend for (32-bit ARM
//! and 32-bit x86).
//!
//! Both backends run the same Wasm and must return the same bytes and errors.
//! Wall-clock limits are the one expected difference: interpreted code runs
//! slower, so a contract close to the execution-time limit under Cranelift can
//! exceed it under Pulley.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WasmBackend {
    /// Compile Wasm to native code with Cranelift.
    Cranelift,
    /// Interpret Wasm with wasmtime's Pulley interpreter.
    Pulley,
}

impl Default for WasmBackend {
    fn default() -> Self {
        Self::default_for_target()
    }
}

impl WasmBackend {
    /// Every backend, in a fixed order.
    pub const ALL: [WasmBackend; 2] = [WasmBackend::Cranelift, WasmBackend::Pulley];

    /// Cranelift where the target can run it, Pulley everywhere else.
    pub const fn default_for_target() -> Self {
        if Self::Cranelift.is_available() {
            Self::Cranelift
        } else {
            Self::Pulley
        }
    }

    /// Whether this backend can run on the target this binary was built for.
    ///
    /// iOS covers the simulator too, so simulator runs match the device.
    pub const fn is_available(self) -> bool {
        match self {
            Self::Cranelift => {
                !cfg!(target_os = "ios") && !cfg!(target_arch = "arm") && !cfg!(target_arch = "x86")
            }
            Self::Pulley => true,
        }
    }

    /// The wasmtime target name for Pulley bytecode that this host can run.
    pub const fn pulley_target() -> &'static str {
        match (
            cfg!(target_pointer_width = "64"),
            cfg!(target_endian = "big"),
        ) {
            (true, false) => "pulley64",
            (true, true) => "pulley64be",
            (false, false) => "pulley32",
            (false, true) => "pulley32be",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cranelift => "cranelift",
            Self::Pulley => "pulley",
        }
    }
}

impl std::fmt::Display for WasmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulley_is_available_everywhere() {
        assert!(WasmBackend::Pulley.is_available());
    }

    #[test]
    fn default_backend_is_available() {
        assert!(WasmBackend::default_for_target().is_available());
    }

    #[test]
    fn serde_names_are_lowercase() {
        assert_eq!(
            serde_json::to_string(&WasmBackend::Pulley).unwrap(),
            "\"pulley\""
        );
        assert_eq!(
            serde_json::from_str::<WasmBackend>("\"cranelift\"").unwrap(),
            WasmBackend::Cranelift
        );
    }
}
