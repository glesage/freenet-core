//! Thin shim so `cargo run -p freenet-mobile --bin uniffi-bindgen` drives the
//! UniFFI binding generator against this crate's compiled library.
fn main() {
    uniffi::uniffi_bindgen_main()
}
