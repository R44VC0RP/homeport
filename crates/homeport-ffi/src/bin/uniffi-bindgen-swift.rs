//! Generates the Swift bindings, header and modulemap (used by scripts/build-ffi.sh).
fn main() {
    uniffi::uniffi_bindgen_swift()
}
