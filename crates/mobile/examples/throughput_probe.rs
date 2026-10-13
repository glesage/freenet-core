//! Subscription throughput on this computer, for comparison with the phones:
//! one connection subscribes, another sends updates back to back.
//! `cargo run -p freenet-mobile --release --example throughput_probe -- [count]`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use freenet_mobile::client::{ContractListener, ContractRef, UpdateKind, connect_client};
use freenet_mobile::{MobileNode, NodeMode, NodeSettings};
use futures::executor::block_on;

struct Count(AtomicUsize);

impl ContractListener for Count {
    fn on_update(&self, _: ContractRef, _: UpdateKind, _: Vec<u8>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn on_closed(&self, reason: String) {
        eprintln!("closed: {reason}");
    }
}

fn main() -> anyhow::Result<()> {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
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
    let reader = block_on(connect_client(info.ws_port))?;
    let writer = block_on(connect_client(info.ws_port))?;
    let wasm = freenet_mobile::conformance::fixture_contract_wasm();
    let put = block_on(reader.put(wasm, vec![0x7A], vec![1], false, None))?;
    let counter = Arc::new(Count(AtomicUsize::new(0)));
    block_on(reader.subscribe(put.contract.instance_id.clone(), counter.clone(), None))?;
    let started = Instant::now();
    let mut slowest = 0f64;
    for i in 0..count {
        let t = Instant::now();
        match block_on(writer.update(put.contract.clone(), vec![0x42; 64], Some(30_000))) {
            Ok(_) => {}
            Err(e) => {
                println!(
                    "update {i} failed after {:.1} s: {e}",
                    t.elapsed().as_secs_f64()
                );
                break;
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        slowest = slowest.max(ms);
        if i % 50 == 0 {
            println!(
                "update {i}: {ms:.1} ms, {} notifications so far",
                counter.0.load(Ordering::SeqCst)
            );
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    std::thread::sleep(std::time::Duration::from_secs(1));
    println!(
        "{count} updates in {elapsed:.2} s ({:.1}/s), slowest {slowest:.1} ms, {} notifications",
        count as f64 / elapsed,
        counter.0.load(Ordering::SeqCst)
    );
    let log_dir = root.path().join("logs");
    for entry in std::fs::read_dir(&log_dir)? {
        let text = std::fs::read_to_string(entry?.path())?;
        for line in text
            .lines()
            .filter(|l| l.contains("rate") || l.contains("WARN") || l.contains("ERROR"))
            .take(15)
        {
            println!("log: {}", &line[..line.len().min(300)]);
        }
    }
    block_on(node.clone().stop())?;
    Ok(())
}
