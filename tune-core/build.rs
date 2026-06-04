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
}
