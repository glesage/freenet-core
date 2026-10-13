//! Wasm backend conformance, run on the device itself.
//!
//! The same fixture contract (`tests/test-contract-backend-conformance`, built
//! into `fixtures/backend-conformance.wasm`) runs on every backend the target
//! has. Each backend must return the expected bytes, turn real traps into typed
//! runtime errors without disturbing the next call, and every backend the
//! target cannot run must be refused with an error instead of crashing.

use std::time::Instant;

use freenet::config::WasmBackend;
use freenet::conformance::{ConformanceOracle, OracleErrorKind, RuntimeOracle};
use freenet_stdlib::prelude::{StateDelta, UpdateData, ValidateResult};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::runtime;

/// The fixture contract, built from `tests/test-contract-backend-conformance`.
pub const FIXTURE_WASM: &[u8] = include_bytes!("../fixtures/backend-conformance.wasm");

/// The fixture contract's Wasm, for native routes that store it themselves.
#[uniffi::export]
pub fn fixture_contract_wasm() -> Vec<u8> {
    FIXTURE_WASM.to_vec()
}

#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct ConformanceCase {
    pub name: String,
    pub backend: String,
    pub passed: bool,
    pub detail: String,
    pub elapsed_ms: f64,
}

/// Extra real-world contract code to compile on every backend, such as
/// River's room contract.
#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct NamedModule {
    pub name: String,
    #[serde(skip)]
    pub wasm: Vec<u8>,
    #[serde(skip)]
    pub parameters: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct ConformanceReport {
    pub target: String,
    pub default_backend: String,
    pub available_backends: Vec<String>,
    pub fixture_sha256: String,
    pub cases: Vec<ConformanceCase>,
    pub passed: bool,
}

/// Run the suite. `include_timeout` adds the execution-limit case, which takes
/// about five seconds per backend.
#[uniffi::export]
pub async fn run_backend_conformance(
    extra_modules: Vec<NamedModule>,
    include_timeout: bool,
) -> ConformanceReport {
    let result = runtime::run(async move {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || run_blocking(&handle, &extra_modules, include_timeout))
            .await
            .map_err(crate::error::MobileError::internal)
    })
    .await;
    result.unwrap_or_else(|err| ConformanceReport {
        target: env!("FREENET_MOBILE_TARGET").into(),
        default_backend: WasmBackend::default_for_target().to_string(),
        available_backends: vec![],
        fixture_sha256: hex::encode(Sha256::digest(FIXTURE_WASM)),
        cases: vec![ConformanceCase {
            name: "suite".into(),
            backend: "-".into(),
            passed: false,
            detail: err.to_string(),
            elapsed_ms: 0.0,
        }],
        passed: false,
    })
}

struct Recorder {
    cases: Vec<ConformanceCase>,
}

impl Recorder {
    fn record(
        &mut self,
        name: &str,
        backend: &str,
        started: Instant,
        outcome: Result<String, String>,
    ) {
        let (passed, detail) = match outcome {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        self.cases.push(ConformanceCase {
            name: name.into(),
            backend: backend.into(),
            passed,
            detail,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        });
    }
}

fn build(
    handle: &tokio::runtime::Handle,
    wasm: &[u8],
    parameters: &[u8],
    backend: WasmBackend,
) -> Result<RuntimeOracle, String> {
    handle
        .block_on(RuntimeOracle::standalone_with_backend(
            wasm.to_vec(),
            parameters.to_vec(),
            backend,
        ))
        .map_err(|e| e.to_string())
}

fn expect_valid(oracle: &mut RuntimeOracle, state: &[u8]) -> Result<(), String> {
    match oracle.validate_state(state, &Default::default()) {
        Ok(ValidateResult::Valid) => Ok(()),
        Ok(other) => Err(format!("expected Valid, got {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// validate, update, summarize and delta on fixed input, as hex.
fn round_trip(oracle: &mut RuntimeOracle) -> Result<Vec<String>, String> {
    expect_valid(oracle, &[1, 2, 3])?;
    let modification = oracle
        .update_state(
            &[1, 2, 3],
            &[UpdateData::Delta(StateDelta::from(vec![4, 5]))],
        )
        .map_err(|e| e.to_string())?;
    let next = modification
        .new_state
        .ok_or("update_state returned no new state")?
        .as_ref()
        .to_vec();
    if next != [1, 2, 3, 4, 5] {
        return Err(format!("update_state returned {next:?}"));
    }
    let summary = oracle.summarize_state(&next).map_err(|e| e.to_string())?;
    let old_summary = oracle
        .summarize_state(&[1, 2, 3])
        .map_err(|e| e.to_string())?;
    let delta = oracle
        .get_state_delta(&next, &old_summary)
        .map_err(|e| e.to_string())?;
    if delta != [4, 5] {
        return Err(format!("get_state_delta returned {delta:?}"));
    }
    Ok(vec![
        hex::encode(&next),
        hex::encode(&summary),
        hex::encode(&old_summary),
        hex::encode(&delta),
    ])
}

fn expect_trap(oracle: &mut RuntimeOracle, state: &[u8], needle: &str) -> Result<String, String> {
    match oracle.validate_state(state, &Default::default()) {
        Ok(result) => Err(format!("expected a trap, got {result:?}")),
        Err(err) if err.kind == OracleErrorKind::Runtime && err.message.contains(needle) => {
            expect_valid(oracle, &[1])
                .map_err(|e| format!("the call after the trap failed: {e}"))?;
            Ok(format!("trapped with {needle:?}, next call ran"))
        }
        Err(err) => Err(format!(
            "expected a runtime error naming {needle:?}, got {err}"
        )),
    }
}

fn run_blocking(
    handle: &tokio::runtime::Handle,
    extra_modules: &[NamedModule],
    include_timeout: bool,
) -> ConformanceReport {
    let mut rec = Recorder { cases: Vec::new() };
    let available: Vec<WasmBackend> = WasmBackend::ALL
        .into_iter()
        .filter(|b| b.is_available())
        .collect();
    let mut round_trips: Vec<(WasmBackend, Vec<String>)> = Vec::new();

    for backend in &available {
        let name = backend.as_str();
        let started = Instant::now();
        let mut oracle = match build(handle, FIXTURE_WASM, &[], *backend) {
            Ok(oracle) => {
                rec.record("compile", name, started, Ok("fixture compiled".into()));
                oracle
            }
            Err(e) => {
                rec.record("compile", name, started, Err(e));
                continue;
            }
        };

        let started = Instant::now();
        let outcome = round_trip(&mut oracle);
        if let Ok(bytes) = &outcome {
            round_trips.push((*backend, bytes.clone()));
        }
        rec.record("round_trip", name, started, outcome.map(|b| b.join(" ")));

        for (case, state, needle) in [
            (
                "trap_out_of_bounds",
                vec![0xF0],
                "out of bounds memory access",
            ),
            (
                "trap_divide_by_zero",
                vec![0xF1, 0],
                "integer divide by zero",
            ),
            ("trap_unreachable", vec![0xF2], "unreachable"),
        ] {
            let started = Instant::now();
            let outcome = expect_trap(&mut oracle, &state, needle);
            rec.record(case, name, started, outcome);
        }

        let mut compute_state = vec![0xE0];
        compute_state.extend_from_slice(&20_000u32.to_le_bytes());
        compute_state.extend_from_slice(&[0x5A; 59]);
        let started = Instant::now();
        let outcome = expect_valid(&mut oracle, &compute_state)
            .map(|()| "hashed 64 bytes 20000 times".to_string());
        rec.record("compute", name, started, outcome);

        let started = Instant::now();
        let outcome =
            expect_valid(&mut oracle, &[0xE1, 16]).map(|()| "allocated 16 MiB".to_string());
        rec.record("memory_16_mib", name, started, outcome);

        if include_timeout {
            let started = Instant::now();
            let outcome = match oracle.validate_state(&[0xF3], &Default::default()) {
                Err(err) if err.kind == OracleErrorKind::Resource => {
                    expect_valid(&mut oracle, &[1])
                        .map(|()| format!("interrupted: {}", err.message))
                }
                Err(err) => Err(format!("expected a resource error, got {err}")),
                Ok(result) => Err(format!("an endless loop returned {result:?}")),
            };
            rec.record("execution_limit", name, started, outcome);
        }

        for module in extra_modules {
            let started = Instant::now();
            let outcome = build(handle, &module.wasm, &module.parameters, *backend)
                .map(|_| format!("{} bytes compiled", module.wasm.len()));
            rec.record(&format!("compile_{}", module.name), name, started, outcome);
        }
    }

    if round_trips.len() >= 2 {
        let started = Instant::now();
        let (first_backend, first) = &round_trips[0];
        let outcome = round_trips[1..]
            .iter()
            .find(|(_, bytes)| bytes != first)
            .map(|(backend, _)| Err(format!("{backend} differs from {first_backend}")))
            .unwrap_or_else(|| Ok("identical bytes on every backend".into()));
        rec.record("same_bytes", "all", started, outcome);

        let started = Instant::now();
        rec.record(
            "artifact_refusal",
            "all",
            started,
            artifact_refusal(&available),
        );
    }

    for backend in WasmBackend::ALL.into_iter().filter(|b| !b.is_available()) {
        let started = Instant::now();
        let outcome = match build(handle, FIXTURE_WASM, &[], backend) {
            Ok(_) => Err(format!("{backend} started on a target that cannot run it")),
            Err(e) if e.contains("not available") => Ok(e),
            Err(e) => Err(format!("refused, but not as unavailable: {e}")),
        };
        rec.record("refuse_unavailable", backend.as_str(), started, outcome);
    }

    let passed = !rec.cases.is_empty() && rec.cases.iter().all(|c| c.passed);
    ConformanceReport {
        target: env!("FREENET_MOBILE_TARGET").into(),
        default_backend: WasmBackend::default_for_target().to_string(),
        available_backends: available.iter().map(|b| b.to_string()).collect(),
        fixture_sha256: hex::encode(Sha256::digest(FIXTURE_WASM)),
        cases: rec.cases,
        passed,
    }
}

/// A module one backend compiled must not load into another backend's engine.
fn artifact_refusal(available: &[WasmBackend]) -> Result<String, String> {
    let engine = |backend: WasmBackend| -> Result<wasmtime::Engine, String> {
        let mut config = wasmtime::Config::new();
        if backend == WasmBackend::Pulley {
            config
                .target(WasmBackend::pulley_target())
                .map_err(|e| e.to_string())?;
        }
        if cfg!(any(target_os = "ios", target_os = "android")) {
            config.signals_based_traps(false);
        }
        wasmtime::Engine::new(&config).map_err(|e| e.to_string())
    };
    let engines = available
        .iter()
        .map(|b| engine(*b).map(|e| (*b, e)))
        .collect::<Result<Vec<_>, _>>()?;
    for (from, from_engine) in &engines {
        let artifact = wasmtime::Module::new(from_engine, FIXTURE_WASM)
            .and_then(|m| m.serialize())
            .map_err(|e| e.to_string())?;
        for (to, to_engine) in &engines {
            if from == to {
                continue;
            }
            // SAFETY: the bytes come from `Module::serialize` in this process,
            // so they are a well-formed wasmtime artifact; the call must refuse
            // them because they target another backend.
            if unsafe { wasmtime::Module::deserialize(to_engine, &artifact) }.is_ok() {
                return Err(format!("{to} loaded an artifact compiled for {from}"));
            }
        }
    }
    Ok("each backend refused the others' artifacts".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_passes_on_the_build_host() {
        let report = runtime::block_on(run_backend_conformance(vec![], false));
        for case in &report.cases {
            assert!(
                case.passed,
                "{} on {}: {}",
                case.name, case.backend, case.detail
            );
        }
        assert!(report.passed);
        assert!(report.available_backends.contains(&"pulley".to_string()));
    }
}
