//! Runtime provisioning of the onnxruntime shared library for the acoustic
//! Smart Radio.
//!
//! `ort` is built with `load-dynamic`, so the crate compiles on every target
//! with no onnxruntime binary — but the shared lib has to exist at runtime.
//! ort's own prebuilt dists are *static* archives (`libonnxruntime.a`), useless
//! for dynamic loading, so instead we fetch Microsoft's official onnxruntime
//! release archives, which ship real shared libs (`.so`/`.dylib`/`.dll`) as
//! plain `.tgz`/`.zip`. We verify each against a pinned SHA-256, unpack it, cache
//! it next to the model, and hand the lib path to [`ort::init_from`].
//!
//! onnxruntime 1.21.0 is pinned — it exposes ORT C API ≥ 17, the minimum the
//! `ort` 2.0.0-rc.13 bindings require, so the ABI matches.

use std::path::{Path, PathBuf};

use tokio::sync::OnceCell;

/// Process-global guard so onnxruntime is initialised into `ort` exactly once,
/// before any `Session` is built — no matter which subsystem reaches it first
/// (the background audio-embedding sweep or the natural-language search
/// endpoint). `ort::init_from().commit()` must run once and only once per
/// process; a second commit is an error, so both paths funnel through here.
static RUNTIME_LOADED: OnceCell<()> = OnceCell::const_new();

/// Provision (download+verify+unpack on first use) the onnxruntime shared lib
/// under `cache_root` and load it globally into `ort`, exactly once for the
/// process. Concurrent callers coalesce onto a single init; on failure the guard
/// stays unset so a later call retries.
pub async fn ensure_loaded(cache_root: &Path) -> Result<(), String> {
    RUNTIME_LOADED
        .get_or_try_init(|| async {
            let dylib = ensure_runtime(cache_root).await?;
            // Ceinture et bretelles pour le plafond de fils. `ort` documente que
            // `with_intra_threads` est SANS EFFET si onnxruntime a été compilé
            // avec OpenMP — et c'est justement les binaires préconstruits de
            // Microsoft que l'on provisionne. Dans ce cas seule
            // `OMP_NUM_THREADS` compte, et elle doit être posée AVANT que le
            // moteur ne crée son pool, donc avant `init_from`.
            //
            // On ne l'écrase jamais : un opérateur qui l'a réglée à la main sur
            // sa machine a le dernier mot. Sinon, la moitié des cœurs — le même
            // arbitrage que le réglage `equilibre` de la passe acoustique.
            if std::env::var_os("OMP_NUM_THREADS").is_none() {
                let half = (std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(2)
                    / 2)
                .max(1);
                // SAFETY: on est avant toute création de pool par onnxruntime,
                // et `RUNTIME_LOADED` garantit qu'un seul fil passe ici.
                unsafe { std::env::set_var("OMP_NUM_THREADS", half.to_string()) };
                tracing::info!(omp_num_threads = half, "audio_runtime_omp_capped");
            }
            ort::init_from(&dylib)
                .map_err(|e| format!("ort init_from {}: {e}", dylib.display()))?
                .commit();
            tracing::info!(dylib = %dylib.display(), "audio_runtime_loaded");
            Ok::<(), String>(())
        })
        .await
        .map(|_| ())
}

/// A platform's prebuilt onnxruntime: where to get it, its archive checksum, and
/// the canonical shared-lib filename to load out of it.
struct Dist {
    url: &'static str,
    /// SHA-256 of the release archive (computed from the pinned v1.21.0 assets).
    sha256: &'static str,
    /// The dynamic library `ort` must load, as named inside the archive's `lib/`
    /// dir (the canonical unversioned name — a symlink to the real file on unix).
    lib: &'static str,
}

// Microsoft onnxruntime v1.21.0 CPU builds. Checksums computed from the assets at
// github.com/microsoft/onnxruntime/releases/tag/v1.21.0.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const DIST: Option<Dist> = Some(Dist {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.21.0/onnxruntime-linux-x64-1.21.0.tgz",
    sha256: "7485c7e7aac6501b27e353dcbe068e45c61ab51fbaf598d13970dfae669d20bf",
    lib: "libonnxruntime.so",
});
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const DIST: Option<Dist> = Some(Dist {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.21.0/onnxruntime-linux-aarch64-1.21.0.tgz",
    sha256: "4508084bde1232ee1ab4b6fad2155be0ea2ccab1c1aae9910ddb3fb68a60805e",
    lib: "libonnxruntime.so",
});
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const DIST: Option<Dist> = Some(Dist {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.21.0/onnxruntime-osx-arm64-1.21.0.tgz",
    sha256: "5c3f2064ee97eb7774e87f396735c8eada7287734f1bb7847467ad30d4036115",
    lib: "libonnxruntime.dylib",
});
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DIST: Option<Dist> = Some(Dist {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.21.0/onnxruntime-osx-x86_64-1.21.0.tgz",
    sha256: "8305afd2d75ee5702844a23b099d41885af30ad3d1b4cf3d8d795e3d8c1f9396",
    lib: "libonnxruntime.dylib",
});
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const DIST: Option<Dist> = Some(Dist {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.21.0/onnxruntime-win-x64-1.21.0.zip",
    sha256: "5c07bb2805cd666dda75fa9bfa60e75f2f90d478b952298dd9d55c00740d81bf",
    lib: "onnxruntime.dll",
});
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
const DIST: Option<Dist> = None;

/// Ensure the onnxruntime shared library is present under `cache_root`,
/// downloading + checksum-verifying + unpacking the release archive on first use.
/// Returns the path to the lib, ready for [`ort::init_from`].
///
/// Idempotent: an already-unpacked lib is trusted (the archive was verified on
/// the run that wrote it). Unpacking goes to a `.part` staging dir that is
/// renamed into place, so a killed download never leaves a half-unpacked runtime.
pub async fn ensure_runtime(cache_root: &Path) -> Result<PathBuf, String> {
    let dist = DIST.ok_or_else(|| {
        format!(
            "no prebuilt onnxruntime for this platform ({}/{}); acoustic Smart Radio \
             stays disabled here",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })?;

    let dir = cache_root.join("onnxruntime");
    // Canonical lib path: cached flat under `<dir>/<lib>` after the first run.
    let cached = dir.join(dist.lib);
    if cached.exists() {
        return Ok(cached);
    }
    std::fs::create_dir_all(cache_root)
        .map_err(|e| format!("mkdir {}: {e}", cache_root.display()))?;

    tracing::info!(url = dist.url, dest = %dir.display(), "audio_runtime_downloading");
    // Voir `crate::http::client` : jamais `reqwest::get`, dont le client par
    // défaut dépend d'un vérificateur TLS que l'Android FFI n'initialise pas.
    let bytes = crate::http::client::long_timeout()
        .get(dist.url)
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download status: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download body: {e}"))?;

    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::new_with_prefix(&bytes).finalize());
    if got != dist.sha256 {
        return Err(format!(
            "checksum mismatch: got {got}, want {}",
            dist.sha256
        ));
    }

    // Unpack + locate the lib off the async runtime — pure CPU/blocking work.
    let staging = dir.with_extension("part");
    let archive = bytes.to_vec();
    let is_zip = dist.url.ends_with(".zip");
    let lib_name = dist.lib;
    let staging_task = staging.clone();
    let lib_path = tokio::task::spawn_blocking(move || {
        unpack(&archive, is_zip, &staging_task)?;
        find_lib(&staging_task, lib_name).ok_or_else(|| format!("{lib_name} not found in archive"))
    })
    .await
    .map_err(|e| format!("unpack task: {e}"))??;

    // Flatten the found lib (+ its sibling dir contents) to `<dir>/`. We move the
    // whole containing dir so versioned/symlinked siblings travel with it, then
    // point at the canonical name inside.
    let lib_dir = lib_path.parent().unwrap_or(&staging).to_path_buf();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&lib_dir, &dir).map_err(|e| format!("publish runtime dir: {e}"))?;
    let _ = std::fs::remove_dir_all(&staging);

    if !cached.exists() {
        return Err(format!("{} missing after unpack", cached.display()));
    }
    tracing::info!(lib = %cached.display(), "audio_runtime_ready");
    Ok(cached)
}

/// Unpack a `.tgz` (gzip+tar) or `.zip` archive fully into `out`, preserving its
/// internal directory layout and unix symlinks (so `libonnxruntime.so` → the
/// versioned real file resolves in place).
fn unpack(archive: &[u8], is_zip: bool, out: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
    if is_zip {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
            .map_err(|e| format!("open zip: {e}"))?;
        zip.extract(out).map_err(|e| format!("extract zip: {e}"))?;
    } else {
        let dec = flate2::read::GzDecoder::new(archive);
        let mut ar = tar::Archive::new(dec);
        ar.set_preserve_permissions(true);
        ar.unpack(out).map_err(|e| format!("extract tgz: {e}"))?;
    }
    Ok(())
}

/// Recursively find the file named exactly `name` under `root`, skipping macOS
/// `.dSYM` debug bundles (which nest a same-named binary). Returns the first
/// match — the archives contain a single canonical lib.
fn find_lib(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = path.file_name()?.to_string_lossy().into_owned();
        if fname.ends_with(".dSYM") {
            continue;
        }
        if path.is_dir() {
            if let Some(found) = find_lib(&path, name) {
                return Some(found);
            }
        } else if fname == name {
            return Some(path);
        }
    }
    None
}
