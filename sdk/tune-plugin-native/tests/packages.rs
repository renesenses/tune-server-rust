use std::{collections::BTreeMap, fs};
use tune_plugin_native::package::*;
mod support;
use support::sign;
fn bundle(dir: &std::path::Path, contents: &[u8], target: &str) -> Vec<u8> {
    let bin = dir.join("fixture.so");
    fs::write(&bin, contents).unwrap();
    let manifest =
        serde_json::from_str(include_str!("../../tune-plugin-equalizer/manifest.json")).unwrap();
    pack(manifest, &bin, target, &BTreeMap::new()).unwrap()
}
#[test]
fn signed_install_update_rollback_and_tamper_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugins");
    let a = bundle(temp.path(), b"version A", host_target());
    let (keys, sig) = sign(&a);
    install_for(&root, &a, &sig, &keys, "equalizer").unwrap();
    let first = version_directory(&root, "equalizer").unwrap();
    let b = bundle(temp.path(), b"version B", host_target());
    let (_, sig_b) = sign(&b);
    install(&root, &b, &sig_b, &keys).unwrap();
    let second = version_directory(&root, "equalizer").unwrap();
    assert_ne!(first, second);
    let previous = rollback(&root, "equalizer", &keys).unwrap();
    assert_eq!(version_directory(&root, "equalizer").unwrap(), first);
    assert_eq!(
        previous.previous.unwrap(),
        second.file_name().unwrap().to_str().unwrap()
    );
    let before = fs::read(root.join("equalizer/active.json")).unwrap();
    assert!(install(&root, &b, &sig, &keys).is_err());
    assert!(install(&root, &b, &sig_b, &[]).is_err());
    assert!(install_for(&root, &b, &sig_b, &keys, "crossfeed").is_err());
    let wrong = bundle(temp.path(), b"wrong", "unsupported-target");
    let (_, wrong_sig) = sign(&wrong);
    assert!(install(&root, &wrong, &wrong_sig, &keys).is_err());
    assert_eq!(
        fs::read(root.join("equalizer/active.json")).unwrap(),
        before
    );
    fs::write(second.join("fixture.so"), b"tampered").unwrap();
    assert!(rollback(&root, "equalizer", &keys).is_err());
    assert_eq!(
        fs::read(root.join("equalizer/active.json")).unwrap(),
        before
    );
    // Even correctly signed non-library data must never masquerade as a plugin.
    assert!(load(&root, "equalizer", &keys).is_err());
    deactivate(&root, "equalizer").unwrap();
    assert!(installed_ids(&root).unwrap().is_empty());
    assert!(first.exists());
}
#[test]
fn packaging_refuses_nonportable_and_reserved_assets() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("x.so");
    fs::write(&bin, b"x").unwrap();
    for name in [
        "../escape",
        "a//b",
        "a/./b",
        "a\\b",
        "C:/bad",
        "bundle.minisig",
        "bundle.tuneplugin",
        "foo.",
    ] {
        let manifest =
            serde_json::from_str(include_str!("../../tune-plugin-equalizer/manifest.json"))
                .unwrap();
        assert!(
            pack(
                manifest,
                &bin,
                host_target(),
                &BTreeMap::from([(name.into(), vec![1])])
            )
            .is_err(),
            "{name}"
        );
    }
}
