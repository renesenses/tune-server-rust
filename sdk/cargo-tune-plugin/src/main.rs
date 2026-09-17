use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use tune_plugin_sdk::manifest::{Manifest, valid_id};

const USAGE: &str = "Experimental source SDK (not a production plugin installer)\n\
cargo tune-plugin new <id> --template <dsp|batch> --sdk-path <sdk> --output <new-directory>\n\
cargo tune-plugin check <plugin-directory>\n\
cargo tune-plugin test <plugin-directory>";

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let raw = fs::read_to_string(path.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    manifest
        .validate()
        .map_err(|e| format!("manifest: {e:?}"))?;
    Ok(manifest)
}

fn quoted_path(path: &Path) -> Result<String, String> {
    // JSON quoted strings form valid TOML basic strings for these filesystem
    // paths, including Windows backslashes and quotes. Never shell-interpolate.
    let value = path.to_str().ok_or("SDK path must be UTF-8")?;
    serde_json::to_string(value).map_err(|e| e.to_string())
}

fn scaffold(args: &[String]) -> Result<(), String> {
    if args.len() != 7 || !valid_id(&args[0]) {
        return Err(USAGE.into());
    }
    let id = &args[0];
    let mut template = None;
    let mut sdk = None;
    let mut output = None;
    for pair in args[1..].as_chunks::<2>().0 {
        let target = match pair[0].as_str() {
            "--template" => &mut template,
            "--sdk-path" => &mut sdk,
            "--output" => &mut output,
            _ => return Err(USAGE.into()),
        };
        if target.replace(pair[1].clone()).is_some() {
            return Err("duplicate option".into());
        }
    }
    let template = template.ok_or(USAGE)?;
    if !matches!(template.as_str(), "dsp" | "batch") {
        return Err("template must be dsp or batch".into());
    }
    let sdk = fs::canonicalize(sdk.ok_or(USAGE)?).map_err(|e| e.to_string())?;
    let output = PathBuf::from(output.ok_or(USAGE)?);
    for crate_name in ["tune-plugin-sdk", "tune-plugin-testkit"] {
        if !sdk.join(crate_name).join("Cargo.toml").is_file() {
            return Err(format!("missing SDK crate: {crate_name}"));
        }
    }
    let (lib, tests, capability) = if template == "dsp" {
        (
            include_str!("../templates/dsp.rs"),
            include_str!("../templates/dsp_tests.rs"),
            "audio-process",
        )
    } else {
        (
            include_str!("../templates/batch.rs"),
            include_str!("../templates/batch_tests.rs"),
            "file-jobs",
        )
    };
    let manifest = serde_json::json!({
        "id": id, "sdk": {"major": 0, "minor": 1}, "kind": template,
        "config_version": 1, "entitlement": format!("plugin.{id}"), "distribution": "source",
        "capabilities": [{"id": capability, "version": {"major": 0, "minor": 1}, "required": true}]
    });
    let cargo = format!(
        "[workspace]\n\n[package]\nname = \"tune-plugin-{id}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\ntune-plugin-sdk = {{ path = {} }}\nserde_json = \"1\"\n\n[dev-dependencies]\ntune-plugin-testkit = {{ path = {} }}\n",
        quoted_path(&sdk.join("tune-plugin-sdk"))?,
        quoted_path(&sdk.join("tune-plugin-testkit"))?
    );
    let tests = tests.replace(
        "PLUGIN_CRATE",
        &format!("tune_plugin_{}", id.replace('-', "_")),
    );
    let manifest_text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    let readme = format!(
        "# {id}\n\nExperimental {template} plugin generated against SDK 0.1.\n\nRun `cargo test` here; these tests exercise real sample processing or a memory job host.\nThe DSP example is a gain, not Tune EQ/crossfeed. The batch example copies PCM through a host, not a production converter or Dé-ploc.\n\nThis is a source-composed library. It is NOT installable into the released Tune server. There is no native ABI, production host adapter or UI bundle yet. `manifest.json` records requested capabilities, not grants.\nDo not rename a test-host success into hardware, encoder or production validation.\n\nGenerated SDK dependencies use explicit paths; pin a repository revision when sharing this project. Never import tune-core or tune-server.\n"
    );
    // Exclusive creation: refuse an existing directory, including an empty one
    // or a symlink. No --force mode that could clobber a user's project.
    fs::create_dir(&output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    for dir in ["src", "tests"] {
        fs::create_dir(output.join(dir)).map_err(|e| e.to_string())?;
    }
    for (name, body) in [
        ("Cargo.toml", cargo.as_str()),
        ("manifest.json", manifest_text.as_str()),
        ("src/lib.rs", lib),
        ("tests/conformance.rs", tests.as_str()),
        ("README.md", readme.as_str()),
        (".gitignore", "/target/\n"),
    ] {
        fs::write(output.join(name), body).map_err(|e| e.to_string())?;
    }
    println!("Created {} (experimental source plugin)", output.display());
    Ok(())
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    if args.first().is_some_and(|s| s == "tune-plugin") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        Some("new") => scaffold(&args[1..]),
        Some("check" | "test") if args.len() == 2 => {
            let path = Path::new(&args[1]);
            let manifest = read_manifest(path)?;
            for required in ["Cargo.toml", "src/lib.rs", "tests/conformance.rs"] {
                if !path.join(required).is_file() {
                    return Err(format!("missing {required}"));
                }
            }
            if args[0] == "test" {
                let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                    .arg("test")
                    .arg("--manifest-path")
                    .arg(path.join("Cargo.toml"))
                    .arg("--all-targets")
                    .status()
                    .map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err("plugin conformance tests failed".into());
                }
            }
            println!(
                "{}: source manifest valid; production compatibility is not certified",
                manifest.id
            );
            Ok(())
        }
        Some("--help" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
