//! Shared protocol fixtures: the same inputs on every platform must give the
//! same bytes, results, typed errors and callback order as on desktop.
//!
//! Desktop writes the expected values once (`examples/generate_fixtures.rs`).
//! Each device run computes the same values and compares them case by case.
//!
//! - **Encodings**: client requests and a node reply built from fixed inputs,
//!   bincode-encoded as the native client API sends them.
//! - **Operations**: a scripted run against a local-mode node: put, get,
//!   update, subscribe with ordered callbacks, typed errors, and a timed-out
//!   request followed by a read that must return the same bytes.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractRequest, ContractResponse, HostResponse, NodeQuery,
};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractWasmAPIVersion, Parameters, RelatedContracts,
    StateDelta, StateSummary, UpdateData, WrappedContract, WrappedState,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::client::{ContractListener, ContractRef, UpdateKind, connect_client};
use crate::conformance::FIXTURE_WASM;
use crate::error::MobileError;
use crate::runtime;

pub const FIXTURE_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct FixtureValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct FixtureCheck {
    pub name: String,
    pub expected: Option<String>,
    pub actual: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct FixtureReport {
    pub target: String,
    pub checks: Vec<FixtureCheck>,
    pub passed: bool,
}

/// The expected values file desktop writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureFile {
    pub format: u32,
    pub generated_on: String,
    pub core_revision: String,
    pub stdlib_version: String,
    pub values: BTreeMap<String, String>,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(bytes.as_ref())
}

fn encode(request: &ClientRequest<'static>) -> String {
    hex(bincode::serialize(request).expect("client requests always encode"))
}

/// A tiny stand-in contract: the key derivation only hashes the bytes.
fn sample_contract() -> ContractContainer {
    ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
        Arc::new(ContractCode::from(
            b"\0asm\x01\0\0\0freenet-mobile-fixture".to_vec(),
        )),
        Parameters::from(vec![1u8, 2, 3]),
    )))
}

/// Request and reply encodings, computed on this platform.
#[uniffi::export]
pub fn encoding_fixtures() -> Vec<FixtureValue> {
    let contract = sample_contract();
    let key = contract.key();
    let id = *key.id();
    let mut values = vec![
        ("key.instance_id", id.to_string()),
        ("key.code_hash", key.code_hash().encode()),
        (
            "request.put",
            encode(&ClientRequest::ContractOp(ContractRequest::Put {
                contract: contract.clone(),
                state: WrappedState::new(vec![4, 5, 6]),
                related_contracts: RelatedContracts::default(),
                subscribe: false,
                blocking_subscribe: false,
            })),
        ),
        (
            "request.get",
            encode(&ClientRequest::ContractOp(ContractRequest::Get {
                key: id,
                return_contract_code: true,
                subscribe: false,
                blocking_subscribe: false,
            })),
        ),
        (
            "request.update_delta",
            encode(&ClientRequest::ContractOp(ContractRequest::Update {
                key,
                data: UpdateData::Delta(StateDelta::from(vec![7u8, 8])),
            })),
        ),
        (
            "request.subscribe",
            encode(&ClientRequest::ContractOp(ContractRequest::Subscribe {
                key: id,
                summary: Some(StateSummary::from(vec![9u8])),
            })),
        ),
        (
            "request.connected_peers",
            encode(&ClientRequest::NodeQueries(NodeQuery::ConnectedPeers)),
        ),
        (
            "request.disconnect",
            encode(&ClientRequest::Disconnect {
                cause: Some(Cow::Borrowed("fixture")),
            }),
        ),
    ]
    .into_iter()
    .map(|(name, value)| FixtureValue {
        name: name.into(),
        value,
    })
    .collect::<Vec<_>>();

    // The node sends `Result<HostResponse, ClientError>`.
    let reply: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::GetResponse {
            key,
            contract: None,
            state: WrappedState::new(vec![4, 5, 6]),
        },
    ));
    values.push(FixtureValue {
        name: "reply.get".into(),
        value: hex(bincode::serialize(&reply).expect("replies always encode")),
    });
    values
}

/// Collects subscription callbacks in arrival order.
struct Collector {
    events: Mutex<Vec<String>>,
}

impl ContractListener for Collector {
    fn on_update(&self, _contract: ContractRef, kind: UpdateKind, bytes: Vec<u8>) {
        self.events.lock().push(format!("{kind:?}:{}", hex(bytes)));
    }

    fn on_closed(&self, reason: String) {
        self.events.lock().push(format!("closed:{reason}"));
    }
}

fn error_kind(result: Result<impl std::fmt::Debug, MobileError>) -> String {
    match result {
        Ok(value) => format!("ok:{value:?}"),
        Err(err) => match err.client_kind() {
            Some(kind) => format!("{kind:?}"),
            None => format!("error:{err}"),
        },
    }
}

/// Run the scripted operations against the local-mode node on `ws_port`.
async fn operation_values(ws_port: u16) -> Result<Vec<FixtureValue>, MobileError> {
    let mut values = BTreeMap::new();
    let client = connect_client(ws_port).await?;
    let writer = connect_client(ws_port).await?;

    let put = client
        .put(FIXTURE_WASM.to_vec(), vec![], vec![1, 2, 3], false, None)
        .await?;
    values.insert("op.put.instance_id", put.contract.instance_id.clone());
    values.insert("op.put.code_hash", put.contract.code_hash.clone());

    let got = client
        .get(put.contract.instance_id.clone(), false, false, None)
        .await?;
    values.insert("op.get.state", hex(&got.state));

    let updated = client
        .update(put.contract.clone(), vec![4, 5], None)
        .await?;
    values.insert("op.update.summary", hex(&updated.summary));
    let got = client
        .get(put.contract.instance_id.clone(), false, false, None)
        .await?;
    values.insert("op.get_after_update.state", hex(&got.state));

    // Callback order: three updates from another connection.
    let collector = Arc::new(Collector {
        events: Mutex::new(Vec::new()),
    });
    client
        .subscribe(put.contract.instance_id.clone(), collector.clone(), None)
        .await?;
    for delta in [[6u8], [7], [8]] {
        writer
            .update(put.contract.clone(), delta.to_vec(), None)
            .await?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while collector.events.lock().len() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    values.insert("op.subscribe.callbacks", collector.events.lock().join(","));

    // Typed errors.
    let unknown = crate::fixtures::sample_contract().key().id().to_string();
    values.insert(
        "op.error.get_unknown",
        error_kind(client.get(unknown, false, false, Some(10_000)).await),
    );
    values.insert(
        "op.error.put_invalid_state",
        error_kind(
            client
                .put(FIXTURE_WASM.to_vec(), vec![9], vec![], false, None)
                .await,
        ),
    );
    values.insert(
        "op.error.put_trapping_state",
        error_kind(
            client
                .put(FIXTURE_WASM.to_vec(), vec![8], vec![0xF0], false, None)
                .await,
        ),
    );

    // Cancellation: a request whose reply cannot arrive in time, because the
    // contract's validation spins until the node's execution limit, then a
    // read of the first contract, which must return the same bytes.
    values.insert(
        "op.cancel.timed_out",
        error_kind(
            client
                .put(FIXTURE_WASM.to_vec(), vec![7], vec![0xF3], false, Some(500))
                .await,
        ),
    );
    let after = client
        .get(put.contract.instance_id.clone(), false, false, None)
        .await?;
    values.insert("op.cancel.read_after", hex(&after.state));

    Ok(values
        .into_iter()
        .map(|(name, value)| FixtureValue {
            name: name.into(),
            value,
        })
        .collect())
}

/// The operation fixtures, run against a running local-mode node.
#[uniffi::export]
pub async fn operation_fixtures(ws_port: u16) -> Result<Vec<FixtureValue>, MobileError> {
    runtime::run(operation_values(ws_port)).await
}

/// Compare this platform's values with the expected file desktop wrote.
/// `ws_port` runs the operation fixtures too; `None` checks encodings only.
#[uniffi::export]
pub async fn verify_fixtures(
    expected_json: String,
    ws_port: Option<u16>,
) -> Result<FixtureReport, MobileError> {
    let expected: FixtureFile =
        serde_json::from_str(&expected_json).map_err(|e| MobileError::InvalidSettings {
            reason: format!("the expected fixtures are not valid: {e}"),
        })?;
    let mut actual = encoding_fixtures();
    if let Some(port) = ws_port {
        actual.extend(operation_fixtures(port).await?);
    }
    let mut checks: Vec<FixtureCheck> = actual
        .into_iter()
        .map(|value| {
            let expected_value = expected.values.get(&value.name).cloned();
            FixtureCheck {
                passed: expected_value.as_deref() == Some(value.value.as_str()),
                name: value.name,
                expected: expected_value,
                actual: value.value,
            }
        })
        .collect();
    // An expected case this run did not produce is a failure too.
    for (name, value) in &expected.values {
        let is_operation = name.starts_with("op.");
        if (ws_port.is_some() || !is_operation) && !checks.iter().any(|c| &c.name == name) {
            checks.push(FixtureCheck {
                name: name.clone(),
                expected: Some(value.clone()),
                actual: "missing".into(),
                passed: false,
            });
        }
    }
    let passed = checks.iter().all(|c| c.passed);
    Ok(FixtureReport {
        target: env!("FREENET_MOBILE_TARGET").into(),
        checks,
        passed,
    })
}

/// Build the expected-values file from this platform's values.
pub fn fixture_file(values: Vec<FixtureValue>) -> FixtureFile {
    FixtureFile {
        format: FIXTURE_FORMAT,
        generated_on: env!("FREENET_MOBILE_TARGET").into(),
        core_revision: env!("FREENET_MOBILE_CORE_REVISION").into(),
        stdlib_version: crate::STDLIB_VERSION.into(),
        values: values.into_iter().map(|v| (v.name, v.value)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_are_stable_within_a_build() {
        assert_eq!(encoding_fixtures(), encoding_fixtures());
    }

    #[test]
    fn encodings_decode_back() {
        let values = encoding_fixtures();
        let get = values.iter().find(|v| v.name == "request.get").unwrap();
        let bytes = hex::decode(&get.value).unwrap();
        let request: ClientRequest<'_> = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            request,
            ClientRequest::ContractOp(ContractRequest::Get {
                return_contract_code: true,
                ..
            })
        ));
    }
}
