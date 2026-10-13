//! Cranelift and Pulley run the same Wasm to the same bytes and the same errors.
//!
//! The fixture is `tests/test-contract-backend-conformance`; its first state
//! byte selects a normal run or a real trap (see that crate's docs).

use freenet_stdlib::prelude::*;

use super::super::contract::*;
use super::super::engine::Engine;
use super::super::error::RuntimeInnerError;
use super::super::runtime::ContractExecError;
use super::super::{Runtime, RuntimeConfig};
use super::TestSetup;
use crate::config::WasmBackend;

const CONTRACT: &str = "test_contract_backend_conformance";

fn available_backends() -> Vec<WasmBackend> {
    WasmBackend::ALL
        .into_iter()
        .filter(|backend| backend.is_available())
        .collect()
}

struct BackendRuntime {
    runtime: Runtime,
    key: ContractKey,
    _dir: tempfile::TempDir,
}

async fn runtime_on(
    backend: WasmBackend,
    cache_dir: Option<std::path::PathBuf>,
) -> Result<BackendRuntime, Box<dyn std::error::Error>> {
    let TestSetup {
        contract_store,
        delegate_store,
        secrets_store,
        contract_key,
        temp_dir,
    } = super::setup_test_contract(CONTRACT).await?;
    let config = RuntimeConfig {
        wasm_backend: backend,
        wasmtime_cache_dir: Some(cache_dir.unwrap_or_else(|| temp_dir.path().join("wasm-cache"))),
        ..Default::default()
    };
    let runtime =
        Runtime::build_with_config(contract_store, delegate_store, secrets_store, false, config)?;
    Ok(BackendRuntime {
        runtime,
        key: contract_key,
        _dir: temp_dir,
    })
}

/// The four contract functions on fixed inputs, as bytes, so two backends'
/// results compare directly.
fn round_trip(rt: &mut BackendRuntime) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let params = Parameters::from(Vec::<u8>::new());
    let state = WrappedState::new(vec![1, 2, 3]);
    let valid = rt
        .runtime
        .validate_state(&rt.key, &params, &state, &Default::default())?;
    assert_eq!(valid, ValidateResult::Valid);

    let modification = rt.runtime.update_state(
        &rt.key,
        &params,
        &state,
        &[UpdateData::Delta(StateDelta::from(vec![4, 5]))],
    )?;
    let next = modification
        .new_state
        .ok_or("update_state returned no new state")?;
    assert_eq!(next.as_ref(), &[1, 2, 3, 4, 5]);

    let full = WrappedState::new(next.as_ref().to_vec());
    let summary = rt.runtime.summarize_state(&rt.key, &params, &full)?;
    assert_eq!(summary.as_ref().len(), 12);
    assert_eq!(&summary.as_ref()[..4], &5u32.to_le_bytes());

    let old_summary = rt.runtime.summarize_state(&rt.key, &params, &state)?;
    let delta = rt
        .runtime
        .get_state_delta(&rt.key, &params, &full, &old_summary)?;
    assert_eq!(delta.as_ref(), &[4, 5]);

    Ok(vec![
        next.as_ref().to_vec(),
        summary.as_ref().to_vec(),
        old_summary.as_ref().to_vec(),
        delta.as_ref().to_vec(),
    ])
}

fn validate(
    rt: &mut BackendRuntime,
    state: Vec<u8>,
) -> Result<ValidateResult, super::super::ContractError> {
    rt.runtime.validate_state(
        &rt.key,
        &Parameters::from(Vec::<u8>::new()),
        &WrappedState::new(state),
        &Default::default(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn backends_return_the_same_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    for backend in available_backends() {
        let mut rt = runtime_on(backend, None).await?;
        results.push((backend, round_trip(&mut rt)?));
    }
    assert!(results.iter().any(|(b, _)| *b == WasmBackend::Pulley));
    let (first_backend, first) = &results[0];
    for (backend, bytes) in &results[1..] {
        assert_eq!(bytes, first, "{backend} differs from {first_backend}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn traps_keep_their_kind_and_the_runtime_recovers() -> Result<(), Box<dyn std::error::Error>>
{
    let cases: [(&str, Vec<u8>, &str); 3] = [
        (
            "out-of-bounds read",
            vec![0xF0],
            "out of bounds memory access",
        ),
        ("divide by zero", vec![0xF1, 0], "integer divide by zero"),
        ("panic", vec![0xF2], "unreachable"),
    ];
    for backend in available_backends() {
        let mut rt = runtime_on(backend, None).await?;
        for (name, state, expected) in &cases {
            let err = validate(&mut rt, state.clone())
                .expect_err(&format!("{backend}: {name} should trap"));
            assert!(
                matches!(err.deref(), RuntimeInnerError::WasmError(_)),
                "{backend}: {name} should be a Wasm runtime error, got {err:?}"
            );
            let message = err.to_string();
            assert!(
                message.contains(expected),
                "{backend}: {name} message should name the trap ({expected}), got: {message}"
            );
            // A trap poisons nothing: the next call on the same runtime runs.
            assert_eq!(validate(&mut rt, vec![1])?, ValidateResult::Valid);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn execution_limit_interrupts_every_backend() -> Result<(), Box<dyn std::error::Error>> {
    for backend in available_backends() {
        let mut rt = runtime_on(backend, None).await?;
        let started = std::time::Instant::now();
        let err = validate(&mut rt, vec![0xF3]).expect_err("an endless loop must be interrupted");
        assert!(
            matches!(
                err.deref(),
                RuntimeInnerError::ContractExecError(ContractExecError::MaxComputeTimeExceeded)
            ),
            "{backend}: expected MaxComputeTimeExceeded, got {err:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "{backend}: the 5 s limit took {:?}",
            started.elapsed()
        );
        assert_eq!(validate(&mut rt, vec![1])?, ValidateResult::Valid);
    }
    Ok(())
}

/// A module compiled by one backend's engine is refused by the other's, and a
/// compile cache shared by both never hands one backend the other's artifact.
#[tokio::test(flavor = "multi_thread")]
async fn backends_refuse_each_others_artifacts() -> Result<(), Box<dyn std::error::Error>> {
    let backends = available_backends();
    if backends.len() < 2 {
        return Ok(());
    }
    let wasm = super::get_test_module(CONTRACT)?;
    let engines = backends
        .iter()
        .map(|backend| {
            Engine::create_backend_engine(&RuntimeConfig {
                wasm_backend: *backend,
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (from, from_engine) in backends.iter().zip(&engines) {
        let artifact = wasmtime::Module::new(from_engine, &wasm)?.serialize()?;
        for (to, to_engine) in backends.iter().zip(&engines) {
            if from == to {
                continue;
            }
            // SAFETY: the bytes come from `Module::serialize` in this process,
            // so they are a well-formed wasmtime artifact. The call must refuse
            // them because they target another backend.
            let loaded = unsafe { wasmtime::Module::deserialize(to_engine, &artifact) };
            assert!(
                loaded.is_err(),
                "{to} loaded an artifact compiled for {from}"
            );
        }
    }

    let shared = tempfile::tempdir()?;
    let mut results = Vec::new();
    for _pass in 0..2 {
        for backend in &backends {
            let mut rt = runtime_on(*backend, Some(shared.path().to_path_buf())).await?;
            results.push(round_trip(&mut rt)?);
        }
    }
    assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
    Ok(())
}
