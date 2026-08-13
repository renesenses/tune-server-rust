use std::process::Command;

fn main() {
    // Capture rustc version at compile time
    let rustc_version = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".into());
    println!(
        "cargo:rustc-env=TUNE_RUSTC_VERSION={}",
        rustc_version.trim()
    );

    // Apple's reference ALAC encoder (Apache-2.0), vendored for the native
    // ALAC conversion path (#1526). Same precedent as rusqlite `bundled`:
    // compiled into the binary, can never be missing at runtime.
    // C and C++ units are built separately — the .c files are C, not C++.
    // Link order matters for static archives: the C++ side (ALACEncoder)
    // calls into the C units (ag_enc/dp_enc/matrix_enc), so `alac_cpp` must
    // be emitted BEFORE `alac_c` — the linker resolves left to right and
    // discards archive members nobody has referenced yet.
    let alac = "vendor/alac";
    cc::Build::new()
        .cpp(true)
        // Le g++ du conteneur cross aarch64 compile en C++03 par défaut :
        // sans ceci, « 'nullptr' was not declared in this scope ».
        .std("c++11")
        .include(alac)
        .files([
            format!("{alac}/ALACEncoder.cpp"),
            format!("{alac}/tune_alac_shim.cpp"),
        ])
        .warnings(false)
        .compile("alac_cpp");
    cc::Build::new()
        .include(alac)
        .files([
            format!("{alac}/ALACBitUtilities.c"),
            format!("{alac}/EndianPortable.c"),
            format!("{alac}/ag_enc.c"),
            // ag_dec.c porte aussi set_standard_ag_params/set_ag_params,
            // partagées par l'ENCODEUR — découpage historique d'Apple.
            format!("{alac}/ag_dec.c"),
            format!("{alac}/dp_enc.c"),
            format!("{alac}/matrix_enc.c"),
        ])
        .warnings(false)
        .compile("alac_c");
    println!("cargo:rerun-if-changed={alac}");
}
