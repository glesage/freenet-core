//! Cranelift-vs-Pulley conformance.
//!
//! iOS forbids JIT, so the mobile embedding runs contracts on wasmtime's Pulley
//! interpreter (`RuntimeConfig::use_pulley`). The contract ABI must not care
//! which backend executed it: the same inputs have to produce byte-identical
//! outputs on both, or a phone and a desktop would disagree about a contract's
//! state. The conformance test builds one runtime per backend in the same
//! process and compares every contract entry point over the same vectors the
//! backend-agnostic `tests::contract` suite uses.

use freenet_stdlib::prelude::*;

use super::{TestSetup, setup_test_contract};
use crate::wasm_runtime::engine::Engine;
use crate::wasm_runtime::runtime::RuntimeConfig;
use crate::wasm_runtime::{ContractError, ContractRuntimeInterface, Runtime};

const TEST_CONTRACT_1: &str = "test_contract_1";

/// One runtime over a fresh store holding `TEST_CONTRACT_1`, on the requested
/// backend. The temp dir is returned so it outlives the runtime.
async fn runtime_for(
    use_pulley: bool,
) -> Result<(Runtime, ContractKey, tempfile::TempDir), Box<dyn std::error::Error>> {
    let TestSetup {
        contract_store,
        delegate_store,
        secrets_store,
        contract_key,
        temp_dir,
    } = setup_test_contract(TEST_CONTRACT_1).await?;
    let runtime = Runtime::build_with_config(
        contract_store,
        delegate_store,
        secrets_store,
        false,
        RuntimeConfig {
            use_pulley,
            ..RuntimeConfig::default()
        },
    )?;
    Ok((runtime, contract_key, temp_dir))
}

/// Every contract entry point's output for one runtime, in a shape that can be
/// compared across backends with a plain `assert_eq!`.
#[derive(Debug, PartialEq, Eq)]
struct Outcomes {
    validate_valid: String,
    validate_invalid: String,
    updated: Vec<u8>,
    summary: Vec<u8>,
    delta: Vec<u8>,
}

fn run_vectors(runtime: &mut Runtime, key: &ContractKey) -> Result<Outcomes, ContractError> {
    let params = Parameters::from([].as_ref());
    let validate_valid = runtime.validate_state(
        key,
        &params,
        &WrappedState::new(vec![1, 2, 3, 4]),
        &Default::default(),
    )?;
    let validate_invalid = runtime.validate_state(
        key,
        &params,
        &WrappedState::new(vec![1, 0, 0, 1]),
        &Default::default(),
    )?;
    let updated = runtime
        .update_state(
            key,
            &params,
            &WrappedState::new(vec![5, 2, 3]),
            &[StateDelta::from([4].as_ref()).into()],
        )?
        .unwrap_valid();
    let summary = runtime.summarize_state(key, &params, &WrappedState::new(vec![5, 2, 3, 4]))?;
    let delta = runtime.get_state_delta(
        key,
        &params,
        &WrappedState::new(vec![5, 2, 3, 4]),
        &StateSummary::from([2, 3].as_ref()),
    )?;
    Ok(Outcomes {
        validate_valid: format!("{validate_valid:?}"),
        validate_invalid: format!("{validate_invalid:?}"),
        updated: updated.as_ref().to_vec(),
        summary: summary.as_ref().to_vec(),
        delta: delta.as_ref().to_vec(),
    })
}

/// The Pulley profile must actually select the interpreter target, otherwise
/// the whole feature is a no-op that would JIT (and crash) on iOS. wasmtime
/// keeps the engine's target private, but a serialized module carries the
/// compiler's target triple in its metadata header, so compile a trivial module
/// on each engine and look for the triple there.
#[test]
fn pulley_profile_targets_the_interpreter() -> Result<(), Box<dyn std::error::Error>> {
    fn serialized_module(use_pulley: bool) -> Result<Vec<u8>, ContractError> {
        let engine = Engine::create_backend_engine(&RuntimeConfig {
            use_pulley,
            ..RuntimeConfig::default()
        })?;
        let module = wasmtime::Module::new(&engine, b"(module)").expect("trivial module compiles");
        Ok(module.serialize().expect("module serializes"))
    }
    fn mentions(bytes: &[u8], needle: &str) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }
    let jit_bytes = serialized_module(false)?;
    let pulley_bytes = serialized_module(true)?;
    assert!(
        mentions(&pulley_bytes, "pulley64"),
        "use_pulley must compile for the pulley64 target"
    );
    assert!(
        !mentions(&jit_bytes, "pulley64"),
        "the default profile must keep the native JIT target"
    );
    Ok(())
}

/// Same vectors, both backends, byte-identical outcomes.
#[tokio::test(flavor = "multi_thread")]
async fn cranelift_and_pulley_agree_on_contract_vectors() -> Result<(), Box<dyn std::error::Error>>
{
    let (mut jit, jit_key, _dir_a) = runtime_for(false).await?;
    let (mut pulley, pulley_key, _dir_b) = runtime_for(true).await?;
    assert_eq!(
        jit_key, pulley_key,
        "same wasm + params must key identically"
    );

    let expected = run_vectors(&mut jit, &jit_key)?;
    let actual = run_vectors(&mut pulley, &pulley_key)?;
    assert_eq!(
        actual, expected,
        "Pulley must reproduce the Cranelift outcomes"
    );

    // Sanity: the vectors exercise real behaviour, not a degenerate contract.
    assert_eq!(expected.updated, vec![5, 2, 3, 4]);
    assert_eq!(expected.summary, vec![5, 2, 3]);
    assert_eq!(expected.delta, vec![4]);
    assert!(expected.validate_valid.contains("Valid"));
    assert!(expected.validate_invalid.contains("RequestRelated"));
    Ok(())
}
