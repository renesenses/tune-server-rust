//! Signed, content-addressed packages. Verify BEFORE extracting/loading code.
//! Activation swaps only a small pointer file. Loaded libraries remain pinned;
//! installation, rollback and removal take effect at the next server startup.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tune_plugin_sdk::manifest::{Manifest, valid_id};
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
pub fn host_target() -> &'static str {
    env!("TUNE_PLUGIN_TARGET")
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub version: u32,
    pub abi: u32,
    pub target: String,
    pub manifest: Manifest,
    pub binary: String,
    pub files: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Active {
    pub current: String,
    pub previous: Option<String>,
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn safe_path(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with('.')
                && !part.ends_with(' ')
        })
        && !matches!(name, "bundle.tuneplugin" | "bundle.minisig")
        && !name.contains('\\')
        && !name.contains(':')
        && Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !name.starts_with('/')
}
fn digest_name(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn verify_signature(bytes: &[u8], signature: &str, keys: &[String]) -> Result<(), String> {
    if keys.is_empty() {
        return Err("no trusted native plugin signing key configured".into());
    }
    let sig = minisign_verify::Signature::decode(signature).map_err(|e| e.to_string())?;
    if keys
        .iter()
        .filter_map(|key| minisign_verify::PublicKey::from_base64(key).ok())
        .any(|key| key.verify(bytes, &sig, false).is_ok())
    {
        Ok(())
    } else {
        Err("native plugin signature is not trusted".into())
    }
}
fn unpack(bytes: &[u8], target: &str) -> Result<(Package, BTreeMap<String, Vec<u8>>), String> {
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err("archive exceeds size limit".into());
    }
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    if archive.len() > 512 {
        return Err("too many package entries".into());
    }
    let mut total = 0u64;
    let mut entries = BTreeMap::new();
    let mut portable_names = BTreeSet::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|e| e.to_string())?;
        let name = file.name().to_string();
        if !safe_path(&name)
            || file.is_dir()
            || file
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 != 0 && mode & 0o170000 != 0o100000)
            || !portable_names.insert(name.to_lowercase())
        {
            return Err("unsafe or duplicate package entry".into());
        }
        total = total
            .checked_add(file.size())
            .filter(|n| *n <= MAX_EXPANDED)
            .ok_or("expanded package exceeds size limit")?;
        let mut data = Vec::new();
        file.by_ref()
            .take(MAX_EXPANDED + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() as u64 != file.size() {
            return Err("invalid expanded entry size".into());
        }
        entries.insert(name, data);
    }
    let package: Package = serde_json::from_slice(
        entries
            .remove("package.json")
            .ok_or("missing package.json")?
            .as_slice(),
    )
    .map_err(|e| e.to_string())?;
    if package.version != 1
        || package.abi != tune_plugin_abi::ABI_VERSION
        || package.target != target
    {
        return Err("incompatible target or native ABI".into());
    }
    package
        .manifest
        .validate()
        .map_err(|e| format!("manifest: {e:?}"))?;
    let caps = tune_plugin_sdk::manifest::reference_host_capabilities();
    package
        .manifest
        .negotiate(&caps)
        .map_err(|e| format!("capabilities: {e:?}"))?;
    if !safe_path(&package.binary)
        || !package.files.contains_key(&package.binary)
        || package.files.keys().collect::<BTreeSet<_>>() != entries.keys().collect()
    {
        return Err("package file inventory mismatch".into());
    }
    for (name, expected) in &package.files {
        if !digest_name(expected) || sha(&entries[name]) != *expected {
            return Err(format!("checksum mismatch: {name}"));
        }
    }
    Ok((package, entries))
}
/// Builds an unsigned artifact for the signing step. Distribution requires a
/// detached minisign signature; this function does not create/trust a key.
pub fn pack(
    manifest: Manifest,
    binary: &Path,
    target: &str,
    assets: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>, String> {
    manifest
        .validate()
        .map_err(|e| format!("manifest: {e:?}"))?;
    let filename = binary
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| safe_path(s))
        .ok_or("invalid binary name")?
        .to_string();
    let mut files = assets.clone();
    if files.contains_key(&filename) || files.contains_key("package.json") {
        return Err("duplicate binary/manifest name".into());
    }
    files.insert(
        filename.clone(),
        fs::read(binary).map_err(|e| e.to_string())?,
    );
    if files.keys().any(|name| !safe_path(name)) {
        return Err("unsafe asset path".into());
    }
    let package = Package {
        version: 1,
        abi: tune_plugin_abi::ABI_VERSION,
        target: target.into(),
        manifest,
        binary: filename,
        files: files
            .iter()
            .map(|(name, bytes)| (name.clone(), sha(bytes)))
            .collect(),
    };
    files.insert(
        "package.json".into(),
        serde_json::to_vec_pretty(&package).map_err(|e| e.to_string())?,
    );
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(0o644),
            )
            .map_err(|e| e.to_string())?;
        writer.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    let bytes = writer.finish().map_err(|e| e.to_string())?.into_inner();
    unpack(&bytes, target)?;
    Ok(bytes)
}
fn atomic_state(directory: &Path, state: &Active) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, state).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(directory.join("active.json"))
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn active(directory: &Path) -> Result<Option<Active>, String> {
    match fs::read(directory.join("active.json")) {
        Ok(data) => {
            let state: Active = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            if !digest_name(&state.current)
                || state.previous.as_ref().is_some_and(|v| !digest_name(v))
            {
                return Err("invalid version pointer".into());
            }
            Ok(Some(state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
/// Verify, stage on the same filesystem, retain the previous version, then
/// atomically activate for NEXT startup. A failed install leaves active intact.
pub fn install_for(
    root: &Path,
    bytes: &[u8],
    signature: &str,
    keys: &[String],
    expected_id: &str,
) -> Result<Package, String> {
    verify_signature(bytes, signature, keys)?;
    let (package, _) = unpack(bytes, host_target())?;
    if package.manifest.id != expected_id {
        return Err("package belongs to a different feature".into());
    }
    install(root, bytes, signature, keys)
}
pub fn install(
    root: &Path,
    bytes: &[u8],
    signature: &str,
    keys: &[String],
) -> Result<Package, String> {
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err("archive exceeds size limit".into());
    }
    verify_signature(bytes, signature, keys)?;
    let (package, files) = unpack(bytes, host_target())?;
    let directory = root.join(&package.manifest.id);
    let versions = directory.join("versions");
    fs::create_dir_all(&versions).map_err(|e| e.to_string())?;
    let hash = sha(bytes);
    let dest = versions.join(&hash);
    if !dest.exists() {
        let stage = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&versions)
            .map_err(|e| e.to_string())?;
        for (name, content) in files {
            let path = stage.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::write(path, content).map_err(|e| e.to_string())?;
        }
        fs::write(stage.path().join("bundle.tuneplugin"), bytes).map_err(|e| e.to_string())?;
        fs::write(stage.path().join("bundle.minisig"), signature).map_err(|e| e.to_string())?;
        fs::rename(stage.path(), &dest).map_err(|e| e.to_string())?;
    }
    verify_version(&dest, keys)?;
    let current = active(&directory)?;
    if current.as_ref().is_none_or(|v| v.current != hash) {
        atomic_state(
            &directory,
            &Active {
                current: hash,
                previous: current.map(|v| v.current),
            },
        )?;
    }
    Ok(package)
}
fn verify_version(directory: &Path, keys: &[String]) -> Result<Package, String> {
    let data = fs::read(directory.join("bundle.tuneplugin")).map_err(|e| e.to_string())?;
    let signature =
        fs::read_to_string(directory.join("bundle.minisig")).map_err(|e| e.to_string())?;
    if directory.file_name().and_then(|v| v.to_str()) != Some(sha(&data).as_str()) {
        return Err("version digest differs from archive".into());
    }
    verify_signature(&data, &signature, keys)?;
    let (package, _) = unpack(&data, host_target())?;
    for (name, hash) in &package.files {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_EXPANDED {
            return Err("invalid installed file".into());
        }
        if sha(&fs::read(path).map_err(|e| e.to_string())?) != *hash {
            return Err(format!("installed file changed: {name}"));
        }
    }
    Ok(package)
}
pub fn rollback(root: &Path, id: &str, keys: &[String]) -> Result<Active, String> {
    if !valid_id(id) {
        return Err("invalid plugin id".into());
    }
    let directory = root.join(id);
    let current = active(&directory)?.ok_or("plugin not installed")?;
    let previous = current.previous.ok_or("no previous version")?;
    verify_version(&directory.join("versions").join(&previous), keys)?;
    let next = Active {
        current: previous,
        previous: Some(current.current),
    };
    atomic_state(&directory, &next)?;
    Ok(next)
}
/// Verify the retained archive, signature and extracted bytes on EVERY startup
/// before executing any constructor in the library. The Arc pins code in use.
pub fn load(
    root: &Path,
    id: &str,
    keys: &[String],
) -> Result<std::sync::Arc<crate::Library>, String> {
    if !valid_id(id) {
        return Err("invalid plugin id".into());
    }
    let directory = root.join(id);
    let state = active(&directory)?.ok_or("plugin not installed")?;
    let version = directory.join("versions").join(state.current);
    let package = verify_version(&version, keys)?;
    if package.manifest.id != id {
        return Err("installed plugin id differs".into());
    }
    let library = unsafe { crate::Library::load_trusted(&version.join(package.binary)) }?;
    if serde_json::to_value(&library.manifest).map_err(|e| e.to_string())?
        != serde_json::to_value(&package.manifest).map_err(|e| e.to_string())?
    {
        return Err("embedded manifest differs from signed package".into());
    }
    Ok(library)
}
pub fn installed_ids(root: &Path) -> Result<Vec<String>, String> {
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if valid_id(&id) && entry.path().join("active.json").is_file() {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}
pub fn deactivate(root: &Path, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err("invalid plugin id".into());
    }
    match fs::remove_file(root.join(id).join("active.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
pub fn version_directory(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err("invalid plugin id".into());
    }
    let directory = root.join(id);
    Ok(directory
        .join("versions")
        .join(active(&directory)?.ok_or("plugin not installed")?.current))
}
/// Only signed, inventory-listed UI assets. Returned bytes come from the
/// verified archive itself, so disk substitutions cannot affect the response.
pub fn read_asset(root: &Path, id: &str, name: &str, keys: &[String]) -> Result<Vec<u8>, String> {
    let name = format!("ui/{name}");
    if !safe_path(&name) {
        return Err("invalid asset path".into());
    }
    let version = version_directory(root, id)?;
    let bytes = fs::read(version.join("bundle.tuneplugin")).map_err(|e| e.to_string())?;
    let signature =
        fs::read_to_string(version.join("bundle.minisig")).map_err(|e| e.to_string())?;
    verify_signature(&bytes, &signature, keys)?;
    let (package, mut entries) = unpack(&bytes, host_target())?;
    if package.manifest.id != id {
        return Err("plugin id mismatch".into());
    }
    entries
        .remove(&name)
        .ok_or("asset not in signed inventory".into())
}
