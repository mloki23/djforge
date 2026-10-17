//! Compiles the vendored Signalsmith Stretch C wrapper (no bindgen: the bindings in `src/ffi.rs`
//! are written by hand against `vendor/wrapper/wrapper.h`).

fn main() {
    println!("cargo::rerun-if-changed=vendor");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .file("vendor/wrapper/wrapper.cpp")
        .include("vendor/wrapper")
        .include("vendor/signalsmith-stretch")
        .include("vendor")
        .define("NDEBUG", None)
        .opt_level(3)
        .warnings(false)
        .compile("signalsmith-stretch");
}
