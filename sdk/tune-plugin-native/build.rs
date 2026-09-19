fn main() {
    println!(
        "cargo:rustc-env=TUNE_PLUGIN_TARGET={}",
        std::env::var("TARGET").expect("Cargo TARGET")
    );
}
