//! Where the reserved address space goes: virtual size after each step.
//! `cargo run -p freenet-mobile --release --example vm_probe -- [cranelift|pulley]`

use freenet_mobile::metrics::process_metrics;
use freenet_mobile::{MobileNode, NodeMode, NodeSettings, WasmBackendChoice};
use futures::executor::block_on;

fn report(step: &str) {
    let m = process_metrics();
    println!(
        "{step:<32} virtual {:>8.1} GiB  resident {:>7.1} MiB  footprint {:>7.1} MiB",
        m.virtual_bytes as f64 / (1u64 << 30) as f64,
        m.resident_bytes as f64 / (1u64 << 20) as f64,
        m.footprint_bytes.unwrap_or(0) as f64 / (1u64 << 20) as f64
    );
}

fn main() -> anyhow::Result<()> {
    let backend = match std::env::args().nth(1).as_deref() {
        Some("cranelift") => Some(WasmBackendChoice::Cranelift),
        Some("pulley") => Some(WasmBackendChoice::Pulley),
        _ => None,
    };
    report("process start");
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
        wasm_backend: backend,
        log_filter: None,
        module_cache_budget_bytes: None,
        max_hosting_storage_bytes: None,
        max_hosting_disk_bytes: None,
    })?;
    let info = block_on(node.clone().start())?;
    report("node started");
    let client = block_on(freenet_mobile::client::connect_client(info.ws_port))?;
    let wasm = freenet_mobile::conformance::fixture_contract_wasm();
    for i in 0..10u8 {
        block_on(client.put(wasm.clone(), vec![i], vec![1, 2, 3], false, None))?;
    }
    report("10 contracts stored");
    for i in 10..60u8 {
        block_on(client.put(wasm.clone(), vec![i], vec![1, 2, 3], false, None))?;
    }
    report("60 contracts stored");
    block_on(node.clone().stop())?;
    report("node stopped");
    let report_c = block_on(freenet_mobile::conformance::run_backend_conformance(
        vec![],
        false,
    ));
    report(&format!("conformance run (passed {})", report_c.passed));
    Ok(())
}
