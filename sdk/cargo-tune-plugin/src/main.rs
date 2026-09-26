use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use tune_plugin_sdk::manifest::{Manifest, valid_id};

mod packaging;
const USAGE: &str = "Tune premium audio SDK (ABI 1)\n\
cargo tune-plugin new <id> --template <dsp|batch|equalizer|crossfeed|converter|declick> --sdk-path <sdk> --output <new-directory>\n\
cargo tune-plugin check <plugin-directory>\n\
cargo tune-plugin test <plugin-directory>\n\
cargo tune-plugin pack <plugin-directory> --target <triple> --output <new.tuneplugin>\n\
cargo tune-plugin dev <plugin-directory> --input <wav> --output <new.wav> --settings <json-file>";

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let raw = fs::read_to_string(path.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    manifest
        .validate()
        .map_err(|e| format!("manifest: {e:?}"))?;
    manifest
        .negotiate(&tune_plugin_sdk::manifest::reference_host_capabilities())
        .map_err(|e| format!("capabilities: {e:?}"))?;
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
    let template = match template.ok_or(USAGE)?.as_str() {
        "dsp-with-ui" => "dsp".to_string(),
        "batch-with-ui" => "batch".to_string(),
        value => value.to_string(),
    };
    let sdk = fs::canonicalize(sdk.ok_or(USAGE)?).map_err(|e| e.to_string())?;
    let concrete = !matches!(template.as_str(), "dsp" | "batch");
    let catalog: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(sdk.join("plugins.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let reference = if concrete {
        let entry = catalog["native"]
            .as_array()
            .ok_or("invalid native plugin catalog")?
            .iter()
            .find(|p| p["id"].as_str() == Some(template.as_str()))
            .ok_or("unknown plugin template")?;
        let directory = entry["crate"].as_str().ok_or("missing template crate")?;
        if !valid_id(directory) {
            return Err("invalid template crate".into());
        }
        sdk.join(directory)
    } else {
        sdk.join(format!("tune-plugin-{template}"))
    };
    let reference_manifest: serde_json::Value = if concrete {
        serde_json::from_str(
            &fs::read_to_string(reference.join("manifest.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?
    } else {
        serde_json::Value::Null
    };
    let kind = if concrete {
        reference_manifest["kind"]
            .as_str()
            .filter(|k| matches!(*k, "dsp" | "batch"))
            .ok_or("invalid template kind")?
    } else {
        template.as_str()
    };
    let output = PathBuf::from(output.ok_or(USAGE)?);
    for crate_name in ["tune-plugin-sdk", "tune-plugin-testkit"] {
        if !sdk.join(crate_name).join("Cargo.toml").is_file() {
            return Err(format!("missing SDK crate: {crate_name}"));
        }
    }
    let mut extra_files = Vec::new();
    let (mut lib, tests) = if concrete {
        let lib = fs::read_to_string(reference.join("src/lib.rs")).map_err(|e| e.to_string())?;
        let tests = fs::read_to_string(reference.join("tests/conformance.rs"))
            .map_err(|e| e.to_string())?
            .replace(
                &format!("tune_plugin_{}", template.replace('-', "_")),
                "PLUGIN_CRATE",
            );
        // #5081 — le crossfeed porte aussi `ombre.rs` (le filtre d'ombre de
        // la tête) et les témoins de son moteur ; absents ailleurs, ignorés.
        for name in [
            "engine.rs",
            "sdk.rs",
            "ombre.rs",
            "engine_ombre_5081_tests.rs",
        ] {
            let source = reference.join("src").join(name);
            if source.is_file() {
                extra_files.push((
                    format!("src/{name}"),
                    fs::read_to_string(source).map_err(|e| e.to_string())?,
                ));
            }
        }
        (lib, tests)
    } else if kind == "dsp" {
        (
            include_str!("../templates/dsp.rs").to_string(),
            include_str!("../templates/dsp_tests.rs").to_string(),
        )
    } else {
        (
            include_str!("../templates/batch.rs").to_string(),
            include_str!("../templates/batch_tests.rs").to_string(),
        )
    };
    let capability = if kind == "dsp" {
        "audio-process"
    } else {
        "file-jobs"
    };
    let mut manifest = serde_json::json!({
        "id": id, "sdk": {"major": 0, "minor": 1}, "kind": kind,
        "config_version": 1, "entitlement": format!("plugin.{id}"), "distribution": "source",
        "capabilities": [{"id": capability, "version": {"major": 0, "minor": 1}, "required": true}]
    });
    if concrete {
        manifest = serde_json::from_str(
            &fs::read_to_string(reference.join("manifest.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        manifest["id"] = serde_json::json!(id);
    }
    let mut cargo = format!(
        "[workspace]\n\n[package]\nname = \"tune-plugin-{id}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\ntune-plugin-sdk = {{ path = {} }}\nserde_json = \"1\"\n\n[dev-dependencies]\ntune-plugin-testkit = {{ path = {} }}\n",
        quoted_path(&sdk.join("tune-plugin-sdk"))?,
        quoted_path(&sdk.join("tune-plugin-testkit"))?
    );
    if concrete {
        // L'égaliseur ET le crossfeed appellent tous deux
        // `tune_plugin_audio_support::niveau_moyen` depuis #4685 : le projet
        // engendré doit déclarer la caisse, sinon il ne compile pas (E0433) —
        // c'est ce qui a mis « SDK source contracts » au rouge sur les trois
        // OS. La liste se lit dans le SOURCE recopié, pas dans une énumération
        // de noms : un troisième greffon qui s'y mettrait serait couvert sans
        // qu'on y pense.
        let support = if fs::read_to_string(reference.join("src/engine.rs"))
            .map(|src| src.contains("tune_plugin_audio_support"))
            .unwrap_or(template == "equalizer")
        {
            format!(
                "tune-plugin-audio-support = {{ path = {} }}\n",
                quoted_path(&sdk.join("tune-plugin-audio-support"))?
            )
        } else {
            String::new()
        };
        cargo = cargo.replace(
            "[dependencies]\n",
            &format!(
                "[dependencies]\nserde = {{ version = \"1\", features = [\"derive\"] }}\n{support}"
            ),
        );
    }
    cargo.push_str(&format!("\n[lib]\ncrate-type = [\"rlib\", \"cdylib\"]\n[features]\nnative = [\"dep:tune-plugin-abi\"]\nschemas = [\"dep:schemars\", \"tune-plugin-sdk/schemas\"]\n[dependencies.schemars]\nversion = \"1\"\noptional = true\n[dependencies.tune-plugin-abi]\npath = {}\noptional = true\n", quoted_path(&sdk.join("tune-plugin-abi"))?));
    if !concrete {
        lib = lib.replace(
            "#![forbid(unsafe_code)]",
            "#![cfg_attr(not(feature = \"native\"), forbid(unsafe_code))]",
        );
        lib.push_str(&format!("\n#[cfg(feature = \"native\")]\ntune_plugin_abi::export_{kind}!(crate::Plugin, include_str!(\"../manifest.json\"));\n"));
    }
    if concrete {
        let example = fs::read_to_string(reference.join("examples/schema.rs"))
            .map_err(|e| e.to_string())?
            .replace(
                &format!("tune_plugin_{}", template.replace('-', "_")),
                &format!("tune_plugin_{}", id.replace('-', "_")),
            );
        extra_files.push(("examples/schema.rs".into(), example));
        cargo.push_str("\n[[example]]\nname = \"schema\"\nrequired-features = [\"schemas\"]\n");
    }
    let tests = tests.replace(
        "PLUGIN_CRATE",
        &format!("tune_plugin_{}", id.replace('-', "_")),
    );
    let manifest_text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    let readme = format!(
        "# {id}\n\n{template} plugin generated against Tune SDK 0.1.\n\nRun `cargo test` here. The four named feature templates copy the real implementations and their conformance tests; `dsp` and `batch` remain minimal examples. Source dependencies point only to the SDK, never tune-core or tune-server.\n\nBuild with `--features native` or `cargo tune-plugin pack`. For source composition register the factory/tool in your Tune host; the shipped four adapters preserve legacy API and UI. Native distribution uses the versioned C ABI; Rust traits remain within their library. Pin the SDK repository revision before distributing source. See SDK README and docs/plugins/premium-sdk.md for services, lifecycle, UI bridge and migration.\n"
    );
    // Exclusive creation: refuse an existing directory, including an empty one
    // or a symlink. No --force mode that could clobber a user's project.
    fs::create_dir(&output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    for dir in [
        "src", "tests", "ui", "schemas", "fixtures", "docs", "ci", "examples",
    ] {
        fs::create_dir(output.join(dir)).map_err(|e| e.to_string())?;
    }
    for (name, body) in [
        ("Cargo.toml", cargo.as_str()),
        ("manifest.json", manifest_text.as_str()),
        ("src/lib.rs", lib.as_str()),
        ("tests/conformance.rs", tests.as_str()),
        ("README.md", readme.as_str()),
        (".gitignore", "/target/\n"),
    ] {
        fs::write(output.join(name), body).map_err(|e| e.to_string())?;
    }
    fs::write(
        output.join("ui/client.mjs"),
        include_str!("../../ui/client.mjs"),
    )
    .map_err(|e| e.to_string())?;
    for (name, content) in [
        ("bridge.mjs", include_str!("../../ui/bridge.mjs")),
        ("index.html", include_str!("../../ui/index.html")),
        ("panel.mjs", include_str!("../../ui/panel.mjs")),
    ] {
        fs::write(output.join("ui").join(name), content).map_err(|e| e.to_string())?;
    }
    // Self-contained signed document: sandboxed modules need no cookie-bearing
    // subresource fetch, and all service access stays on the private port.
    let inline = format!(
        "{}\n{}",
        include_str!("../../ui/bridge.mjs").replace("export function ", "function "),
        include_str!("../../ui/panel.mjs")
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let html = include_str!("../../ui/index.html").replace(
        "<script type=\"module\" src=\"panel.mjs\"></script>",
        &format!("<script type=\"module\">{inline}</script>"),
    );
    fs::write(output.join("ui/index.html"), html).map_err(|e| e.to_string())?;
    for name in ["manifest", "commands", "events", "jobs"] {
        let schema = fs::read(
            sdk.join("tune-plugin-sdk/schemas")
                .join(format!("{name}.json")),
        )
        .map_err(|e| e.to_string())?;
        fs::write(output.join("schemas").join(format!("{name}.json")), schema)
            .map_err(|e| e.to_string())?;
    }
    let config_schema = if concrete {
        fs::read(reference.join("schemas/config.json")).map_err(|e| e.to_string())?
    } else {
        serde_json::to_vec_pretty(&serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":if kind=="dsp" {serde_json::json!({"gain":{"type":"number","minimum":0,"maximum":2}})} else {serde_json::json!({"codec":{"type":"string"}})}})).map_err(|e|e.to_string())?
    };
    fs::write(output.join("schemas/config.json"), config_schema).map_err(|e| e.to_string())?;
    fs::write(output.join("schemas/migrations.json"),"{\"version\":1,\"migrations\":[],\"policy\":\"preserve settings; refuse unknown future versions\"}\n").map_err(|e|e.to_string())?;
    fs::write(
        output.join("docs/contract.md"),
        include_str!("../../README.md"),
    )
    .map_err(|e| e.to_string())?;
    fs::write(output.join("fixtures/signals.json"),"{\"version\":1,\"signals\":[\"silence\",\"impulse\",\"deterministic stereo sine\"],\"oracle\":\"tests/conformance.rs\"}\n").map_err(|e|e.to_string())?;
    fs::write(output.join("ci/check.sh"),"#!/bin/sh\nset -eu\ncargo test --all-targets\ncargo build --features native\ncargo doc --no-deps\n").map_err(|e|e.to_string())?;
    fs::write(
        output.join("ui/client.d.ts"),
        include_str!("../../ui/client.d.ts"),
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        output.join("ui/fr.json"),
        "{\"load\":\"Lire\",\"apply\":\"Appliquer\",\"cancel\":\"Annuler\"}\n",
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        output.join("ui/en.json"),
        "{\"load\":\"Read\",\"apply\":\"Apply\",\"cancel\":\"Cancel\"}\n",
    )
    .map_err(|e| e.to_string())?;
    for (name, body) in extra_files {
        fs::write(output.join(name), body).map_err(|e| e.to_string())?;
    }
    println!("Created {} (SDK 0.1, native ABI 1)", output.display());
    Ok(())
}

fn run(mut args: Vec<String>) -> Result<(), String> {
    if args.first().is_some_and(|s| s == "tune-plugin") {
        args.remove(0);
    }
    if args.first().is_some_and(|s| s == "test")
        && args.get(1).is_some_and(|s| s == "--conformance")
    {
        args.remove(1);
    }
    match args.first().map(String::as_str) {
        Some("new") => scaffold(&args[1..]),
        Some("pack") => packaging::pack(&args[1..]),
        Some("dev") => packaging::dev(&args[1..]),
        Some("check" | "test") if args.len() == 2 => {
            let path = Path::new(&args[1]);
            let manifest = read_manifest(path)?;
            for required in ["Cargo.toml", "src/lib.rs", "tests/conformance.rs"] {
                if !path.join(required).is_file() {
                    return Err(format!("missing {required}"));
                }
            }
            for name in ["manifest", "config", "commands", "events", "jobs"] {
                let value: serde_json::Value = serde_json::from_slice(
                    &fs::read(path.join("schemas").join(format!("{name}.json")))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                if value["$schema"] != "https://json-schema.org/draft/2020-12/schema"
                    || !value.is_object()
                {
                    return Err(format!("invalid schema document: {name}"));
                }
            }
            let locales: Vec<serde_json::Value> = ["fr", "en"]
                .iter()
                .map(|locale| {
                    fs::read(path.join("ui").join(format!("{locale}.json")))
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
                })
                .collect::<Result<_, _>>()?;
            if locales.iter().any(|locale| !locale.is_object())
                || locales[0].as_object().unwrap().keys().collect::<Vec<_>>()
                    != locales[1].as_object().unwrap().keys().collect::<Vec<_>>()
            {
                return Err("translation keys differ".into());
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
