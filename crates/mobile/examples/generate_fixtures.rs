//! Write the expected protocol fixture values from this desktop build.
//!
//! `cargo run -p freenet-mobile --release --example generate_fixtures -- <out.json>`
//!
//! Devices compare their own values against this file with
//! `verify_fixtures`.

use freenet_mobile::fixtures::{encoding_fixtures, fixture_file, operation_fixtures};
use freenet_mobile::{MobileNode, NodeMode, NodeSettings};
use futures::executor::block_on;

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: generate_fixtures <out.json>"))?;
    let root = tempfile::tempdir()?;
    let node = MobileNode::new(NodeSettings {
        data_dir: root.path().join("data").display().to_string(),
        config_dir: root.path().join("config").display().to_string(),
        log_dir: root.path().join("logs").display().to_string(),
        cache_dir: None,
        mode: NodeMode::Local,
        preferred_ws_port: None,
        network_port: None,
        gateways: vec![],
        wasm_backend: None,
        log_filter: None,
        module_cache_budget_bytes: None,
        max_hosting_storage_bytes: None,
        max_hosting_disk_bytes: None,
    })?;
    let info = block_on(node.clone().start())?;
    let mut values = encoding_fixtures();
    values.extend(block_on(operation_fixtures(info.ws_port))?);
    block_on(node.clone().stop())?;
    let file = fixture_file(values);
    std::fs::write(&out, format!("{}\n", serde_json::to_string_pretty(&file)?))?;
    println!("wrote {} fixture values to {out}", file.values.len());
    Ok(())
}
