//! On-disk cache for pre-transcoded renditions.
//!
//! Network outputs (DLNA/UPnP) need a fully-encoded file with Content-Length +
//! Range support, so the orchestrator decodes and re-encodes the whole source
//! before playback can start — 30+ seconds for a hi-res track over a NAS.
//! Nothing reused that work: the temp file used a random UUID name and was
//! deleted when the stream session ended, so replaying a track (or a burst of
//! superseded taps on the same track) re-transcoded from scratch every time.
//!
//! This module gives those files a **deterministic** name derived from
//! everything that affects the encoded bytes, so an identical request finds the
//! finished file and serves it instantly. Cache files live in a root of their
//! own **per account** — `racine_de_travail("tune-tcache")`, i.e.
//! `…/tune-tcache-<uid>/` (#5133) — and use the `tune-tcache-`
//! prefix, which `streamer::is_temp_transcode_file` does NOT match, so the
//! per-session and startup cleanups leave them alone — their lifetime is
//! governed here by [`evict`] (bounded total size, LRU).
//!
//! EQ is intentionally out of the key: a zone EQ curve changes the output, and
//! hashing the filter set here would be fragile. Callers must pass `None`-cache
//! (skip the cache) whenever a zone EQ is active.

use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};
use tracing::info;

/// Filename prefix for cached renditions. Deliberately distinct from the
/// `tune-transcode-` family so the streamer's cleanup never deletes these.
const CACHE_PREFIX: &str = "tune-tcache-";

/// A file modified within this window is never evicted — it may still be
/// streaming to a slow renderer. Matches the streamer's 1800s session GC.
const EVICT_MIN_AGE_SECS: u64 = 1800;

/// Default cache size cap (MiB) if `TUNE_TRANSCODE_CACHE_MAX_MB` is unset.
const DEFAULT_MAX_MB: u64 = 4096;

/// Minimum size (bytes) for a cache file to count as a completed transcode.
const MIN_VALID_BYTES: u64 = 1024;

/// Deterministic cache path for a transcoded rendition, or `None` if the
/// source metadata can't be read (→ caller falls back to a fresh transcode).
///
/// The key covers the source path, its mtime and size (so replacing or
/// re-tagging the file invalidates the entry), plus the target container and
/// the output sample rate / bit depth / channel count. It does NOT cover EQ —
/// see the module docs.
pub fn cache_path(
    source: &str,
    out_ext: &str,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
) -> Option<String> {
    cache_path_dsp(source, out_ext, sample_rate, bit_depth, channels, None)
}

/// Version de l'algorithme DSP entrant dans la clé. À incrémenter dès qu'une
/// mise à jour de l'égaliseur, de la convolution ou du ReplayGain change les
/// octets rendus pour une même empreinte : sinon une rendition d'hier
/// resservirait l'ancien traitement (LAT-F2).
pub const DSP_CACHE_VERSION: u32 = 1;

/// Empreinte du traitement appliqué à la rendition : la représentation
/// canonique de l'égaliseur (JSON du profil), le facteur ReplayGain (ses
/// octets IEEE, pas une décimale arrondie) et le CONTENU de la réponse
/// impulsionnelle, chacun derrière son séparateur de domaine, plus
/// [`DSP_CACHE_VERSION`]. `None` quand aucun traitement n'est en jeu : la clé
/// reste alors exactement celle d'avant LAT-F2, et le cache existant est
/// conservé.
pub fn empreinte_dsp(
    eq_profile_json: Option<&str>,
    replaygain_factor: Option<f64>,
    impulse_response: Option<&[u8]>,
) -> Option<[u8; 32]> {
    empreinte_dsp_v(
        DSP_CACHE_VERSION,
        eq_profile_json,
        replaygain_factor,
        impulse_response,
    )
}

fn empreinte_dsp_v(
    version: u32,
    eq_profile_json: Option<&str>,
    replaygain_factor: Option<f64>,
    impulse_response: Option<&[u8]>,
) -> Option<[u8; 32]> {
    if eq_profile_json.is_none() && replaygain_factor.is_none() && impulse_response.is_none() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(b"dsp-version\0");
    h.update(version.to_le_bytes());
    if let Some(eq) = eq_profile_json {
        h.update(b"eq\0");
        h.update(eq.as_bytes());
        h.update([0u8]);
    }
    if let Some(rg) = replaygain_factor {
        h.update(b"rg\0");
        h.update(rg.to_bits().to_le_bytes());
    }
    if let Some(ir) = impulse_response {
        h.update(b"ir\0");
        h.update(Sha256::digest(ir));
    }
    Some(h.finalize().into())
}

/// L'empreinte de cache d'une TRANCHE de fichier (#3631).
///
/// 🔴 **Sans elle, le cache confondrait les pistes d'un même album CUE.** La
/// clé de [`cache_path_dsp`] est bâtie sur le fichier SOURCE — or les quinze
/// pistes d'une image partagent ce fichier, à la mtime et à la taille près.
/// La première rendition mise en cache serait donc servie pour les quatorze
/// autres : quinze pistes différentes, un seul et même morceau.
///
/// Rend l'empreinte de traitement inchangée quand il n'y a pas de tranche : une
/// piste ordinaire garde exactement la clé qu'elle avait.
pub fn empreinte_avec_tranche(
    dsp: Option<[u8; 32]>,
    tranche: Option<(u64, Option<u64>)>,
) -> Option<[u8; 32]> {
    let Some((debut_ms, fin_ms)) = tranche else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"tranche\0");
    h.update(debut_ms.to_le_bytes());
    h.update(fin_ms.unwrap_or(u64::MAX).to_le_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

/// Clé de cache d'une rendition locale, avec l'empreinte du traitement quand
/// il y en a un : une rendition par réglage, rejouée instantanément, au lieu
/// de retranscoder à chaque écoute dès qu'un EQ est actif (LAT-F2).
pub fn cache_path_dsp(
    source: &str,
    out_ext: &str,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
    empreinte_dsp: Option<&[u8; 32]>,
) -> Option<String> {
    let meta = std::fs::metadata(source).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update(mtime.to_le_bytes());
    h.update(meta.len().to_le_bytes());
    h.update(out_ext.as_bytes());
    h.update(sample_rate.to_le_bytes());
    h.update(bit_depth.to_le_bytes());
    h.update(channels.to_le_bytes());
    if let Some(empreinte) = empreinte_dsp {
        h.update(b"dsp\0");
        h.update(empreinte);
    }
    let hex = format!("{:x}", h.finalize());
    let name = format!("{CACHE_PREFIX}{}.{out_ext}", &hex[..32]);
    chemin_du_rendu(&name)
}

/// Deterministic cache path for a transcoded *streaming* rendition (Tidal /
/// Qobuz HI-RES DASH), keyed by stream identity rather than a source file.
///
/// Unlike [`cache_path`], there is no stable source file to stat: the DASH fMP4
/// is downloaded to a random temp name, renamed to `.decoding`, then deleted, so
/// its path/mtime/size are meaningless as a key. Instead we hash the durable
/// stream identity — `service | source_id | out_ext | sample_rate | bit_depth |
/// channels` — which is what actually determines the transcoded bytes. This is
/// independent of any `metadata()` call: a HI-RES track resolved for the same
/// zone always maps to the same cached FLAC/WAV. `None` only when the
/// account's cache root is unusable (see [`racine_preparee`]): the caller then
/// transcodes without caching.
///
/// Shares the `tune-tcache-` prefix so [`is_hit`], [`touch`] and [`evict`] cover
/// these entries identically. EQ is out of the key (see the module docs): the
/// caller must skip the cache when a zone EQ is active. `bit_depth` must be the
/// pre-decode *negotiated* target (16 when capped to WAV, else the source depth)
/// so warm and play agree before the expensive decode.
pub fn cache_path_streaming(
    service: &str,
    source_id: &str,
    out_ext: &str,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
) -> Option<String> {
    let mut h = Sha256::new();
    h.update(service.as_bytes());
    h.update([0u8]); // domain separator so "ab|c" ≠ "a|bc"
    h.update(source_id.as_bytes());
    h.update([0u8]);
    h.update(out_ext.as_bytes());
    h.update(sample_rate.to_le_bytes());
    h.update(bit_depth.to_le_bytes());
    h.update(channels.to_le_bytes());
    let hex = format!("{:x}", h.finalize());
    let name = format!("{CACHE_PREFIX}{}.{out_ext}", &hex[..32]);
    chemin_du_rendu(&name)
}

/// Étiquette de la racine du cache : `racine_de_travail(ETIQUETTE_RACINE)` rend
/// `…/tune-tcache-<uid>`, un dossier par compte (#5133).
const ETIQUETTE_RACINE: &str = "tune-tcache";

/// Le chemin du rendu `nom` dans la racine du compte courant.
fn chemin_du_rendu(nom: &str) -> Option<String> {
    chemin_dans(
        &crate::chemins_de_travail::racine_de_travail(ETIQUETTE_RACINE),
        crate::chemins_de_travail::uid_courant(),
        nom,
    )
}

/// Le chemin du rendu `nom` sous `racine`, une fois la racine préparée pour
/// `proprietaire`. Forme testable de [`chemin_du_rendu`] : la racine est
/// passée, donc un test peut jouer deux comptes sans en avoir deux.
fn chemin_dans(racine: &std::path::Path, proprietaire: u32, nom: &str) -> Option<String> {
    let racine = racine_preparee(racine, proprietaire)?;
    Some(racine.join(nom).to_string_lossy().to_string())
}

/// Crée la racine du cache si besoin (`0700` sous Unix) et ne la rend que si
/// elle est **à nous** : un vrai dossier (pas un lien) qui appartient à
/// `proprietaire`.
///
/// Avant #5133, les rendus vivaient à plat dans le dossier temporaire partagé,
/// sous un nom qui ne dépendait que de la piste : deux comptes qui rendaient la
/// même piste partageaient le même fichier, et le second ne pouvait pas y
/// renommer le sien. Une racine déjà là mais qui appartient à un autre compte
/// ramènerait ce partage : on n'y lit ni n'y écrit, on transcode sans cache.
fn racine_preparee(racine: &std::path::Path, proprietaire: u32) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(racine)
        {
            tracing::debug!(racine = %racine.display(), error = %e, "transcode_cache_root_unavailable");
            return None;
        }
        let m = std::fs::symlink_metadata(racine).ok()?;
        if !m.file_type().is_dir() || m.uid() != proprietaire {
            tracing::warn!(
                racine = %racine.display(),
                "transcode_cache_root_not_owned — cache désactivé pour ce compte"
            );
            return None;
        }
        if m.permissions().mode() & 0o077 != 0 {
            let _ = std::fs::set_permissions(racine, std::fs::Permissions::from_mode(0o700));
        }
        Some(racine.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        // Sous Windows, le dossier temporaire est déjà propre au compte.
        let _ = proprietaire;
        std::fs::create_dir_all(racine).ok()?;
        Some(racine.to_path_buf())
    }
}

/// True when `path` holds a completed transcode (exists, non-trivial size).
pub fn is_hit(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() >= MIN_VALID_BYTES)
        .unwrap_or(false)
}

/// Mark a reused entry as recently used (bumps mtime) so LRU eviction keeps
/// hot files. Best-effort — a failure just means slightly less accurate LRU.
pub fn touch(path: &str) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Configured cache size cap in bytes (`TUNE_TRANSCODE_CACHE_MAX_MB`).
fn max_bytes() -> u64 {
    std::env::var("TUNE_TRANSCODE_CACHE_MAX_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_MB)
        .saturating_mul(1024 * 1024)
}

/// Evict least-recently-used cache files until the total is under the
/// configured cap (`TUNE_TRANSCODE_CACHE_MAX_MB`). Files touched within
/// `EVICT_MIN_AGE_SECS` are skipped (possibly in use). Pure filesystem I/O —
/// call from `spawn_blocking`, not the async executor.
pub fn evict() {
    evict_with_cap(max_bytes());
}

/// Eviction with an explicit byte cap (the testable core of [`evict`]).
///
/// Balaie la racine du compte courant (#5133), puis purge dans son dossier
/// parent — le dossier temporaire — les rendus posés à plat par les versions
/// d'avant, **seulement** ceux du compte courant.
fn evict_with_cap(cap: u64) {
    let uid = crate::chemins_de_travail::uid_courant();
    let racine = crate::chemins_de_travail::racine_de_travail(ETIQUETTE_RACINE);
    evict_in(&racine, cap, uid);
    if let Some(ancien) = racine.parent() {
        purger_anciens_rendus(ancien, uid);
    }
}

/// Supprime, dans `dossier`, les rendus `tune-tcache-*` posés à plat par les
/// versions d'avant #5133 : des fichiers ordinaires (ni lien, ni dossier — la
/// racine `tune-tcache-<uid>` porte le même préfixe) qui appartiennent à `uid`
/// et assez vieux pour ne plus être servis. Ceux d'un autre compte restent.
fn purger_anciens_rendus(dossier: &std::path::Path, uid: u32) {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return;
    };
    let maintenant = SystemTime::now();
    for entree in entrees.flatten() {
        if !entree
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(CACHE_PREFIX))
        {
            continue;
        }
        // `DirEntry::metadata` ne suit pas les liens symboliques.
        let Ok(m) = entree.metadata() else { continue };
        if !m.is_file() || !appartient_a(&m, uid) {
            continue;
        }
        let age = maintenant
            .duration_since(m.modified().unwrap_or(maintenant))
            .unwrap_or(Duration::ZERO);
        if age >= Duration::from_secs(EVICT_MIN_AGE_SECS) {
            let _ = std::fs::remove_file(entree.path());
        }
    }
}
/// Le cœur de l'éviction, avec son dossier et son propriétaire **passés**.
///
/// Les rendus vivent dans la racine du compte (#5133) ; avant, ils vivaient à
/// plat dans le dossier temporaire partagé (#4770). Le filtre reste : seuls les fichiers qui appartiennent à `uid`
/// comptent dans le total et peuvent être supprimés : sans ce filtre, le
/// plafond comparait un total gonflé par le cache du voisin, et, sur un
/// `TMPDIR` non sticky, la boucle effaçait les fichiers d'un autre serveur.
///
/// `uid` est un paramètre, pas un appel à `uid_courant()` ici même : c'est ce
/// qui permet au témoin de jouer « un autre compte » sans avoir deux comptes.
/// Sous Windows, `temp_dir()` est déjà propre au compte : pas de filtre.
fn evict_in(dir: &std::path::Path, cap: u64, uid: u32) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = SystemTime::now();
    let mut files: Vec<(std::path::PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with(CACHE_PREFIX) {
            continue;
        }
        if let Ok(m) = entry.metadata() {
            if !m.is_file() || !appartient_a(&m, uid) {
                continue;
            }
            total += m.len();
            files.push((entry.path(), m.len(), m.modified().unwrap_or(now)));
        }
    }
    if total <= cap {
        return;
    }
    // Least-recently-used first.
    files.sort_by_key(|(_, _, mt)| *mt);
    let mut removed: u64 = 0;
    for (path, size, mtime) in files {
        if total <= cap {
            break;
        }
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age < Duration::from_secs(EVICT_MIN_AGE_SECS) {
            continue; // may still be serving
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += size;
        }
    }
    if removed > 0 {
        info!(
            removed_bytes = removed,
            remaining_bytes = total,
            "transcode_cache_evicted"
        );
    }
}
/// Vrai quand le fichier décrit par `m` appartient au compte `uid`.
#[cfg(unix)]
fn appartient_a(m: &std::fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.uid() == uid
}
#[cfg(not(unix))]
fn appartient_a(_m: &std::fs::Metadata, _uid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A unique real file so metadata() succeeds; content size varies the key.
    ///
    /// Rendu comme `ScratchFile` et non comme `String` : le fichier vit à la
    /// racine de `temp_dir()` — c'est là que l'éviction balaie, un
    /// sous-dossier retirerait sa substance au test — et il doit malgré tout
    /// disparaître à la sortie du test, panique comprise (#3030).
    fn tmp_source(bytes: usize) -> crate::test_scratch::ScratchFile {
        let p = crate::test_scratch::scratch_file("tcache-src", &format!("-{bytes}.flac"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
        p
    }

    #[test]
    fn cache_path_is_deterministic_and_param_sensitive() {
        let fichier = tmp_source(100);
        let src = fichier.to_string_lossy();
        let a = cache_path(&src, "flac", 44100, 16, 2).unwrap();
        let b = cache_path(&src, "flac", 44100, 16, 2).unwrap();
        assert_eq!(a, b, "same inputs → same path");
        assert!(a.contains("tune-tcache-"));
        assert!(a.ends_with(".flac"));

        // Any output-affecting param changes the path.
        assert_ne!(a, cache_path(&src, "wav", 44100, 16, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 48000, 16, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 44100, 24, 2).unwrap());
        assert_ne!(a, cache_path(&src, "flac", 44100, 16, 1).unwrap());
    }

    /// LAT-F2 : sans traitement, la clé est celle d'avant (le cache existant
    /// survit) ; avec, chaque composante de l'empreinte — égaliseur,
    /// ReplayGain, contenu de l'IR, version de l'algorithme — change la clé.
    #[test]
    fn la_cle_porte_le_dsp_et_chaque_composante_l_invalide() {
        let source = crate::test_scratch::scratch_file("tune-tcache-dsp", ".flac");
        std::fs::write(&source, b"pas un vrai flac").unwrap();
        let src = source.to_string_lossy().to_string();

        let nu = cache_path(&src, "flac", 44_100, 16, 2).unwrap();
        assert_eq!(
            cache_path_dsp(&src, "flac", 44_100, 16, 2, None).unwrap(),
            nu
        );
        assert!(
            empreinte_dsp(None, None, None).is_none(),
            "sans traitement, pas d'empreinte"
        );

        let eq_a = empreinte_dsp(Some(r#"{"bass_gain_db":3.0}"#), None, None).unwrap();
        let eq_b = empreinte_dsp(Some(r#"{"bass_gain_db":4.0}"#), None, None).unwrap();
        let rg = empreinte_dsp(None, Some(0.5), None).unwrap();
        let rg2 = empreinte_dsp(None, Some(0.5000001), None).unwrap();
        let ir_a = empreinte_dsp(None, None, Some(b"RIFF....ir-a")).unwrap();
        let ir_b = empreinte_dsp(None, None, Some(b"RIFF....ir-b")).unwrap();
        let tout = empreinte_dsp(
            Some(r#"{"bass_gain_db":3.0}"#),
            Some(0.5),
            Some(b"RIFF....ir-a"),
        )
        .unwrap();

        let cle = |e: &[u8; 32]| cache_path_dsp(&src, "flac", 44_100, 16, 2, Some(e)).unwrap();
        let cles = [
            cle(&eq_a),
            cle(&eq_b),
            cle(&rg),
            cle(&rg2),
            cle(&ir_a),
            cle(&ir_b),
            cle(&tout),
        ];
        for c in &cles {
            assert_ne!(
                *c, nu,
                "une rendition traitee ne doit jamais prendre la cle du signal brut"
            );
        }
        let mut distinctes = cles.to_vec();
        distinctes.sort();
        distinctes.dedup();
        assert_eq!(
            distinctes.len(),
            cles.len(),
            "deux traitements differents partagent une cle"
        );
        assert_eq!(
            cle(&eq_a),
            cle(&empreinte_dsp(Some(r#"{"bass_gain_db":3.0}"#), None, None).unwrap()),
            "meme traitement, meme cle"
        );
        assert_ne!(
            empreinte_dsp_v(1, Some("{}"), None, None),
            empreinte_dsp_v(2, Some("{}"), None, None),
            "une nouvelle version de l'algorithme doit invalider les renditions"
        );
    }

    #[test]
    fn cache_path_none_for_missing_source() {
        assert!(cache_path("/no/such/file.flac", "flac", 44100, 16, 2).is_none());
    }

    #[test]
    fn cache_path_streaming_is_deterministic_and_param_sensitive() {
        let a = cache_path_streaming("tidal", "12345", "flac", 96000, 24, 2);
        let b = cache_path_streaming("tidal", "12345", "flac", 96000, 24, 2);
        assert_eq!(a, b, "same inputs → same path");
        // Shares the cache prefix so is_hit / touch / evict cover it.
        let chemin = a.as_deref().expect("racine du cache utilisable");
        assert!(chemin.contains("tune-tcache-"));
        assert!(chemin.ends_with(".flac"));

        // Every identity/output param changes the path.
        assert_ne!(
            a,
            cache_path_streaming("qobuz", "12345", "flac", 96000, 24, 2)
        );
        assert_ne!(
            a,
            cache_path_streaming("tidal", "67890", "flac", 96000, 24, 2)
        );
        assert_ne!(
            a,
            cache_path_streaming("tidal", "12345", "wav", 96000, 24, 2)
        );
        assert_ne!(
            a,
            cache_path_streaming("tidal", "12345", "flac", 44100, 24, 2)
        );
        assert_ne!(
            a,
            cache_path_streaming("tidal", "12345", "flac", 96000, 16, 2)
        );
        assert_ne!(
            a,
            cache_path_streaming("tidal", "12345", "flac", 96000, 24, 1)
        );
    }

    #[test]
    fn cache_path_streaming_domain_separated() {
        // The NUL separators prevent field-boundary collisions between
        // service+source_id that would otherwise concatenate to the same bytes.
        assert_ne!(
            cache_path_streaming("ti", "dal1", "flac", 96000, 24, 2),
            cache_path_streaming("tid", "al1", "flac", 96000, 24, 2),
        );
    }

    #[test]
    fn is_hit_requires_completed_file() {
        let p = crate::test_scratch::scratch_file("tune-tcache-hit", ".flac");
        let ps = p.to_string_lossy().to_string();
        assert!(!is_hit(&ps), "missing → miss");
        std::fs::write(&p, vec![0u8; 10]).unwrap();
        assert!(!is_hit(&ps), "tiny file → miss");
        std::fs::write(&p, vec![0u8; 2048]).unwrap();
        assert!(is_hit(&ps), "completed file → hit");
    }

    #[test]
    fn evict_never_removes_recent_files() {
        // A freshly written cache file is younger than EVICT_MIN_AGE_SECS, so
        // even with a 0-byte cap it must survive (it may be streaming).
        let p = crate::test_scratch::scratch_file("tune-tcache-recent", ".flac");
        std::fs::write(&p, vec![0u8; 4096]).unwrap();
        // Cap of 0 forces eviction pressure; the file is younger than
        // EVICT_MIN_AGE_SECS so it must still survive.
        evict_with_cap(0);
        assert!(p.exists(), "recent file must not be evicted");
    }
    /// Témoin de #4770 : l'éviction ne touche pas le cache d'un AUTRE compte.
    ///
    /// Un seul compte suffit : le fichier est à nous, et on évince « au nom »
    /// d'un autre UID. Deux assertions, dans cet ordre :
    /// 1. au nom d'un autre compte, un rendu vieux de deux heures sous un
    ///    plafond nul SURVIT — sans le filtre, il est supprimé ;
    /// 2. au nom de son propriétaire, le même fichier PART — sans cette
    ///    contre-partie, le test passerait aussi si l'éviction ne supprimait
    ///    plus rien du tout.
    #[cfg(unix)]
    #[test]
    fn eviction_ne_touche_pas_le_cache_d_un_autre_compte() {
        let dossier = crate::test_scratch::scratch_dir("tcache-eviction-4770");
        let rendu = dossier.path().join(format!("{CACHE_PREFIX}voisin.flac"));
        std::fs::write(&rendu, vec![0u8; 4096]).unwrap();
        let vieux = SystemTime::now() - Duration::from_secs(2 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&rendu)
            .unwrap()
            .set_modified(vieux)
            .unwrap();
        let moi = crate::chemins_de_travail::uid_courant();
        let autre = moi.wrapping_add(1);

        evict_in(dossier.path(), 0, autre);
        assert!(
            rendu.exists(),
            "l'éviction d'un autre compte a supprimé un rendu qui n'est pas à lui"
        );

        evict_in(dossier.path(), 0, moi);
        assert!(
            !rendu.exists(),
            "le propriétaire doit pouvoir évincer son propre rendu (sinon le témoin ne prouve rien)"
        );
    }

    /// Témoin de #5133, côté production : un rendu est rangé dans la racine
    /// du compte courant (`racine_de_travail("tune-tcache")` → `…-<uid>`), pas
    /// à plat dans le dossier temporaire partagé. Sans le correctif, son
    /// dossier parent est le dossier temporaire lui-même.
    #[test]
    fn un_rendu_vit_dans_la_racine_du_compte_courant() {
        let fichier = tmp_source(2048);
        let src = fichier.to_string_lossy();
        let racine = crate::chemins_de_travail::racine_de_travail(ETIQUETTE_RACINE);
        let attendu = format!(
            "{ETIQUETTE_RACINE}-{}",
            crate::chemins_de_travail::uid_courant()
        );
        assert_eq!(
            racine.file_name().unwrap().to_string_lossy(),
            attendu,
            "la racine du cache ne porte pas l'UID du compte"
        );
        let local = cache_path(&src, "flac", 44_100, 16, 2).expect("chemin local");
        let flux =
            cache_path_streaming("tidal", "5133", "flac", 96_000, 24, 2).expect("chemin flux");
        for chemin in [&local, &flux] {
            assert_eq!(
                std::path::Path::new(chemin).parent(),
                Some(racine.as_path()),
                "le rendu {chemin} n'est pas rangé dans la racine du compte {racine:?}"
            );
        }
    }

    /// Témoin de #5133 : deux comptes qui rendent la MÊME piste ont deux
    /// fichiers distincts, chacun dans sa racine ; le second met bien son
    /// rendu en cache, et aucun ne voit ni ne lit le fichier de l'autre.
    ///
    /// Les deux UID sont simulés : les racines sont composées par
    /// `racine_de_travail_sous` avec 1000 et 1001, et préparées au nom du
    /// compte réel, qui crée bel et bien les deux dossiers.
    #[cfg(unix)]
    #[test]
    fn deux_comptes_ne_partagent_pas_le_rendu_d_une_meme_piste() {
        use crate::chemins_de_travail::{racine_de_travail_sous, uid_courant};
        let base = crate::test_scratch::scratch_dir("tcache-comptes-5133");
        let fichier = tmp_source(3000);
        let src = fichier.to_string_lossy();
        let piste = cache_path(&src, "flac", 44_100, 16, 2).expect("chemin de la piste");
        let nom = std::path::Path::new(&piste)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let moi = uid_courant();

        let racine_a = racine_de_travail_sous(base.path(), ETIQUETTE_RACINE, 1000);
        let racine_b = racine_de_travail_sous(base.path(), ETIQUETTE_RACINE, 1001);
        let chemin_a = chemin_dans(&racine_a, moi, &nom).expect("racine du compte A");
        let chemin_b = chemin_dans(&racine_b, moi, &nom).expect("racine du compte B");
        assert_ne!(chemin_a, chemin_b, "deux comptes partagent le même rendu");
        assert_eq!(
            std::path::Path::new(&chemin_a).parent(),
            Some(racine_a.as_path())
        );
        assert_eq!(
            std::path::Path::new(&chemin_b).parent(),
            Some(racine_b.as_path())
        );

        // Comme en production : écrire un temporaire, puis le renommer.
        let rendre = |chemin: &str, octet: u8| {
            let tmp = base.path().join(format!("tune-transcode-{octet}.flac"));
            std::fs::write(&tmp, vec![octet; 4096]).unwrap();
            std::fs::rename(&tmp, chemin)
        };

        // Le compte A rend la piste.
        rendre(&chemin_a, b'A').expect("mise en cache du compte A");
        assert!(is_hit(&chemin_a));

        // Le compte B ne voit pas le rendu de A…
        assert!(
            !is_hit(&chemin_b),
            "le compte B trouve en cache le rendu du compte A"
        );
        // …et met bien le sien en cache.
        rendre(&chemin_b, b'B').expect("le second compte n'a pas pu mettre son rendu en cache");
        assert!(is_hit(&chemin_b));

        // Chacun lit SON rendu.
        assert!(std::fs::read(&chemin_a).unwrap().iter().all(|&o| o == b'A'));
        assert!(std::fs::read(&chemin_b).unwrap().iter().all(|&o| o == b'B'));
    }

    /// Une racine qui existe déjà mais n'est pas à nous — autre propriétaire,
    /// ou lien symbolique — n'est pas utilisée : pas de cache plutôt qu'un
    /// cache partagé. Contre-partie : au nom de son propriétaire, la même
    /// racine est rendue, en `0700`.
    #[cfg(unix)]
    #[test]
    fn une_racine_qui_n_est_pas_a_nous_n_est_pas_utilisee() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::test_scratch::scratch_dir("tcache-racine-5133");
        let moi = crate::chemins_de_travail::uid_courant();
        let racine = base.path().join("tune-tcache-4242");

        assert!(
            racine_preparee(&racine, moi.wrapping_add(1)).is_none(),
            "une racine d'un autre compte a été acceptée"
        );
        assert!(chemin_dans(&racine, moi.wrapping_add(1), "x.flac").is_none());

        let rendue = racine_preparee(&racine, moi).expect("sa propre racine");
        assert_eq!(rendue, racine);
        let mode = std::fs::metadata(&racine).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "la racine est lisible par d'autres comptes");

        let lien = base.path().join("tune-tcache-lien");
        std::os::unix::fs::symlink(&racine, &lien).unwrap();
        assert!(
            racine_preparee(&lien, moi).is_none(),
            "un lien symbolique a été accepté comme racine"
        );
    }

    /// Les anciens rendus posés à plat (avant #5133) ne sont purgés que s'ils
    /// sont au compte courant et assez vieux ; la racine `tune-tcache-<uid>`,
    /// qui porte le même préfixe, n'est jamais touchée.
    #[cfg(unix)]
    #[test]
    fn les_anciens_rendus_a_plat_ne_partent_que_s_ils_sont_a_nous() {
        let dossier = crate::test_scratch::scratch_dir("tcache-anciens-5133");
        let vieux = SystemTime::now() - Duration::from_secs(2 * 3600);
        let ancien = dossier.path().join(format!("{CACHE_PREFIX}ancien.flac"));
        let recent = dossier.path().join(format!("{CACHE_PREFIX}recent.flac"));
        std::fs::write(&ancien, vec![0u8; 4096]).unwrap();
        std::fs::write(&recent, vec![0u8; 4096]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&ancien)
            .unwrap()
            .set_modified(vieux)
            .unwrap();
        let racine = dossier.path().join(format!("{ETIQUETTE_RACINE}-1000"));
        std::fs::create_dir(&racine).unwrap();
        std::fs::write(racine.join(format!("{CACHE_PREFIX}garde.flac")), b"x").unwrap();
        let moi = crate::chemins_de_travail::uid_courant();

        purger_anciens_rendus(dossier.path(), moi.wrapping_add(1));
        assert!(
            ancien.exists(),
            "l'ancien rendu d'un autre compte a été supprimé"
        );

        purger_anciens_rendus(dossier.path(), moi);
        assert!(
            !ancien.exists(),
            "l'ancien rendu du compte n'a pas été purgé"
        );
        assert!(recent.exists(), "un rendu récent a été purgé");
        assert!(racine.join(format!("{CACHE_PREFIX}garde.flac")).exists());
    }
}
