# freenet-mobile

An embedded Freenet node for iOS and Android apps, with [UniFFI](https://mozilla.github.io/uniffi-rs/latest/) Swift and Kotlin bindings. The iOS and Android packaging, demo apps and measurement harness live in `freenet-appkit`.

| Module | What it holds |
| --- | --- |
| `node` | Start, stop, status, peer count and events. One coordinator runs every start and stop |
| `settings` | Host-supplied directories and the per-mode store layout; discards a persisted config written for another data directory, mode or gateway source |
| `client` | Put, get, update and subscribe for custom Swift and Kotlin screens |
| `bundle` | Host-served web bundles checked against a per-file SHA-256 manifest |
| `bridge` | The WebView JSON bridge, handled here so iOS and Android answer every message the same way |
| `conformance` | The Wasm backend suite, run on the device |
| `fixtures` | Shared protocol fixtures compared with desktop values |
| `metrics`, `json` | Memory, CPU, storage and peer-traffic measurements, and JSON forms of the reports |

How the node runs:

- One process-wide Tokio runtime; the node is built inside it. Nothing installs a signal, panic or abort handler, and stopping is an explicit call.
- The client API listens on `127.0.0.1`, on the preferred port when it is free and any free port otherwise. `NodeInfo` reports the port the node got.
- *Local* mode is an isolated node with no peers, for fixtures and offline use. *Network* mode joins through gateway overrides or the public gateway index.
- iOS runs Wasm through wasmtime's Pulley interpreter; Core refuses Cranelift there.

## Build and test

```bash
cargo test -p freenet-mobile
```

Generate bindings from a built library:

```bash
cargo run -p freenet-mobile --features bindgen-cli --bin uniffi-bindgen -- generate --library target/release/libfreenet_mobile.dylib --language swift --out-dir out
```

## Examples

| Example | Use |
| --- | --- |
| `generate_fixtures` | Writes the desktop protocol fixture values that device runs compare with |
| `isolated_gateway` | An isolated test network on this computer, preloaded with contracts, for phones to join |
| `fetch_contract` | Reads a contract such as River's website container from the public network, read-only |
| `bundle_manifest` | Writes `manifest.json` for a web bundle directory |
| `vm_probe`, `throughput_probe` | Address-space and subscription-throughput baselines on desktop |

## The fixture contract

`fixtures/backend-conformance.wasm` is `tests/test-contract-backend-conformance` built for release. Rebuild it after changing that crate:

```bash
cd tests/test-contract-backend-conformance && cargo build --release --target wasm32-unknown-unknown && cp target/wasm32-unknown-unknown/release/test_contract_backend_conformance.wasm ../../crates/mobile/fixtures/backend-conformance.wasm
```
