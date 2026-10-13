//! Write `manifest.json` for a web bundle directory.
//!
//! `cargo run -p freenet-mobile --example bundle_manifest -- <dir> <name>`

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: bundle_manifest <dir> <name>"))?;
    let name = args.next().unwrap_or_else(|| "bundle".into());
    let manifest = freenet_mobile::bundle::write_bundle_manifest(dir.clone(), name)?;
    let files: serde_json::Value = serde_json::from_str(&manifest)?;
    let count = files["files"].as_object().map(|f| f.len()).unwrap_or(0);
    println!("wrote {dir}/manifest.json with {count} files");
    Ok(())
}
