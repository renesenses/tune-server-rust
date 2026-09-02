use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rayon::prelude::*;
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use super::hasher::compute_audio_hash;
use crate::audio::support::{LibraryAudioSupport, UnsupportedLibraryAudio};
use crate::metadata::{TrackMetadata, tagless_fallback_no_props, try_read_metadata};

/// Maximum time allowed for reading metadata + computing hash for a single file.
/// Files on NAS over a flaky network can hang indefinitely; this prevents the
/// entire scan from stalling on a single corrupt or unreachable file.
// Large Hi-Res FLAC (24/96, big embedded art) on slow/network storage can take
// well over 10s just to read tags via lofty — 10s wrongly skipped them entirely
// (Progman: files dropped from the library). Give more headroom, and on timeout
// fall back to filename metadata instead of losing the file.
const FILE_TIMEOUT: Duration = Duration::from_secs(30);

// Slow network storage (a NAS, or an SSD hanging off a UPnP streamer accessed
// over the LAN — Pierre M's NAS, Philippe Landes' Hifi Rose RS130) regularly
// exceeds FILE_TIMEOUT on the *first* tag read but succeeds with more headroom.
// Falling straight back to filename-only metadata left those tracks with
// duration = 0, which breaks gapless end-detection (the track is cut short or
// the queue stops advancing). Retry once with a much larger budget before
// giving up, so the real duration/tags are recovered.
const RETRY_FILE_TIMEOUT: Duration = Duration::from_secs(90);

// The audio hash (duplicate detection) does NOT read the whole file: it MD5s
// a single 64 KB sample at the 25% offset (scanner/hasher.rs). Its real cost
// is open + seek + one read — cheap even for huge files. The generous budget
// exists because on a stalled NAS those three syscalls can hang like any
// other I/O, and hashing is best-effort: on timeout the track keeps its real
// tags and only the hash is skipped (Progman: stalled mount, not file size).
const HASH_TIMEOUT: Duration = Duration::from_secs(120);

// Per-file metadata reads are I/O-bound: each rayon task blocks on the tag read
// (lofty), frequently on high-latency network storage. The default rayon pool
// has only ~CPU-core-count threads, so effective concurrency — and throughput —
// was capped at the core count, which made a full scan ~10x slower than a
// tag-only indexer like MinimServer (Pierre M; Philippe Landes: 12h for 20200
// DSD tracks). Read metadata on a dedicated, higher-concurrency pool so many
// more per-file latencies overlap. Mirrors the 32-thread stat pool already used
// for the mtime pre-check (#619).
const SCAN_IO_CONCURRENCY: usize = 32;

/// Resolve the scan I/O concurrency, honouring an optional `TUNE_SCAN_IO_CONCURRENCY`
/// override. The fixed default (32) is a good fit for a fast SSD NAS, but the
/// sweet spot is storage-specific: a weak 2-bay HDD NAS (Synology DS218Play,
/// forum #1194) can be *slowed* by 32 concurrent reads scattered across the
/// platters (each file needs a seek for the tags at the start + a seek to 25%
/// for the dup-detection hash), while a high-latency share benefits from even
/// more. Rather than guess, let the operator tune it against their own NAS —
/// something we cannot benchmark centrally. Empty/invalid/zero → the default;
/// clamped to 1..=256 so a typo can't spawn a pathological number of OS threads.
pub fn scan_io_concurrency() -> usize {
    concurrence_depuis_reglage(std::env::var("TUNE_SCAN_IO_CONCURRENCY").ok().as_deref())
}

/// La décision, séparée de la LECTURE du réglage.
///
/// Fonction PURE, comme [`concurrence_pour_disque`] juste en dessous, et pour
/// une raison plus forte qu'un principe : son test l'éprouvait en écrivant
/// dans l'environnement du processus. Or `set_var` est `unsafe` depuis
/// l'édition 2024 parce qu'il n'est pas sûr entre fils d'exécution — et il
/// réécrit le bloc `environ` que TOUS les autres fils lisent au même moment.
/// Le parcours de bibliothèque en lit un à chaque fichier rencontré
/// (`is_tune_temp_file` appelle `std::env::temp_dir()`), donc le test faisait
/// courir la totalité du scanner contre une réécriture d'environnement.
///
/// Ce n'était pas théorique : mesuré sur Shrek, l'ajout de deux parcours de
/// test supplémentaires suffisait à faire tomber **dix** tests de parcours
/// d'un coup, tous avec « 0 fichier trouvé », dans la suite `--workspace`
/// seule. Le même test passait en `-p tune-core` isolé, et disparaissait avec
/// `--skip scan_io_concurrency_env_override`. Prendre le réglage en argument
/// supprime la course à sa source, sans rien retirer à la couverture.
fn concurrence_depuis_reglage(reglage: Option<&str>) -> usize {
    if let Some(n) = reglage
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .map(|n| n.clamp(1, 256))
    {
        return n;
    }
    concurrence_pour_disque(disque_rotatif())
}

/// Concurrence adaptée au type de disque, une fois le réglage manuel écarté.
///
/// Fonction PURE, pour qu'elle soit testable sans `/sys` ni disque : la sonde
/// système est séparée dans `disque_rotatif`.
///
/// Sur plateaux, 32 lectures concurrentes ne parallélisent rien — elles font
/// osciller une tête unique entre 32 endroits. Chaque fichier demande deux
/// déplacements (les tags au début, le hachage de déduplication à 25 %), donc
/// 64 déplacements en vol sur un seul actionneur. Mesuré chez Yacine, 49 488
/// fichiers sur un disque USB : **5,8 fichiers/s**, contre 44 000 en une
/// trentaine de secondes sur SSD dans notre propre README (#1948).
///
/// `None` — type inconnu, ou pas Linux — garde la valeur d'origine : on ne
/// dégrade pas un SSD par prudence mal placée.
pub(crate) fn concurrence_pour_disque(rotatif: Option<bool>) -> usize {
    match rotatif {
        Some(true) => 4,
        _ => SCAN_IO_CONCURRENCY,
    }
}

/// Le stockage des dossiers de musique est-il sur plateaux ?
///
/// Lit `/sys/dev/block/<major>:<minor>/queue/rotational` — `1` = plateaux.
/// Pour une partition, `queue/` vit sur le disque parent, d'où le repli sur
/// `../queue/rotational`.
///
/// Rend `None` hors Linux, ou si quoi que ce soit dans la chaîne échoue : ce
/// n'est qu'une heuristique de performance, elle ne doit jamais empêcher un
/// scan. Lue une seule fois par processus — le pool n'est construit qu'une
/// fois, et un disque ne change pas de nature en cours de route.
pub(crate) fn disque_rotatif() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let racine = std::env::var("TUNE_MUSIC_DIRS")
            .ok()
            .and_then(|v| {
                serde_json::from_str::<Vec<String>>(&v)
                    .ok()
                    .and_then(|d| d.into_iter().next())
            })
            .unwrap_or_else(|| "/".to_string());
        let dev = std::fs::metadata(&racine).ok()?.dev();
        let (majeur, mineur) = (unsafe { libc::major(dev) }, unsafe { libc::minor(dev) });
        let base = format!("/sys/dev/block/{majeur}:{mineur}");
        for chemin in [
            format!("{base}/queue/rotational"),
            format!("{base}/../queue/rotational"),
        ] {
            if let Ok(v) = std::fs::read_to_string(&chemin) {
                return match v.trim() {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                };
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Lower CPU and I/O priority of the calling thread (Linux only, no-op
/// elsewhere). Applied to the dedicated scan pool threads only — never to
/// shared tokio pools — so a full scan stays in the background instead of
/// starving the UI and playback on small machines (USB-key appliance,
/// laptop CPUs: Stéphane, Tune OS). I/O class is best-effort level 7, not
/// IDLE: IDLE can starve the scan completely under continuous playback.
fn lower_scan_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        // Linux semantics: PRIO_PROCESS with a tid targets that thread only.
        // (`as _` : glibc type __priority_which_t vs c_int selon les libc.)
        libc::setpriority(libc::PRIO_PROCESS as _, tid as libc::id_t, 10);
        const IOPRIO_CLASS_BE: libc::c_long = 2;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        let ioprio = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio);
    }
}

/// The dedicated I/O thread pool for tag reads, built once for the whole
/// process. Returns `None` (→ caller falls back to the default rayon pool) if
/// the pool couldn't be built. Reusing it avoids spawning and tearing down
/// SCAN_IO_CONCURRENCY OS threads on every scan pass.
fn scan_io_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(scan_io_concurrency())
            .thread_name(|i| format!("scan-io-{i}"))
            .start_handler(|_| lower_scan_thread_priority())
            .build()
            .ok()
    })
    .as_ref()
}

/// Audio extensions recognised by the scanner. Shared with the decoder
/// contract and the file watcher (which excludes `iso`: ISO SACD needs the
/// extraction step that only the full directory walk performs).
pub use crate::audio::support::LIBRARY_AUDIO_EXTENSIONS as SUPPORTED_EXTENSIONS;

/// Audio (or audio-describing) extensions Tune does NOT read, listed so the scan
/// report can say WHY files were left out instead of letting them vanish.
///
/// Deliberately a fixed list rather than "anything not in SUPPORTED_EXTENSIONS":
/// a music library is full of `.jpg`, `.nfo`, `.m3u`, `.log` and `.accurip`, and
/// counting those would drown the one line the user can act on.
///
/// `cue` earns its place even though it is not audio: a `.cue` next to a single
/// large file is precisely how an album gets stored as one track, so its
/// presence explains a missing album better than anything else.
pub use crate::audio::support::KNOWN_UNREAD_AUDIO_EXTENSIONS as KNOWN_UNREAD_AUDIO;

const SKIP_DIRS: &[&str] = &[
    "duplicates",
    ".tune",
    ".Spotlight-V100",
    ".Trashes",
    "@eaDir",
    "#recycle",
    ".DS_Store",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Normalize a directory path for cross-platform compatibility.
///
/// On Windows, paths may use either `/` or `\` as separators. Users may also
/// add trailing slashes. This function:
/// - Converts forward slashes to the OS-native separator
/// - Strips trailing separators (except for root paths like `C:\` or `/`)
/// - Preserves UNC paths (`\\server\share`)
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // On Windows, normalize forward slashes to backslashes so that
    // std::path operations and WalkDir work with a consistent separator.
    #[cfg(target_os = "windows")]
    let normalized = trimmed.replace('/', "\\");
    #[cfg(not(target_os = "windows"))]
    let normalized = trimmed.to_string();

    // Strip trailing separator, but keep root paths intact (e.g. `C:\` or `/`)
    let result = normalized.trim_end_matches(['/', '\\']);
    if result.is_empty() {
        // Was just "/" or "\"
        return normalized.chars().next().unwrap().to_string();
    }

    // Keep the trailing separator for Windows drive roots like "C:"
    #[cfg(target_os = "windows")]
    if result.len() == 2 && result.as_bytes()[1] == b':' {
        return format!("{result}\\");
    }

    result.to_string()
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: String,
    pub metadata: Option<TrackMetadata>,
    /// Format audio reconnu mais non décodable par le binaire livré.
    /// Distinct d'un échec de métadonnées : le rapport doit expliquer le choix
    /// fail-closed sans présenter le fichier comme corrompu.
    pub unsupported: Option<UnsupportedLibraryAudio>,
    pub audio_hash: Option<String>,
    pub file_size: u64,
    pub mtime: u64,
}

/// Combien de chemins écartés une liste du rapport de scan retient au plus.
///
/// Le rapport doit répondre « lesquels ? », pas « tous » : une bibliothèque
/// dont 40 000 fichiers `.cue` sont écartés produirait sinon un rapport de
/// plusieurs mégaoctets, relu à chaque `GET /scan/report`. Les compteurs
/// (`skipped_unsupported`, `skipped_no_metadata`, `skipped_duplicate`,
/// `skipped_unsupported_by_ext`) restent, eux, exhaustifs : le plafond borne
/// l'échantillon nominatif, jamais le décompte.
pub const PLAFOND_CHEMINS_ECARTES: usize = 500;

/// Ajoute un chemin écarté tant que la liste n'a pas atteint son plafond.
///
/// Une seule définition pour les six endroits qui écartent un fichier — deux
/// dans ce parcours, deux dans la phase de métadonnées, deux dans les boucles
/// d'import. Le motif « un chemin corrigé, les autres nus » est la façon dont
/// ce rapport a déjà divergé trois fois (#2012) ; un plafond recopié à la main
/// six fois divergerait de la même manière.
pub fn pousser_chemin_ecarte(liste: &mut Vec<String>, chemin: impl Into<String>) {
    if liste.len() < PLAFOND_CHEMINS_ECARTES {
        liste.push(chemin.into());
    }
}

#[derive(Debug, Default)]
pub struct ScanStats {
    pub total_files: usize,
    pub metadata_ok: usize,
    pub metadata_failed: usize,
    pub metadata_timeout: usize,
    pub hash_ok: usize,
    pub failed_paths: Vec<String>,
    pub unsupported_by_ext: std::collections::HashMap<String, usize>,
    pub unsupported_reasons: std::collections::HashMap<String, String>,
    /// Les CHEMINS des fichiers dont le format a été reconnu à la lecture mais
    /// n'est pas décodable, plafonnés par [`PLAFOND_CHEMINS_ECARTES`].
    ///
    /// `unsupported_by_ext` dit « 280 fichiers `.mpc` » ; seul ceci dit
    /// LESQUELS. C'est la question posée par le testeur (#2050) et celle qui
    /// manque pour instruire « des fichiers présents dans l'explorateur sont
    /// absents de Tune » (#2365, #2802).
    pub unsupported_paths: Vec<String>,
    /// Les lignes que la BASE a refusées à l'insertion, sur l'ensemble du scan
    /// (#2939).
    ///
    /// Ce compteur-ci n'est pas de la même nature que les précédents et c'est
    /// tout son intérêt. `metadata_failed` compte des LECTURES de fichier :
    /// il vaut 0 dès que les balises se lisent, quoi qu'il advienne ensuite.
    /// Chez Alain Bonnel (fil 1313), les quatorze fichiers d'un album se sont
    /// lus sans une erreur — `metadata_ok=14 metadata_failed=0` — puis les
    /// quatorze insertions ont été refusées (`UNIQUE constraint failed:
    /// tracks.file_path`). Le résumé de fin de scan annonçait donc un scan
    /// sans le moindre défaut alors qu'un album entier venait d'être perdu.
    ///
    /// L'écriture en base n'est pas faite par ce module : elle est faite par
    /// la fermeture passée à [`scan_files_batched`]. C'est pourquoi cette
    /// fermeture REND désormais son verdict d'écriture ([`EcrituresDuLot`])
    /// au lieu de ne rien rendre — le compteur ne peut plus rester à zéro
    /// pendant que l'appelant, lui, sait très bien qu'il a perdu des pistes.
    pub db_insert_failed: usize,
    /// Les lignes que la base a refusées à la MISE À JOUR — même mécanique et
    /// même raison que [`Self::db_insert_failed`].
    pub db_update_failed: usize,
}

impl ScanStats {
    /// Le scan a-t-il perdu quelque chose en chemin ?
    ///
    /// Un seul endroit pour répondre, parce que la question s'est déjà posée
    /// à trois endroits recopiés à la main (#2012). « Sans erreur » ne veut
    /// pas dire « toutes les balises se sont lues » : un fichier lu puis
    /// refusé par la base est perdu tout autant qu'un fichier illisible.
    pub fn a_perdu_des_pistes(&self) -> bool {
        self.metadata_failed > 0 || self.db_insert_failed > 0 || self.db_update_failed > 0
    }
}

/// Ce qu'un lot a réellement réussi à ÉCRIRE, rendu par la fermeture
/// d'importation à [`scan_files_batched`] (#2939).
///
/// Le parcours lit des fichiers ; il n'écrit rien en base. Sans ce retour, le
/// résumé qu'il publie ne peut parler que de lecture — et c'est exactement ce
/// qui s'est produit : `batched_scan_complete total=14 metadata_ok=14
/// metadata_failed=0` pour un lot dont les quatorze insertions ont été
/// refusées. Un `()` en valeur de retour ne pose aucune question à
/// l'appelant ; cette structure-ci l'oblige à répondre.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EcrituresDuLot {
    /// Lignes présentées à l'insertion et refusées par la base.
    pub insert_failed: usize,
    /// Lignes présentées à la mise à jour et refusées par la base.
    pub update_failed: usize,
}

impl EcrituresDuLot {
    /// Le lot n'a rien perdu — la valeur d'un import qui s'est bien passé.
    pub const SANS_PERTE: Self = Self {
        insert_failed: 0,
        update_failed: 0,
    };

    /// Le manque à écrire d'un lot : ce qui a été présenté moins ce qui est
    /// entré. Écrit une fois ici plutôt que soustrait à la main chez chacun
    /// des deux appelants — c'est la soustraction qui diverge.
    pub fn manque(presentees_a_l_insertion: usize, insertions_reussies: usize) -> Self {
        Self {
            insert_failed: presentees_a_l_insertion.saturating_sub(insertions_reussies),
            update_failed: 0,
        }
    }

    /// Ajoute le manque à la mise à jour, sur le même modèle.
    pub fn avec_manque_a_la_mise_a_jour(
        mut self,
        presentees: usize,
        mises_a_jour_reussies: usize,
    ) -> Self {
        self.update_failed = presentees.saturating_sub(mises_a_jour_reussies);
        self
    }
}

#[derive(Debug)]
enum ReadFileError {
    Timeout,
    Unsupported(UnsupportedLibraryAudio),
    Other(String),
}

/// Read metadata (and optionally compute hash) for a single file, with a
/// [`FILE_TIMEOUT`] guard. If the underlying I/O does not complete in time,
/// [`ReadFileError::Timeout`] is returned.
///
/// We spawn a real OS thread because the metadata/hash reads are blocking I/O
/// that can hang on NAS mounts — `rayon` tasks must not block indefinitely.
/// Read tags (and optionally the audio hash), retrying once with a larger tag
/// budget on timeout. On slow network storage the first `FILE_TIMEOUT` read
/// often times out but a second, longer read succeeds — recovering the real
/// duration instead of leaving the track at duration 0. (Pierre M's NAS,
/// Philippe Landes' RS130 SSD)
fn read_file_with_retry(
    path: &PathBuf,
    with_hash: bool,
) -> Result<(Option<TrackMetadata>, Option<String>), ReadFileError> {
    let result = match read_file_with_timeout(path, with_hash, FILE_TIMEOUT) {
        Err(ReadFileError::Timeout) => read_file_with_timeout(path, with_hash, RETRY_FILE_TIMEOUT),
        other => other,
    };
    result.map(|(mut metadata, hash)| {
        if let Some(meta) = metadata.as_mut() {
            let corrections = meta.sanitize_text_fields();
            if !corrections.is_empty() {
                warn!(
                    path = %path.display(),
                    corrections = ?corrections,
                    "scan_metadata_unsafe_text_sanitized"
                );
            }
        }
        (metadata, hash)
    })
}

/// A path is an address on the real filesystem and cannot be rewritten in the
/// DB without making the track unopenable. Report the exact unsafe codepoints
/// and the safe spelling instead; ingest-generated destination components use
/// the same sanitizer and therefore never create new paths like this.
fn warn_unsafe_path_text(path: &str) {
    let (safe_path, corrections) =
        crate::metadata::sanitize_untrusted_single_line_text(path, "file_path");
    if !corrections.is_empty() {
        warn!(
            path,
            safe_path,
            corrections = ?corrections,
            "scan_path_contains_unsafe_text_preserved_for_io"
        );
    }
}

/// Best-effort DSD (DSF/DFF) duration from the file header, bounded so a stalled
/// mount can't re-hang the scan. Used only in the timeout fallback: even when
/// lofty's full tag read timed out (big embedded art over slow storage), the
/// ~92-byte DSD header usually still reads fine, so the track gets a real
/// duration in the library instead of 0 — a 0 disables gapless/advance/prefetch
/// downstream (the DSD testers' slow-storage libraries: Philippe Landes' 20k
/// DSD tracks). Play-time backfill (resolve_local_track) already repairs it on
/// first play; this fixes the library display up front. `None` for non-DSD, on
/// any parse error, or if even the header read times out.
fn probe_dsd_header_duration_bounded(path: &std::path::Path) -> Option<u64> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "dsf" && ext != "dff" {
        return None;
    }
    let p = path.to_string_lossy().to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let dur = if p.to_ascii_lowercase().ends_with(".dff") {
            crate::audio::dff::parse_dff(&p)
                .ok()
                .and_then(|i| i.duration_ms())
        } else {
            crate::audio::dsf::parse_dsf(&p)
                .ok()
                .and_then(|i| i.duration_ms())
        };
        let _ = tx.send(dur);
    });
    rx.recv_timeout(Duration::from_secs(10))
        .ok()
        .flatten()
        .filter(|&d| d > 0)
}

fn read_file_with_timeout(
    path: &PathBuf,
    with_hash: bool,
    tag_timeout: Duration,
) -> Result<(Option<TrackMetadata>, Option<String>), ReadFileError> {
    // Phase 1 — read the tags. This is fast even on a NAS (only the header /
    // tag blocks are read), so `tag_timeout` is plenty. A timeout here means the
    // tags are genuinely unreadable → caller falls back to filename metadata.
    let meta_path = path.clone();
    let (mtx, mrx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = match crate::audio::support::library_audio_support(&meta_path) {
            LibraryAudioSupport::Unsupported(unsupported) => {
                Err(ReadFileError::Unsupported(unsupported))
            }
            LibraryAudioSupport::Supported | LibraryAudioSupport::NotAudio => {
                try_read_metadata(&meta_path).map_err(ReadFileError::Other)
            }
        };
        let _ = mtx.send(result);
    });
    let metadata = match mrx.recv_timeout(tag_timeout) {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(ReadFileError::Timeout),
    };

    if !with_hash {
        return Ok((Some(metadata), None));
    }

    // Phase 2 — compute the audio hash (used only for duplicate detection). This
    // reads the WHOLE file, which on very large Hi-Res files over a NAS can far
    // exceed the tag-read budget (Progman: a 23-min FLAC 24/88.2 ≈ 1 GB). Make
    // it best-effort: if it doesn't finish in HASH_TIMEOUT, keep the real tags
    // and just skip the hash (audio_hash = None) instead of dropping the track
    // to filename-only metadata. Dedup is degraded for that one file, nothing
    // more.
    let hash_path = path.clone();
    let (htx, hrx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = htx.send(compute_audio_hash(&hash_path));
    });
    let hash = hrx.recv_timeout(HASH_TIMEOUT).unwrap_or(None);
    Ok((Some(metadata), hash))
}

pub struct ListAudioResult {
    pub files: Vec<PathBuf>,
    pub missing_dirs: Vec<String>,
    /// Paths where the walk itself errored MID-scan (a subfolder that became
    /// unreadable, a transient SMB stall, a nested mount that dropped, a
    /// permission wall). Files below these paths may still exist on disk even
    /// though they are absent from `files` — the post-scan prune must treat
    /// them like `missing_dirs`, otherwise their tracks get silently deleted.
    pub error_dirs: Vec<String>,
    /// One "path: kind — message" line per entry of `missing_dirs`, so the
    /// scan report can tell the user WHY a root was skipped (NotFound = bad
    /// UNC / NAS unmounted, PermissionDenied = no SMB credentials, mapped
    /// drive invisible to a service token, …) instead of burying the reason
    /// in the server log (Alain Bonnel, Windows NAS).
    pub missing_dir_reasons: Vec<String>,
    /// Fichiers audio rencontrés mais non lus, comptés par extension.
    ///
    /// Vide dans l'immense majorité des cas. Quand il ne l'est pas, c'est la
    /// seule chose qui explique à l'utilisateur pourquoi des albums manquent —
    /// sans quoi ils disparaissent en silence (cf `KNOWN_UNREAD_AUDIO`).
    pub skipped_by_ext: std::collections::HashMap<String, usize>,
    /// Motif stable associé à chaque clé de `skipped_by_ext`.
    pub skipped_reasons: std::collections::HashMap<String, String>,
    /// Les CHEMINS écartés par ce parcours, plafonnés par
    /// [`PLAFOND_CHEMINS_ECARTES`] et suffixés de leur motif.
    ///
    /// C'est ici que la liste demandée se perdait : le parcours classait le
    /// fichier, incrémentait `skipped_by_ext`, puis jetait le chemin (#2050).
    pub skipped_paths: Vec<String>,
    /// Les dossiers qui portent au moins une feuille CUE, triés et dédoublés.
    ///
    /// Le parcours voit passer chaque `.cue`, le compte comme non lu, et jette
    /// l'information (#1763). Or une feuille est la seule chose qui explique un
    /// album entier absent, et savoir CE QU'ELLE DÉCRIT demande de la lire —
    /// ce qui ne peut pas se faire ici : le parcours doit rester une opération
    /// de répertoire, sans lecture bloquante par fichier sur un NAS. On retient
    /// donc les dossiers, et [`super::cue_album::inventorier`] les relit après
    /// coup, une fois le parcours terminé.
    ///
    /// Non plafonné : c'est une liste de DOSSIERS, bornée par l'arborescence
    /// que le parcours vient de traverser, et bien plus courte que `files`.
    /// Le plafond est posé plus loin, sur la RELECTURE, qui elle coûte une
    /// ouverture de fichier par feuille — voir
    /// [`super::cue_album::PLAFOND_DOSSIERS_INVENTORIES`].
    pub dossiers_avec_feuille_cue: Vec<PathBuf>,
}

impl ListAudioResult {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.missing_dirs.is_empty()
    }
}

/// Ce que le parcours des dossiers sait de lui-même **pendant** qu'il tourne.
///
/// Le parcours est la phase la plus longue d'un scan complet sur un partage
/// réseau, et c'était la seule qui ne disait rien : `scan_dir_complete` ne
/// tombe qu'à la fin d'une racine, `audio_files_listed` qu'à la fin de toutes.
/// Sur une bibliothèque tenue par un NAS, cela fait plusieurs minutes pendant
/// lesquelles ni le journal ni l'écran ne distinguent « ça travaille » de
/// « c'est planté » (#2203, JP Borderies : 3 min 40 sans une ligne, scan
/// annulé, redémarrage, abandon).
///
/// Rien ici n'est calculé pour l'occasion : ces trois valeurs sont déjà tenues
/// par la boucle de parcours. Elles n'étaient simplement jamais rendues.
pub struct ProgressionParcours<'a> {
    /// Fichiers audio retenus depuis le début du parcours, **toutes racines
    /// confondues**. C'est un compte qui monte, jamais un pourcentage : à cet
    /// instant le total est inconnu — on ne peut pas savoir combien de
    /// fichiers restent avant de les avoir parcourus.
    pub fichiers_vus: usize,
    /// Racine configurée en cours de parcours.
    pub racine: &'a str,
    /// Dossier réellement visité à cet instant. C'est ce qui prouve à
    /// l'utilisateur que la machine avance, et c'est ce qui nous dit, sur un
    /// journal de testeur, OÙ un parcours s'est enlisé.
    pub dossier_courant: &'a str,
}

/// Cadence des annonces de progression du parcours.
///
/// À cadence fixe, jamais par fichier : une bibliothèque de 58 000 fichiers
/// produirait 58 000 lignes, ce qui noierait le journal et coûterait plus cher
/// que le parcours lui-même. Deux secondes, c'est la même cadence que
/// l'émission par lots de la phase d'import (`scan.rs`), pour que l'écran
/// reçoive un flux régulier d'un bout à l'autre du scan.
pub const CADENCE_PROGRESSION_PARCOURS: std::time::Duration = std::time::Duration::from_secs(2);

/// Pourquoi une racine configurée n'est pas parcourue pour elle-même.
///
/// Ce n'est PAS un rejet : ses fichiers sont bien parcourus, par la racine qui
/// la couvre. La distinction compte pour le rapport — une racine absorbée ne
/// doit jamais être confondue avec une racine illisible, qui déclenche, elle,
/// la protection de purge (#2356).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotifAbsorption {
    /// Deux écritures du MÊME dossier : chaîne différente, dossier identique.
    Identique,
    /// Sous-dossier d'une autre racine déjà parcourue.
    Imbriquee,
}

impl MotifAbsorption {
    /// L'étiquette stable posée au journal — lisible dans un export de
    /// testeur, et cherchable.
    pub fn etiquette(self) -> &'static str {
        match self {
            MotifAbsorption::Identique => "meme_dossier",
            MotifAbsorption::Imbriquee => "sous_dossier",
        }
    }
}

/// Une racine configurée que le parcours n'ouvrira pas, et celle qui la couvre.
#[derive(Debug, Clone)]
pub struct RacineAbsorbee {
    /// La racine telle que l'utilisateur l'a déclarée.
    pub racine: String,
    /// La racine qui la contient, telle qu'elle a été déclarée elle aussi.
    pub couverte_par: String,
    pub motif: MotifAbsorption,
}

/// Le verdict du dédoublonnage : ce qui sera parcouru, et ce qui a été absorbé.
///
/// Les deux listes sont rendues ensemble à dessein. Une déduplication ne se
/// juge pas aux doublons qu'elle évite : elle se juge aussi aux racines
/// légitimes qu'elle confond. Rendre `absorbees` permet de compter les deux.
#[derive(Debug, Default)]
pub struct RacinesDedoublonnees {
    /// Les racines à parcourir, dans l'ordre où l'utilisateur les a déclarées
    /// et sous leur écriture d'origine — le parcours les normalise lui-même,
    /// et le journal doit nommer ce que l'utilisateur a saisi.
    pub retenues: Vec<String>,
    pub absorbees: Vec<RacineAbsorbee>,
}

/// Ce que le dédoublonnage sait d'une racine avant de décider.
struct RacineSondee {
    /// Chemin canonique : liens symboliques résolus, `..` réduits, relatif
    /// rendu absolu. `None` si le chemin n'existe pas ou n'est pas résoluble.
    canonique: Option<PathBuf>,
    /// Identité du dossier au sens du système de fichiers. Elle attrape ce que
    /// la chaîne manque : deux montages du même partage, un `mount --bind`,
    /// une casse différente sur un volume insensible à la casse. `None` hors
    /// Unix, où `std` n'expose pas d'identifiant stable.
    identite: Option<(u64, u64)>,
    /// Le chemin désigne bien un dossier.
    est_dossier: bool,
    /// `read_dir` répond. Seule une racine lisible peut en absorber une autre.
    lisible: bool,
}

#[cfg(unix)]
fn identite_de_dossier(chemin: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(chemin).ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identite_de_dossier(_chemin: &std::path::Path) -> Option<(u64, u64)> {
    // Windows n'expose pas d'identifiant de fichier stable par `std`. La
    // comparaison retombe sur le chemin canonique, qui couvre déjà les
    // jonctions et les chemins UNC résolus par `canonicalize`.
    None
}

fn sonder_racine(brut: &str) -> RacineSondee {
    let normalisee = normalize_path(brut);
    let chemin = std::path::Path::new(&normalisee);
    RacineSondee {
        canonique: std::fs::canonicalize(chemin).ok(),
        identite: identite_de_dossier(chemin),
        est_dossier: chemin.is_dir(),
        // Le parcours refera cette sonde quelques lignes plus loin pour en
        // tirer le MOTIF de l'échec (`obstacle::obstacle_de_lecture`), qui
        // demande l'`io::Error` lui-même. On ne la partage pas : `read_dir`
        // n'est qu'un `openat` sur la racine — O(1), aucune entrée lue — et
        // le coût est sans commune mesure avec le parcours qui suit.
        lisible: std::fs::read_dir(chemin).is_ok(),
    }
}

/// Réduit les racines configurées à celles qu'il faut réellement parcourir.
///
/// # Le défaut corrigé (#2889)
///
/// La boucle de parcours itérait sur les racines telles quelles. Chez JeromeQ,
/// `/mnt/eversolo_nvme` et `/mnt/eversolo_nvme/77A6-799D` étaient tous deux
/// déclarés : le second sous-arbre était donc parcouru DEUX fois — une fois
/// pour lui-même, une fois à travers le premier. Chaque fichier entrait deux
/// fois dans `files`, donc deux fois dans la phase de lecture des métadonnées,
/// la plus longue du scan (48 minutes chez lui pour 30 000 fichiers).
///
/// # Ce que la clé couvre
///
/// - la même chaîne déclarée deux fois, aux espaces et à la barre finale près
///   ([`normalize_path`]) ;
/// - les allers-retours `.` et `..`, et un chemin relatif contre son absolu
///   (`canonicalize`) ;
/// - les liens symboliques, dans les deux sens — une racine lien vers une
///   autre racine, ou une racine sous un parent traversé par lien
///   (`canonicalize`) ;
/// - sur Unix : deux montages distincts du même partage, un `mount --bind`,
///   et une casse différente sur un volume insensible à la casse — ces
///   trois-là par l'identité `(device, inode)`, que la chaîne ne voit pas.
///
/// # Ce qu'elle ne couvre PAS
///
/// - **hors Unix**, la casse : `D:\Musique` et `d:\musique` restent deux
///   racines sous Windows, faute d'identifiant de fichier exposé par `std` ;
/// - l'**imbrication** à travers une identité plutôt qu'un chemin : si
///   `/mnt/a` et `/mnt/b` montent le même partage, `/mnt/a` et
///   `/mnt/b/Jazz` ne sont pas rapprochés — seules les racines *égales* le
///   sont. Le cas exact de #2889 (parent et enfant sous le même montage) est,
///   lui, couvert ;
/// - les **fichiers** atteignables par deux arbres (liens physiques). Le
///   dédoublonnage porte sur les racines, pas sur les feuilles ; c'est le
///   hachage audio qui répond de ce cas-là, plus loin dans le scan.
///
/// # Ce qui reste intact
///
/// Une racine **injoignable** — inexistante, NAS tombé, droits refusés — est
/// TOUJOURS retenue : elle doit atteindre la sonde du parcours pour être
/// rapportée dans `missing_dirs` avec son motif, ce qui déclenche
/// `VerdictPurge::ProtegeIllisible` et empêche la purge de supprimer ses
/// pistes (#2356). Elle n'absorbe personne non plus : un parent illisible ne
/// parcourra rien, et avaler son enfant lisible perdrait un sous-arbre sain.
pub fn dedoublonner_racines(dirs: &[String]) -> RacinesDedoublonnees {
    let sondees: Vec<RacineSondee> = dirs.iter().map(|d| sonder_racine(d)).collect();

    // Du plus court au plus long chemin canonique, pour qu'un parent soit
    // toujours examiné avant ses enfants : sans cet ordre, déclarer l'enfant
    // en premier ferait absorber le PARENT, et tout ce qu'il contient en
    // dehors de l'enfant disparaîtrait de la bibliothèque. L'indice sert de
    // départage pour que le résultat ne dépende pas de l'ordre de tri.
    let mut ordre: Vec<usize> = (0..sondees.len()).collect();
    ordre.sort_by_key(|&i| {
        let profondeur = sondees[i]
            .canonique
            .as_ref()
            .map(|c| c.components().count())
            .unwrap_or(usize::MAX);
        (profondeur, i)
    });

    let mut couverture: Vec<Option<(usize, MotifAbsorption)>> = vec![None; sondees.len()];
    let mut retenues_idx: Vec<usize> = Vec::new();

    for &i in &ordre {
        let candidate = &sondees[i];
        let Some(candidate_c) = candidate.canonique.as_ref() else {
            retenues_idx.push(i);
            continue;
        };
        if !candidate.est_dossier {
            // Un chemin qui n'est pas un dossier doit atteindre le parcours
            // pour y être nommé dans `missing_dirs` (#2356).
            retenues_idx.push(i);
            continue;
        }

        let mut couverte = None;
        for &j in &retenues_idx {
            let deja = &sondees[j];
            if !deja.lisible {
                continue;
            }
            let Some(deja_c) = deja.canonique.as_ref() else {
                continue;
            };
            if candidate_c == deja_c
                || (candidate.identite.is_some() && candidate.identite == deja.identite)
            {
                couverte = Some((j, MotifAbsorption::Identique));
                break;
            }
            // `Path::starts_with` compare COMPOSANT par composant, jamais
            // caractère par caractère : `/nas/Musique-2` ne commence pas par
            // `/nas/Musique`. C'est ce qui distingue ce test d'un
            // `str::starts_with`, qui fusionnerait deux bibliothèques
            // voisines aux noms proches.
            if candidate_c.starts_with(deja_c) {
                couverte = Some((j, MotifAbsorption::Imbriquee));
                break;
            }
        }
        match couverte {
            Some(c) => couverture[i] = Some(c),
            None => retenues_idx.push(i),
        }
    }

    // Rendues dans l'ordre de DÉCLARATION, pas dans l'ordre de tri : le
    // journal, le rapport et l'écran doivent refléter ce que l'utilisateur a
    // saisi.
    let mut verdict = RacinesDedoublonnees::default();
    for (i, dir) in dirs.iter().enumerate() {
        match couverture[i] {
            None => verdict.retenues.push(dir.clone()),
            Some((j, motif)) => verdict.absorbees.push(RacineAbsorbee {
                racine: dir.clone(),
                couverte_par: dirs[j].clone(),
                motif,
            }),
        }
    }
    verdict
}

pub fn list_audio_files(dirs: &[String]) -> ListAudioResult {
    list_audio_files_with_excludes(dirs, &[])
}

/// Like [`list_audio_files`], but skips any entry (file or directory subtree)
/// whose full path contains one of `exclude_patterns` (case-insensitive
/// substring — deliberately simple, no glob engine). Patterns come from the
/// `scan_exclude_paths` setting: staging/incoming folders, backup trees, a
/// sibling's library on a shared NAS…
///
/// Ne rend pas compte de son avancement. Les appelants qui ont un écran à
/// nourrir passent par [`list_audio_files_avec_progression`].
pub fn list_audio_files_with_excludes(
    dirs: &[String],
    exclude_patterns: &[String],
) -> ListAudioResult {
    list_audio_files_avec_progression(
        dirs,
        exclude_patterns,
        CADENCE_PROGRESSION_PARCOURS,
        &mut |_| {},
    )
}

/// Comme [`list_audio_files_with_excludes`], mais rend compte de son
/// avancement au fil du parcours, au plus une fois par `cadence`.
///
/// `on_progress` ne peut RIEN changer au parcours : il ne rend rien, ne reçoit
/// que des emprunts, et n'est appelé qu'entre deux entrées. L'ordre des
/// fichiers, les exclusions, les lots et la liste rendue sont identiques à
/// ceux de `list_audio_files_with_excludes` — cette fonction ajoute de la
/// visibilité, pas de la logique.
pub fn list_audio_files_avec_progression(
    dirs: &[String],
    exclude_patterns: &[String],
    cadence: std::time::Duration,
    on_progress: &mut dyn FnMut(ProgressionParcours<'_>),
) -> ListAudioResult {
    let excludes: Vec<String> = exclude_patterns
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let skip_set: HashSet<&str> = SKIP_DIRS.iter().copied().collect();

    let mut files = Vec::new();
    let mut skipped_by_ext: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut skipped_reasons: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut skipped_paths: Vec<String> = Vec::new();
    // `BTreeSet` et non `HashSet` : le rapport de scan doit être reproductible
    // d'un scan à l'autre, et l'ordre d'un `HashSet` ne l'est pas.
    let mut dossiers_cue: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut missing_dirs = Vec::new();
    let mut missing_dir_reasons: Vec<String> = Vec::new();
    let mut error_dirs: Vec<String> = Vec::new();
    // Above this many distinct error scopes the whole root is clearly in
    // trouble (NAS died mid-walk) — protect the entire root instead of
    // accumulating an unbounded list.
    const MAX_ERROR_SCOPES: usize = 50;
    // Lignes nominatives émises pour les ISO SACD non extraits avant de passer
    // au récapitulatif. Même valeur que le plafond des erreurs de parcours :
    // assez pour montrer des exemples, pas assez pour noyer le journal.
    const MAX_ISO_WARN: usize = 5;
    // Horloge des annonces de progression, partagée par TOUTES les racines :
    // la cadence doit valoir pour le parcours entier, pas se réarmer à chaque
    // racine. Initialisée dans le passé pour qu'une première annonce parte
    // aussitôt — c'est celle qui dit à l'écran que le parcours a commencé,
    // et sans elle une racine lente resterait muette pendant sa première
    // tranche de cadence.
    let mut derniere_annonce = std::time::Instant::now()
        .checked_sub(cadence)
        .unwrap_or_else(std::time::Instant::now);

    // Une racine imbriquée dans une autre était parcourue DEUX fois — une fois
    // pour elle-même, une fois à travers sa parente (#2889). Le dédoublonnage
    // se fait ici, au seul goulot par lequel passent les quatre appelants du
    // parcours : `auto_scan`, `export`, `ingest` et le scan principal.
    let racines = dedoublonner_racines(dirs);
    for absorbee in &racines.absorbees {
        info!(
            racine = %absorbee.racine,
            couverte_par = %absorbee.couverte_par,
            motif = %absorbee.motif.etiquette(),
            "scan_root_absorbed — racine déjà couverte par une autre, parcourue une seule fois"
        );
    }

    for dir in &racines.retenues {
        let normalized = normalize_path(dir);
        let dir_path = std::path::Path::new(&normalized);

        // Probe with read_dir instead of a bare exists(): on Windows a NAS path
        // fails for several distinct reasons that exists() collapses to `false`
        // (silent skip → "scan finds nothing", Alain Bonnel). read_dir surfaces
        // the actual error so the user learns WHY: bad UNC / NAS unmounted, no
        // SMB credentials for this session, and — the common Windows case — a
        // mapped drive (Z:\) invisible to an elevated / service token even
        // though it works in Explorer. La traduction de cette erreur en cause
        // lisible est le seul travail de `scanner::obstacle`.
        if let Err(e) = std::fs::read_dir(dir_path) {
            // La raison est NOMMÉE ici et pas ailleurs : `missing_dir_reasons`
            // est rendu VERBATIM par le client web (SettingsView.svelte). Le
            // `format!("{}: {:?} — {}", …, e.kind(), e)` d'avant y déposait le
            // `Debug` d'un `ErrorKind` — donc `Uncategorized` pour tout errno
            // que Rust ne mappe pas, dont ENODEV. C'est le message que JeromeQ
            // a recopié du fil 1539 sans pouvoir en rien faire (#2357).
            let (motif, message) = crate::scanner::obstacle::obstacle_de_lecture(&normalized, &e);
            warn!(
                dir = %normalized,
                original = %dir,
                error = %e,
                kind = ?e.kind(),
                // L'errno numérique n'était journalisé nulle part : sans lui,
                // un `Uncategorized` dans les journaux reste indéchiffrable
                // même pour nous.
                errno = ?e.raw_os_error(),
                motif = %motif,
                "scan_dir_unreadable — cannot open directory (unreachable NAS, mapped drive not visible to this session, or permission denied), skipping"
            );
            missing_dir_reasons.push(message);
            missing_dirs.push(normalized);
            continue;
        }
        if !dir_path.is_dir() {
            // Ce chemin n'est atteignable qu'en course (la racine a été
            // remplacée entre la sonde `read_dir` et ce test) : `read_dir`
            // rend déjà ENOTDIR sur un fichier. Il n'en poussait pas moins la
            // racine dans le néant — un `warn!` et un `continue`, rien dans
            // `missing_dirs`. Or `missing_dirs` n'est pas qu'un rapport :
            // c'est ce qui déclenche `VerdictPurge::ProtegeIllisible`. Une
            // racine écartée hors de cette liste voyait donc ses pistes
            // supprimées comme si les fichiers avaient disparu (#2356).
            let (motif, message) = crate::scanner::obstacle::pas_un_dossier(&normalized);
            warn!(
                dir = %normalized,
                motif = %motif,
                "scan_dir_not_a_directory — path is not a directory, skipping"
            );
            missing_dir_reasons.push(message);
            missing_dirs.push(normalized);
            continue;
        }

        let mut dir_file_count = 0usize;
        let mut dir_error_count = 0usize;
        let mut dir_iso_error_count = 0usize;

        let walker = WalkDir::new(&normalized)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                if !excludes.is_empty() {
                    let path_l = e.path().to_string_lossy().to_lowercase();
                    if excludes.iter().any(|x| path_l.contains(x.as_str())) {
                        debug!(path = %e.path().display(), "scan_excluded_by_pattern");
                        return false;
                    }
                }
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    !skip_set.contains(name.as_ref())
                } else {
                    true
                }
            });

        for entry in walker {
            match entry {
                Ok(entry) => {
                    // Signe de vie du parcours (#2203).
                    //
                    // Placé AVANT le filtre `is_file` à dessein : sur un
                    // partage réseau, le temps ne se passe pas sur les
                    // fichiers retenus, il se passe à traverser des dossiers
                    // qui n'en contiennent aucun. Un battement qui ne tomberait
                    // que sur les fichiers audio se tairait précisément là où
                    // le parcours s'enlise.
                    //
                    // Le test de cadence est un unique `Instant::now()` par
                    // entrée — quelques dizaines de nanosecondes, à comparer
                    // au `stat` réseau que le parcours vient de payer.
                    if derniere_annonce.elapsed() >= cadence {
                        derniere_annonce = std::time::Instant::now();
                        let dossier = if entry.file_type().is_dir() {
                            entry.path()
                        } else {
                            entry.path().parent().unwrap_or_else(|| entry.path())
                        };
                        let dossier = dossier.to_string_lossy();
                        info!(
                            racine = %normalized,
                            dossier = %dossier,
                            fichiers = files.len(),
                            "scan_indexing_progress — parcours en cours"
                        );
                        on_progress(ProgressionParcours {
                            fichiers_vus: files.len(),
                            racine: &normalized,
                            dossier_courant: &dossier,
                        });
                    }
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    // Skip macOS AppleDouble sidecar files (._foo.flac): they carry
                    // the audio extension but are tiny resource-fork metadata, not
                    // real tracks, and were being indexed as bogus duplicates (Elie).
                    if entry.file_name().to_string_lossy().starts_with("._") {
                        continue;
                    }
                    // Skip Tune's own streaming/transcode temp files
                    // (tune-stream-*, tune-prefetch-*, tune-tcache-* in %TEMP%):
                    // a library rooted above the temp dir (Frédéric: whole user
                    // profile) otherwise indexes every transcode as a ghost track.
                    if crate::scanner::is_tune_temp_file(entry.path()) {
                        continue;
                    }
                    let path = entry.path();
                    // Classer AVANT d'ajouter le chemin : WMA, DST autonome et
                    // les autres formats audio connus mais non lus restent
                    // visibles dans le rapport, sans devenir des pistes
                    // cliquables impossibles à décoder. DFF/DST sera distingué
                    // plus tard, dans la phase de métadonnées bornée. Les
                    // fichiers non audio restent silencieux.
                    match crate::audio::support::library_audio_support_by_extension(path) {
                        crate::audio::support::LibraryAudioSupport::Unsupported(unsupported) => {
                            *skipped_by_ext
                                .entry(unsupported.report_key.clone())
                                .or_insert(0) += 1;
                            // Le chemin, pas seulement le décompte : « 280
                            // fichiers .mpc » ne dit pas lesquels, et c'est
                            // lesquels que le testeur demande (#2050).
                            pousser_chemin_ecarte(
                                &mut skipped_paths,
                                format!("{} ({})", path.display(), unsupported.reason),
                            );
                            // Une feuille CUE n'est pas un fichier ignoré comme
                            // un autre : c'est la description d'un album. Le
                            // dossier est retenu ici et relu après le parcours
                            // (#1763) — voir `dossiers_avec_feuille_cue`.
                            if unsupported.report_key == "cue" {
                                if let Some(parent) = path.parent() {
                                    dossiers_cue.insert(parent.to_path_buf());
                                }
                            }
                            skipped_reasons
                                .entry(unsupported.report_key)
                                .or_insert_with(|| unsupported.reason.to_string());
                            continue;
                        }
                        crate::audio::support::LibraryAudioSupport::NotAudio => continue,
                        crate::audio::support::LibraryAudioSupport::Supported => {}
                    }

                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        // ISO SACD: extract DSF tracks instead of adding the ISO directly
                        if ext.eq_ignore_ascii_case("iso") {
                            if crate::audio::iso_sacd::is_sacd_iso(path) {
                                match crate::audio::iso_sacd::extract_iso_to_dsf(path) {
                                    Ok(dsf_files) => {
                                        dir_file_count += dsf_files.len();
                                        files.extend(dsf_files);
                                    }
                                    Err(e) => {
                                        // Le fichier n'entre en base par AUCUN
                                        // chemin : ni comme ISO, ni comme
                                        // pistes extraites. Il doit donc être
                                        // COMPTÉ et NOMMÉ dans le rapport de
                                        // scan, faute de quoi l'album
                                        // disparaît sans qu'aucun écran ne le
                                        // dise — 22 albums SACD chez JeromeQ,
                                        // pour la seule trace d'un `warn!`
                                        // dans un fichier de journal (#2992).
                                        dir_iso_error_count += 1;
                                        // Quelques lignes détaillées, puis un
                                        // récapitulatif : un `warn!` par
                                        // fichier noierait le journal.
                                        if dir_iso_error_count <= MAX_ISO_WARN {
                                            warn!(path = %path.display(), error = %e, "sacd_iso_extract_failed");
                                        }
                                        *skipped_by_ext
                                            .entry(
                                                crate::audio::iso_sacd::CLE_RAPPORT_ISO_SACD
                                                    .to_string(),
                                            )
                                            .or_insert(0) += 1;
                                        // Le motif technique exact accompagne
                                        // le CHEMIN — « sacd_extract not
                                        // found » et « sacd_extract failed »
                                        // ne demandent pas le même geste.
                                        pousser_chemin_ecarte(
                                            &mut skipped_paths,
                                            format!("{} ({e})", path.display()),
                                        );
                                        skipped_reasons
                                            .entry(
                                                crate::audio::iso_sacd::CLE_RAPPORT_ISO_SACD
                                                    .to_string(),
                                            )
                                            .or_insert_with(|| {
                                                crate::audio::iso_sacd::MOTIF_ISO_SACD_NON_EXTRAIT
                                                    .to_string()
                                            });
                                        dir_error_count += 1;
                                    }
                                }
                            } else {
                                // `.iso` sans zone SACD : une image de données.
                                // Elle ne devient pas une piste — la pousser
                                // dans `files` enverrait une image
                                // d'installation de 5 Go traverser la phase de
                                // métadonnées pour finir en piste fantôme.
                                // Elle est écartée, mais NOMMÉE (#2992).
                                *skipped_by_ext
                                    .entry(
                                        crate::audio::iso_sacd::CLE_RAPPORT_ISO_DONNEES.to_string(),
                                    )
                                    .or_insert(0) += 1;
                                pousser_chemin_ecarte(
                                    &mut skipped_paths,
                                    format!(
                                        "{} ({})",
                                        path.display(),
                                        crate::audio::iso_sacd::MOTIF_ISO_SANS_ZONE_SACD
                                    ),
                                );
                                skipped_reasons
                                    .entry(
                                        crate::audio::iso_sacd::CLE_RAPPORT_ISO_DONNEES.to_string(),
                                    )
                                    .or_insert_with(|| {
                                        crate::audio::iso_sacd::MOTIF_ISO_SANS_ZONE_SACD.to_string()
                                    });
                            }
                        } else {
                            files.push(path.to_path_buf());
                            dir_file_count += 1;
                        }
                    }
                }
                Err(err) => {
                    dir_error_count += 1;
                    // Record WHERE the walk failed so the prune can protect the
                    // subtree: without this, files under an unreadable subfolder
                    // of a perfectly reachable root drop out of the discovered
                    // set and their tracks get deleted from the library. No
                    // path on the error (rare) → protect the whole root.
                    let err_scope = err
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| normalized.clone());
                    if error_dirs.len() >= MAX_ERROR_SCOPES {
                        if !error_dirs.contains(&normalized) {
                            error_dirs.push(normalized.clone());
                        }
                    } else if !error_dirs.contains(&err_scope) {
                        error_dirs.push(err_scope);
                    }
                    if dir_error_count <= 5 {
                        warn!(
                            dir = %normalized,
                            error = %err,
                            "scan_walk_error — error while walking directory"
                        );
                    }
                }
            }
        }

        if dir_error_count > 5 {
            warn!(
                dir = %normalized,
                total_errors = dir_error_count,
                "scan_walk_errors_truncated — additional walk errors suppressed"
            );
        }

        // Même patron que ci-dessus : quelques lignes nominatives plafonnées,
        // puis une ligne qui porte le TOTAL. Sans elle, le journal de JeromeQ
        // aurait montré cinq ISO là où vingt-deux albums manquaient (#2992).
        if dir_iso_error_count > MAX_ISO_WARN {
            warn!(
                dir = %normalized,
                total_errors = dir_iso_error_count,
                "sacd_iso_extract_errors_truncated — additional SACD ISO failures suppressed"
            );
        }

        info!(
            dir = %normalized,
            files = dir_file_count,
            errors = dir_error_count,
            "scan_dir_complete"
        );
    }

    info!(
        count = files.len(),
        dirs = dirs.len(),
        // Ce que le parcours a réellement ouvert, et ce qu'il a économisé.
        // Sans ces deux champs, un `count` divisé par deux d'un scan à l'autre
        // resterait inexplicable dans un journal de testeur (#2889).
        parcourues = racines.retenues.len(),
        absorbees = racines.absorbees.len(),
        missing = missing_dirs.len(),
        walk_errors = error_dirs.len(),
        "audio_files_listed"
    );
    // Journalisé ici, une seule ligne par scan, et seulement quand il y a
    // quelque chose à dire : c'est la trace qui manquait pour répondre « vos
    // fichiers .mpc ne sont pas lus » au lieu de chercher un bug de scanner.
    if !skipped_by_ext.is_empty() {
        let mut par_ext: Vec<(String, usize)> = skipped_by_ext
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        par_ext.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total: usize = par_ext.iter().map(|(_, n)| n).sum();
        let detail = par_ext
            .iter()
            .map(|(e, n)| {
                let reason = skipped_reasons.get(e).map(String::as_str).unwrap_or("");
                format!(".{e}={n} ({reason})")
            })
            .collect::<Vec<_>>()
            .join(" ");
        warn!(
            total,
            detail = %detail,
            "scan_unsupported_audio_skipped — fichiers audio rencontrés mais non lus par Tune"
        );
    }

    ListAudioResult {
        files,
        missing_dirs,
        error_dirs,
        missing_dir_reasons,
        skipped_by_ext,
        skipped_reasons,
        skipped_paths,
        dossiers_avec_feuille_cue: dossiers_cue.into_iter().collect(),
    }
}

pub fn scan_files_parallel(
    files: &[PathBuf],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let counter = AtomicUsize::new(0);
    let timeout_counter = AtomicUsize::new(0);
    let total = files.len();
    let failed_files: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let results: Vec<ScannedFile> = files
        .par_iter()
        .map(|path| {
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            if let Some(ref cb) = progress_callback
                && idx.is_multiple_of(100)
            {
                cb(idx, total);
            }

            // NFC-normalize the path string: macOS HFS+/APFS stores filenames
            // in NFD (decomposed Unicode, e.g. "è" = "e" + combining accent).
            // Without NFC normalization, metadata readers and DB lookups can
            // fail on paths containing accented characters.
            let path_str: String = path.to_string_lossy().nfc().collect();
            warn_unsafe_path_text(&path_str);

            let file_meta = path.metadata().ok();
            let stat_ok = file_meta.is_some();
            let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = file_meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            // Zero-byte "audio" files are aborted copies/downloads, not
            // tracks: don't index a tagless duration-0 ghost, surface them in
            // failed_paths so the report shows what to clean.
            if stat_ok && file_size == 0 {
                warn!(path = %path_str, "scan_file_empty_skipped — zero-byte file (aborted copy?)");
                failed_files
                    .lock()
                    .unwrap()
                    .push((path_str.clone(), "empty file (0 bytes)".into()));
                return ScannedFile {
                    path: path_str,
                    metadata: None,
                    unsupported: None,
                    audio_hash: None,
                    file_size,
                    mtime,
                };
            }

            let (metadata, audio_hash, unsupported) = match read_file_with_retry(path, with_hash) {
                Ok((meta, hash)) => {
                    if meta.is_none() {
                        warn!(
                            path = %path_str,
                            "scan_file_no_metadata — metadata reader returned None"
                        );
                    }
                    (meta, hash, None)
                }
                Err(ReadFileError::Timeout) => {
                    // Don't drop the file — index it with filename-based metadata
                    // so it still appears in the library. audio_hash stays None so
                    // the next scan re-reads full tags once storage is responsive.
                    warn!(
                        path = %path_str,
                        timeout_secs = FILE_TIMEOUT.as_secs(),
                        "scan_file_timeout — tag read timed out, indexing with filename metadata"
                    );
                    timeout_counter.fetch_add(1, Ordering::Relaxed);
                    let mut meta = tagless_fallback_no_props(path);
                    // The full tag read timed out, but the tiny DSD header
                    // usually still reads — recover the duration so a slow-storage
                    // DSD track isn't left at 0 in the library (a 0 breaks
                    // gapless/advance/prefetch). Bounded; non-DSD relies on the
                    // play-time backfill.
                    if meta.duration_ms.is_none_or(|d| d == 0) {
                        if let Some(d) = probe_dsd_header_duration_bounded(path) {
                            meta.duration_ms = Some(d);
                        }
                    }
                    (Some(meta), None, None)
                }
                Err(ReadFileError::Unsupported(unsupported)) => {
                    info!(
                        path = %path_str,
                        format = %unsupported.report_key,
                        reason = unsupported.reason,
                        "scan_file_unsupported — format reconnu mais non décodable"
                    );
                    (None, None, Some(unsupported))
                }
                Err(ReadFileError::Other(err)) => {
                    warn!(
                        path = %path_str,
                        error = %err,
                        "scan_file_metadata_failed — could not read metadata"
                    );
                    failed_files
                        .lock()
                        .unwrap()
                        .push((path_str.clone(), err.clone()));
                    (None, None, None)
                }
            };

            ScannedFile {
                path: path_str,
                metadata,
                unsupported,
                audio_hash,
                file_size,
                mtime,
            }
        })
        .collect();

    let timed_out = timeout_counter.load(Ordering::Relaxed);
    let failed = failed_files.lock().unwrap();
    let failed_paths: Vec<String> = failed
        .iter()
        .map(|(p, e)| format!("{} ({})", p, e))
        .collect();
    let mut unsupported_by_ext = std::collections::HashMap::new();
    let mut unsupported_reasons = std::collections::HashMap::new();
    let mut unsupported_paths: Vec<String> = Vec::new();
    for file in results.iter() {
        let Some(unsupported) = file.unsupported.as_ref() else {
            continue;
        };
        *unsupported_by_ext
            .entry(unsupported.report_key.clone())
            .or_insert(0) += 1;
        // Le chemin était disponible ici depuis toujours, et jeté (#2050).
        pousser_chemin_ecarte(
            &mut unsupported_paths,
            format!("{} ({})", file.path, unsupported.reason),
        );
        unsupported_reasons
            .entry(unsupported.report_key.clone())
            .or_insert_with(|| unsupported.reason.to_string());
    }
    let stats = ScanStats {
        total_files: results.len(),
        metadata_ok: results.iter().filter(|f| f.metadata.is_some()).count(),
        metadata_failed: results
            .iter()
            .filter(|f| f.metadata.is_none() && f.unsupported.is_none())
            .count(),
        metadata_timeout: timed_out,
        hash_ok: results.iter().filter(|f| f.audio_hash.is_some()).count(),
        failed_paths,
        unsupported_by_ext,
        unsupported_reasons,
        unsupported_paths,
        // Ce parcours-ci n'écrit rien en base : il rend les fichiers lus à son
        // appelant, qui importe ensuite. Il ne peut donc rien constater sur
        // les écritures — seul le chemin PAR LOTS reçoit ce verdict de sa
        // fermeture d'import (#2939).
        db_insert_failed: 0,
        db_update_failed: 0,
    };
    if !failed.is_empty() {
        let listing: Vec<String> = failed
            .iter()
            .map(|(p, e)| format!("  {} ({})", p, e))
            .collect();
        warn!(
            count = failed.len(),
            "scan_metadata_failed_summary\n{}",
            listing.join("\n")
        );
    }
    drop(failed);

    if timed_out > 0 {
        warn!(
            count = timed_out,
            timeout_secs = FILE_TIMEOUT.as_secs(),
            "scan_timeout_summary — files skipped due to timeout"
        );
    }

    info!(
        total = stats.total_files,
        metadata_ok = stats.metadata_ok,
        metadata_failed = stats.metadata_failed,
        metadata_timeout = stats.metadata_timeout,
        "parallel_scan_complete"
    );

    (results, stats)
}

/// Default batch size for chunked scanning.
/// Balances memory usage vs. rayon thread-pool efficiency.
pub const SCAN_BATCH_SIZE: usize = 500;

/// Scan files in batches, calling `on_batch` after each chunk is parsed.
///
/// This enables **progressive availability**: each batch can be committed to
/// the database independently, so tracks are queryable as soon as each batch
/// finishes — not only after the entire scan completes.
///
/// The callback receives `(batch: Vec<ScannedFile>, batch_index: usize, total_files: usize)`.
/// It runs on a rayon worker thread, so the caller must ensure any shared
/// state (DB handle, caches) is `Send + Sync`.
///
/// Elle REND ce qu'elle a réellement écrit ([`EcrituresDuLot`]) : le parcours
/// ne touche pas à la base et ne peut donc pas mesurer lui-même ce qu'elle a
/// refusé. Sans ce retour, le résumé publié plus bas ne parle que de lecture
/// de balises et annonce un scan sans erreur alors qu'un album entier vient
/// d'être refusé à l'insertion (#2939, Alain Bonnel, fil 1313). Un import qui
/// n'a rien perdu rend [`EcrituresDuLot::SANS_PERTE`].
///
/// Returns aggregate `ScanStats` over all batches.
pub fn scan_files_batched(
    files: &[PathBuf],
    with_hash: bool,
    batch_size: usize,
    mut on_batch: impl FnMut(Vec<ScannedFile>, usize, usize) -> EcrituresDuLot,
) -> ScanStats {
    let total = files.len();
    let batch_sz = if batch_size == 0 {
        SCAN_BATCH_SIZE
    } else {
        batch_size
    };
    let mut aggregate = ScanStats::default();
    aggregate.total_files = total;

    // Dedicated high-concurrency pool for the I/O-bound tag reads (see
    // SCAN_IO_CONCURRENCY). Built once per process and reused by every scan
    // instead of spawning/tearing down 32 OS threads on each scan pass. Falls
    // back to the default rayon pool if it couldn't be built.
    let io_pool = scan_io_pool();

    for (batch_idx, chunk) in files.chunks(batch_sz).enumerate() {
        // Parse metadata in parallel within this chunk
        let failed_files: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let batch_timeout_counter = AtomicUsize::new(0);

        let read_batch = || {
            chunk
                .par_iter()
                .map(|path| {
                    // NFC-normalize: see comment in scan_files_parallel
                    let path_str: String = path.to_string_lossy().nfc().collect();
                    warn_unsafe_path_text(&path_str);

                    let file_meta = path.metadata().ok();
                    let stat_ok = file_meta.is_some();
                    let file_size = file_meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let mtime = file_meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    // Zero-byte "audio" files are aborted copies/downloads, not
                    // tracks: don't index a tagless duration-0 ghost, surface
                    // them in failed_paths so the report shows what to clean.
                    if stat_ok && file_size == 0 {
                        warn!(path = %path_str, "scan_file_empty_skipped — zero-byte file (aborted copy?)");
                        failed_files
                            .lock()
                            .unwrap()
                            .push((path_str.clone(), "empty file (0 bytes)".into()));
                        return ScannedFile {
                            path: path_str,
                            metadata: None,
                            unsupported: None,
                            audio_hash: None,
                            file_size,
                            mtime,
                        };
                    }

                    let (metadata, audio_hash, unsupported) =
                        match read_file_with_retry(path, with_hash) {
                        Ok((meta, hash)) => (meta, hash, None),
                        Err(ReadFileError::Timeout) => {
                            // Don't drop the file — same fallback as
                            // scan_files_parallel: index it with filename-based
                            // metadata so it still appears in the library.
                            // audio_hash stays None so the next scan re-reads
                            // full tags once storage is responsive.
                            warn!(
                                path = %path_str,
                                timeout_secs = FILE_TIMEOUT.as_secs(),
                                "scan_file_timeout — tag read timed out, indexing with filename metadata"
                            );
                            batch_timeout_counter.fetch_add(1, Ordering::Relaxed);
                            (Some(tagless_fallback_no_props(path)), None, None)
                        }
                        Err(ReadFileError::Unsupported(unsupported)) => {
                            info!(
                                path = %path_str,
                                format = %unsupported.report_key,
                                reason = unsupported.reason,
                                "scan_file_unsupported — format reconnu mais non décodable"
                            );
                            (None, None, Some(unsupported))
                        }
                        Err(ReadFileError::Other(err)) => {
                            warn!(
                                path = %path_str,
                                error = %err,
                                "scan_file_failed"
                            );
                            failed_files.lock().unwrap().push((path_str.clone(), err));
                            (None, None, None)
                        }
                    };

                    ScannedFile {
                        path: path_str,
                        metadata,
                        unsupported,
                        audio_hash,
                        file_size,
                        mtime,
                    }
                })
                .collect()
        };
        // Run the I/O-bound reads on the dedicated high-concurrency pool so many
        // per-file latencies overlap; fall back to the default pool if the
        // dedicated one couldn't be built.
        let batch: Vec<ScannedFile> = match io_pool {
            Some(pool) => pool.install(read_batch),
            None => read_batch(),
        };

        let batch_timeouts = batch_timeout_counter.load(Ordering::Relaxed);

        // Update aggregate stats
        aggregate.metadata_ok += batch.iter().filter(|f| f.metadata.is_some()).count();
        aggregate.metadata_failed += batch
            .iter()
            .filter(|f| f.metadata.is_none() && f.unsupported.is_none())
            .count();
        aggregate.metadata_timeout += batch_timeouts;
        aggregate.hash_ok += batch.iter().filter(|f| f.audio_hash.is_some()).count();
        for file in batch.iter() {
            let Some(unsupported) = file.unsupported.as_ref() else {
                continue;
            };
            *aggregate
                .unsupported_by_ext
                .entry(unsupported.report_key.clone())
                .or_insert(0) += 1;
            // Sœur de la variante par lot ci-dessus : c'est ce chemin-là que
            // l'un des deux aurait oublié (#2050).
            pousser_chemin_ecarte(
                &mut aggregate.unsupported_paths,
                format!("{} ({})", file.path, unsupported.reason),
            );
            aggregate
                .unsupported_reasons
                .entry(unsupported.report_key.clone())
                .or_insert_with(|| unsupported.reason.to_string());
        }

        let failed = failed_files.lock().unwrap();
        if !failed.is_empty() {
            for (p, e) in failed.iter() {
                aggregate.failed_paths.push(format!("{} ({})", p, e));
            }
            let listing: Vec<String> = failed
                .iter()
                .take(10)
                .map(|(p, e)| format!("  {} ({})", p, e))
                .collect();
            warn!(
                count = failed.len(),
                batch = batch_idx,
                "scan_batch_failures\n{}",
                listing.join("\n")
            );
        }
        drop(failed);

        if batch_timeouts > 0 {
            warn!(
                count = batch_timeouts,
                batch = batch_idx,
                timeout_secs = FILE_TIMEOUT.as_secs(),
                "scan_batch_timeouts — files skipped due to timeout"
            );
        }

        info!(
            batch = batch_idx,
            batch_size = batch.len(),
            scanned = (batch_idx + 1) * batch_sz,
            total,
            "scan_batch_complete"
        );

        // Ce que l'import a REFUSÉ d'écrire entre ici et le résumé de fin
        // (#2939). Le parcours ne le sait que parce que la fermeture le lui
        // dit ; il ne peut pas le déduire.
        let ecritures = on_batch(batch, batch_idx, total);
        aggregate.db_insert_failed += ecritures.insert_failed;
        aggregate.db_update_failed += ecritures.update_failed;
        if ecritures.insert_failed > 0 || ecritures.update_failed > 0 {
            warn!(
                batch = batch_idx,
                insert_failed = ecritures.insert_failed,
                update_failed = ecritures.update_failed,
                "scan_batch_writes_refused — la base a refusé des lignes de ce lot"
            );
        }
    }

    if aggregate.metadata_timeout > 0 {
        warn!(
            count = aggregate.metadata_timeout,
            timeout_secs = FILE_TIMEOUT.as_secs(),
            "scan_timeout_summary — files skipped due to timeout"
        );
    }

    // `metadata_failed` ne répond pas à « ce scan a-t-il perdu des pistes ? » —
    // il répond à « les balises se sont-elles lues ? ». Les deux compteurs
    // d'écriture sont là pour que cette ligne, la seule qu'on aura entre les
    // mains la prochaine fois, ne puisse plus annoncer un scan sans défaut
    // pendant qu'un album entier est refusé par la base (#2939).
    info!(
        total = aggregate.total_files,
        metadata_ok = aggregate.metadata_ok,
        metadata_failed = aggregate.metadata_failed,
        metadata_timeout = aggregate.metadata_timeout,
        db_insert_failed = aggregate.db_insert_failed,
        db_update_failed = aggregate.db_update_failed,
        pistes_perdues = aggregate.a_perdu_des_pistes(),
        "batched_scan_complete"
    );

    aggregate
}

pub fn scan_directories(
    dirs: &[String],
    with_hash: bool,
    progress_callback: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> (Vec<ScannedFile>, ScanStats) {
    let result = list_audio_files(dirs);
    scan_files_parallel(&result.files, with_hash, progress_callback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un disque à plateaux ne parallélise pas : 32 lectures concurrentes font
    /// osciller une tête unique entre 32 endroits. Chaque fichier demande deux
    /// déplacements — les tags au début, le hachage de déduplication à 25 % —
    /// donc 64 en vol sur un seul actionneur. Mesuré chez Yacine : 5,8 fichiers
    /// par seconde sur 49 488 fichiers en USB (#1948).
    ///
    /// Fonction pure, donc testable sans `/sys` ni disque : c'est tout l'intérêt
    /// de l'avoir séparée de la sonde système.
    #[test]
    fn un_disque_a_plateaux_reduit_la_concurrence() {
        assert_eq!(concurrence_pour_disque(Some(true)), 4);
    }

    /// Un SSD garde la valeur d'origine, et surtout un type INCONNU aussi :
    /// hors Linux, ou si `/sys` est illisible, on ne dégrade pas tout le monde
    /// par prudence mal placée.
    #[test]
    fn un_ssd_ou_un_type_inconnu_garde_la_valeur_d_origine() {
        assert_eq!(concurrence_pour_disque(Some(false)), SCAN_IO_CONCURRENCY);
        assert_eq!(concurrence_pour_disque(None), SCAN_IO_CONCURRENCY);
    }

    /// La sonde ne doit jamais paniquer ni bloquer : ce n'est qu'une heuristique
    /// de performance, elle ne doit pas pouvoir empêcher un scan.
    #[test]
    fn la_sonde_ne_panique_jamais() {
        let _ = disque_rotatif();
        // Et la valeur qu'elle produit reste dans les bornes utiles.
        let n = concurrence_pour_disque(disque_rotatif());
        assert!((1..=256).contains(&n), "concurrence hors bornes : {n}");
    }

    /// Le réglage manuel est éprouvé SANS toucher à l'environnement.
    ///
    /// Ce test écrivait dans `TUNE_SCAN_IO_CONCURRENCY` avec `set_var`, en se
    /// croyant protégé parce qu'il restaurait la valeur d'origine. Il ne l'était
    /// pas : `set_var` réécrit le bloc `environ` que tous les autres fils
    /// lisent, et le parcours de bibliothèque en lit un PAR FICHIER
    /// (`is_tune_temp_file` → `std::env::temp_dir()`). Sauver et restaurer LA
    /// variable ne protège de rien — c'est la réécriture elle-même qui est
    /// dangereuse. Voir [`concurrence_depuis_reglage`].
    #[test]
    fn scan_io_concurrency_env_override() {
        // Sans variable, c'est le TYPE DE DISQUE qui décide (#1948) — plus la
        // constante. Comparer à la constante ferait passer ce test par chance
        // sur un runner à SSD, et échouer sur une machine à plateaux.
        let sans_variable = concurrence_pour_disque(disque_rotatif());
        assert_eq!(concurrence_depuis_reglage(None), sans_variable);

        assert_eq!(concurrence_depuis_reglage(Some("8")), 8);
        // Les espaces autour du nombre sont tolérés : un réglage recopié dans
        // un fichier d'unité systemd en traîne souvent.
        assert_eq!(concurrence_depuis_reglage(Some(" 8 ")), 8);

        // Zero, garbage and empty all fall back to the default.
        assert_eq!(concurrence_depuis_reglage(Some("0")), sans_variable);
        assert_eq!(concurrence_depuis_reglage(Some("abc")), sans_variable);
        assert_eq!(concurrence_depuis_reglage(Some("")), sans_variable);

        // Over-large is clamped, not honoured verbatim.
        assert_eq!(concurrence_depuis_reglage(Some("100000")), 256);

        // Et la fonction publique branche bien la variable sur cette décision :
        // une LECTURE de l'environnement, jamais une écriture.
        assert_eq!(
            scan_io_concurrency(),
            concurrence_depuis_reglage(std::env::var("TUNE_SCAN_IO_CONCURRENCY").ok().as_deref())
        );
    }

    #[test]
    fn supported_extensions_list() {
        assert!(SUPPORTED_EXTENSIONS.contains(&"flac"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"mp3"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"dsf"));
        assert!(SUPPORTED_EXTENSIONS.contains(&"ape"));
        assert!(!SUPPORTED_EXTENSIONS.contains(&"wma"));
        assert!(!SUPPORTED_EXTENSIONS.contains(&"dst"));
        assert!(!SUPPORTED_EXTENSIONS.contains(&"txt"));
    }

    /// Les deux listes doivent rester DISJOINTES.
    ///
    /// Ajouter un format à `SUPPORTED_EXTENSIONS` sans le retirer de
    /// `KNOWN_UNREAD_AUDIO` produirait un rapport de scan qui annonce comme
    /// « non lus » des fichiers pourtant indexés — pire qu'un silence, puisque
    /// l'utilisateur irait chercher un problème inexistant.
    #[test]
    fn unread_list_never_overlaps_supported() {
        for e in KNOWN_UNREAD_AUDIO {
            assert!(
                !SUPPORTED_EXTENSIONS.contains(e),
                "{e} est à la fois lu et annoncé comme non lu"
            );
        }
    }

    /// Ce que la liste doit couvrir, et ce qu'elle ne doit surtout pas.
    #[test]
    fn unread_list_targets_audio_only() {
        // Les formats réclamés sur le forum (Rhorn, #1763).
        assert!(KNOWN_UNREAD_AUDIO.contains(&"mpc"));
        assert!(KNOWN_UNREAD_AUDIO.contains(&"cue"));
        assert!(KNOWN_UNREAD_AUDIO.contains(&"wma"));
        assert!(KNOWN_UNREAD_AUDIO.contains(&"asf"));
        assert!(KNOWN_UNREAD_AUDIO.contains(&"dst"));
        // Le bruit d'une bibliothèque musicale ne doit JAMAIS y figurer :
        // compter les pochettes et les fichiers de log noierait le seul
        // renseignement exploitable.
        for noise in ["jpg", "png", "nfo", "m3u", "log", "txt", "accurip", "pdf"] {
            assert!(
                !KNOWN_UNREAD_AUDIO.contains(&noise),
                "{noise} n'est pas de l'audio et polluerait le rapport"
            );
        }
    }

    #[test]
    fn wma_asf_et_dst_sont_expliques_sans_etre_catalogues() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_formats_non_lus_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for name in [
            "album.wma",
            "archive.asf",
            "sacd.dst",
            "temoin.flac",
            "cover.jpg",
        ] {
            std::fs::write(base.join(name), b"fixture").unwrap();
        }

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].ends_with("temoin.flac"));
        assert_eq!(result.skipped_by_ext.get("wma"), Some(&1));
        assert_eq!(result.skipped_by_ext.get("asf"), Some(&1));
        assert_eq!(result.skipped_by_ext.get("dst"), Some(&1));
        assert!(
            result
                .skipped_reasons
                .get("wma")
                .is_some_and(|reason| reason.contains("aucun décodeur"))
        );
        assert!(
            result
                .skipped_reasons
                .get("dst")
                .is_some_and(|reason| reason.contains("aucun décodeur"))
        );
        assert!(!result.skipped_by_ext.contains_key("jpg"));
    }

    /// #2060 — un format que le decodeur sait lire ne doit jamais sortir du
    /// parcours SANS TRACE.
    ///
    /// Le contrat des deux listes n’etait verrouille que dans un sens : tout
    /// format catalogue possede un decodeur. Le sens inverse ne l’etait pas,
    /// et `.oga` — l’extension Ogg que le decodeur, l’ecrivain de tags et la
    /// decision de transcodage reconnaissent tous — n’etait NI catalogue NI
    /// declare non lu. Il retombait donc sur `LibraryAudioSupport::NotAudio`,
    /// c’est-a-dire un `continue` muet : aucune piste, aucun compteur, aucune
    /// ligne de rapport. C’est la seule facon dont un fichier disparait sans
    /// laisser de quoi le chercher.
    ///
    /// Le temoin `album.ogg` est le jumeau exact du fichier fautif : meme
    /// conteneur, meme dossier, meme contenu, meme appel. S’il tombait avec
    /// lui, ce test mesurerait la fixture et non le defaut.
    #[test]
    fn un_fichier_oga_entre_en_bibliotheque_comme_son_jumeau_ogg() {
        // Pas sous temp_dir() : `is_tune_temp_file` y ecarte TOUT.
        let base = crate::test_scratch::scratch_dir_in(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
            "walker-oga-2060",
        );
        for name in ["album.ogg", "album.oga"] {
            std::fs::write(base.join(name), b"fixture").unwrap();
        }
        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let noms: Vec<String> = result
            .files
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        // Temoin : vert avant comme apres le correctif.
        assert!(
            noms.iter().any(|n| n == "album.ogg"),
            "temoin album.ogg perdu — la fixture ou le parcours est en cause, pas .oga : {noms:?}"
        );
        assert!(
            noms.iter().any(|n| n == "album.oga"),
            "album.oga absent du resultat du scan : {noms:?}"
        );
        // Et il ne doit pas davantage etre annonce « non lu » : il est lu.
        assert!(
            !result.skipped_by_ext.contains_key("oga"),
            "oga annonce non lu alors qu’il est indexe : {:?}",
            result.skipped_by_ext
        );
    }

    /// Le parcours retient les DOSSIERS porteurs de feuilles CUE (#1763).
    ///
    /// Une feuille est la seule chose qui explique un album entier absent, et
    /// le parcours la comptait comme un fichier non lu de plus avant de jeter
    /// son chemin. Il ne peut PAS la lire au passage — le parcours doit rester
    /// une opération de répertoire, sans lecture bloquante par fichier sur un
    /// NAS — mais il peut retenir où elle est, pour que l'inventaire la relise
    /// après coup.
    ///
    /// Le témoin `temoin.flac` est là exprès : ce test échouerait aussi si la
    /// reconnaissance des formats DÉJÀ lus régressait en chemin.
    #[test]
    fn le_parcours_retient_les_dossiers_porteurs_de_feuilles_cue() {
        // Chemin suffixé de la clé de tâche : deux agents ont déjà détruit
        // mutuellement leurs fixtures sous un nom commun.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_dossiers_cue_p2_1763");
        let _ = std::fs::remove_dir_all(&base);
        let avec = base.join("Camel - Stationary Traveller");
        let sans = base.join("Genesis - Trespass");
        std::fs::create_dir_all(&avec).unwrap();
        std::fs::create_dir_all(&sans).unwrap();
        // Deux feuilles dans le MÊME dossier : le dossier ne doit être retenu
        // qu'une fois, sinon l'inventaire relirait deux fois le même dossier
        // et compterait ses albums en double.
        std::fs::write(avec.join("face-a.cue"), b"fixture").unwrap();
        std::fs::write(avec.join("face-b.cue"), b"fixture").unwrap();
        std::fs::write(avec.join("image.flac"), b"fixture").unwrap();
        std::fs::write(sans.join("temoin.flac"), b"fixture").unwrap();
        std::fs::write(sans.join("album.wma"), b"fixture").unwrap();

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            result.dossiers_avec_feuille_cue.len(),
            1,
            "un dossier porteur, et un seul, quel que soit le nombre de feuilles \
             qu'il contient — obtenu : {:?}",
            result.dossiers_avec_feuille_cue
        );
        assert!(
            result.dossiers_avec_feuille_cue[0].ends_with("Camel - Stationary Traveller"),
            "c'est le dossier de la feuille qui est retenu, pas la feuille : {:?}",
            result.dossiers_avec_feuille_cue
        );
        // Témoin anti-régression : le `.cue` reste compté comme non lu — cette
        // brique AJOUTE une information, elle n'en retire aucune.
        assert_eq!(result.skipped_by_ext.get("cue"), Some(&2));
        // Témoin anti-régression : les formats déjà reconnus le restent, et le
        // `.wma` déjà écarté l'est toujours.
        let noms: Vec<String> = result
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            noms.contains(&"temoin.flac".to_string()),
            "obtenu : {noms:?}"
        );
        assert!(
            noms.contains(&"image.flac".to_string()),
            "obtenu : {noms:?}"
        );
        assert_eq!(noms.len(), 2, "obtenu : {noms:?}");
        assert_eq!(result.skipped_by_ext.get("wma"), Some(&1));
    }

    /// Une bibliothèque sans la moindre feuille ne paie rien (#1763).
    #[test]
    fn une_bibliotheque_sans_feuille_cue_ne_retient_aucun_dossier() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_sans_cue_p2_1763");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("temoin.flac"), b"fixture").unwrap();
        std::fs::write(base.join("album.mpc"), b"fixture").unwrap();

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert!(result.dossiers_avec_feuille_cue.is_empty());
        // Le Musepack reste compté et nommé : c'est tout ce que Tune peut en
        // dire, faute de décodeur (#1763, Rhorn).
        assert_eq!(result.skipped_by_ext.get("mpc"), Some(&1));
    }

    /// Le parcours retient LESQUELS, pas seulement COMBIEN (#2050).
    ///
    /// C'est ici que la liste demandée se perdait : le parcours classait le
    /// fichier, incrémentait `skipped_by_ext`, puis faisait `continue` — le
    /// chemin n'était écrit nulle part, ni en journal ni en rapport. Le
    /// décompte « 280 fichiers .mpc » ne permet pas de retrouver un album.
    #[test]
    fn le_parcours_nomme_les_fichiers_ecartes_pas_seulement_leur_nombre() {
        // Chemin suffixé de la clé de tâche : deux agents ont déjà détruit
        // mutuellement leurs fixtures sous un nom commun.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_chemins_ecartes_i2050");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for name in ["album.wma", "sacd.dst", "temoin.flac", "cover.jpg"] {
            std::fs::write(base.join(name), b"fixture").unwrap();
        }

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        let ecartes = result.skipped_paths.join("\n");
        assert!(
            ecartes.contains("album.wma"),
            "le chemin du .wma écarté doit figurer dans le rapport, pas seulement \
             son décompte — c'est la demande de #2050.\nListe obtenue :\n{ecartes}"
        );
        assert!(
            ecartes.contains("sacd.dst"),
            "le .dst écarté doit être nommé lui aussi : un seul motif instrumenté \
             sur deux laisse l'utilisateur devant une liste incomplète\n{ecartes}"
        );
        // Le motif accompagne le chemin : « pourquoi » sans « lequel » ne
        // servait à rien, « lequel » sans « pourquoi » ne sert pas plus.
        assert!(
            ecartes.contains("aucun décodeur"),
            "chaque chemin doit porter son motif\n{ecartes}"
        );
        // Contre-épreuve : le bruit d'une bibliothèque ne doit pas noyer la
        // liste. Une pochette n'est pas un fichier « ignoré ».
        assert!(
            !ecartes.contains("cover.jpg"),
            "les fichiers non audio n'ont rien à faire dans la liste\n{ecartes}"
        );
        assert!(
            !ecartes.contains("temoin.flac"),
            "un fichier LU ne doit jamais apparaître comme écarté\n{ecartes}"
        );
    }

    /// Écrit une image `.iso` creuse de plus de 4 Mo, avec ou sans Master TOC.
    ///
    /// Plus de 4 Mo à dessein : c'est le seuil de l'ancien `is_sacd_iso`, celui
    /// qui prenait toute image de données pour un SACD. Une fixture plus petite
    /// laisserait la contre-épreuve passer par un autre chemin que celui du
    /// défaut (#2992).
    fn image_iso_de_test(dossier: &std::path::Path, nom: &str, sacd: bool) -> std::path::PathBuf {
        use std::io::{Seek, SeekFrom, Write};
        let chemin = dossier.join(nom);
        let mut fichier = std::fs::File::create(&chemin).unwrap();
        fichier.seek(SeekFrom::Start(0x800 * 510)).unwrap();
        fichier
            .write_all(if sacd { b"SACDMTOC" } else { b"CD001\0\0\0" })
            .unwrap();
        // Fichier creux : la taille est annoncée, les octets ne sont pas
        // écrits — le test ne consomme pas 4 Mo sur disque.
        fichier.set_len(4_200_000).unwrap();
        fichier.flush().unwrap();
        chemin
    }

    /// Un ISO qui n'entre pas en base doit être COMPTÉ et NOMMÉ (#2992).
    ///
    /// Vingt-deux albums SACD de JeromeQ ont disparu de sa bibliothèque sans
    /// qu'aucun écran ne le dise : la branche `Err` de l'extraction émettait un
    /// `warn!` et n'alimentait aucun compteur du rapport de scan. Un scan qui
    /// rend « fichiers=N, erreurs=0 » alors que vingt-deux fichiers ne sont
    /// jamais entrés est un mensonge.
    #[test]
    fn les_iso_ecartes_sont_comptes_et_nommes_dans_le_rapport() {
        // Sous `target/`, jamais sous `/tmp` : `is_tune_temp_file` écarte tout
        // ce qui vit dans le dossier temporaire du système, et une fixture
        // posée là ne serait jamais parcourue. Nom suffixé de la clé de tâche
        // pour ne pas détruire la fixture d'un autre agent.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_iso_ecartes_n2992");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let base = base.as_path();

        // Une image portant bien un Master TOC SACD. L'extraction échouera —
        // `sacd_extract` n'est pas fourni avec Tune, et le contenu est creux —
        // et c'est exactement le cas du journal de JeromeQ.
        image_iso_de_test(base, "Breakfast In America.iso", true);
        // Le faux positif du même journal : une image d'installation.
        image_iso_de_test(base, "ubuntu-26.04-desktop-amd64.iso", false);
        std::fs::write(base.join("temoin.flac"), b"fixture").unwrap();

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(base);

        // Aucun des deux ISO ne devient une piste : ni comme ISO brut, ni comme
        // pistes extraites. C'est le fait à expliquer, pas à taire.
        assert_eq!(
            result.files.len(),
            1,
            "seul le .flac est une piste ; les deux ISO doivent rester dehors.\n\
             Fichiers retenus : {:?}",
            result.files
        );
        assert!(result.files[0].ends_with("temoin.flac"));

        // 1) L'ISO SACD non extrait est compté sous sa propre clé.
        assert_eq!(
            result
                .skipped_by_ext
                .get(crate::audio::iso_sacd::CLE_RAPPORT_ISO_SACD),
            Some(&1),
            "sans ce compteur, le rapport annonce « erreurs : 0 » et l'album \
             disparaît en silence.\nCompteurs obtenus : {:?}",
            result.skipped_by_ext
        );
        // 2) L'image de données est comptée à part : ce n'est pas le même
        //    problème et ce n'est pas le même geste pour l'utilisateur.
        assert_eq!(
            result
                .skipped_by_ext
                .get(crate::audio::iso_sacd::CLE_RAPPORT_ISO_DONNEES),
            Some(&1),
            "une image ISO sans zone SACD doit être signalée comme non audio, \
             pas confondue avec un SACD illisible.\nCompteurs : {:?}",
            result.skipped_by_ext
        );

        // 3) Les motifs sont rendus en clair : « sacd_extract » ne dit rien à
        //    qui lit un rapport de scan.
        let motif_sacd = result
            .skipped_reasons
            .get(crate::audio::iso_sacd::CLE_RAPPORT_ISO_SACD)
            .cloned()
            .unwrap_or_default();
        assert!(
            motif_sacd.contains("ISO SACD"),
            "le motif doit nommer le format concerné : {motif_sacd:?}"
        );
        assert!(
            result
                .skipped_reasons
                .get(crate::audio::iso_sacd::CLE_RAPPORT_ISO_DONNEES)
                .is_some_and(|motif| motif.contains("pas de l'audio")),
            "motifs obtenus : {:?}",
            result.skipped_reasons
        );

        // 4) LESQUELS, pas seulement COMBIEN — « 22 ISO écartés » ne permet pas
        //    de retrouver un album.
        let ecartes = result.skipped_paths.join("\n");
        assert!(
            ecartes.contains("Breakfast In America.iso"),
            "le chemin du SACD écarté doit figurer au rapport\n{ecartes}"
        );
        assert!(
            ecartes.contains("ubuntu-26.04-desktop-amd64.iso"),
            "le chemin de l'image de données doit y figurer aussi\n{ecartes}"
        );
        // Le motif technique exact accompagne le chemin : « outil absent » et
        // « extraction en échec » n'appellent pas la même réponse.
        assert!(
            ecartes.contains("sacd_extract"),
            "la cause technique doit accompagner le chemin\n{ecartes}"
        );
        assert!(
            !ecartes.contains("temoin.flac"),
            "un fichier LU ne doit jamais apparaître comme écarté\n{ecartes}"
        );
    }

    /// Témoin anti-régression : sans ISO, le parcours rend exactement ce qu'il
    /// rendait — aucun compteur d'écart n'apparaît de nulle part (#2992).
    #[test]
    fn une_bibliotheque_sans_iso_ne_gagne_aucun_ecart() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_sans_iso_n2992");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for nom in ["a.flac", "b.mp3", "c.dsf"] {
            std::fs::write(base.join(nom), b"fixture").unwrap();
        }
        std::fs::write(base.join("cover.jpg"), b"fixture").unwrap();

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(result.files.len(), 3);
        assert!(
            result.skipped_by_ext.is_empty(),
            "aucun écart ne doit être inventé sur une bibliothèque saine : {:?}",
            result.skipped_by_ext
        );
        assert!(result.skipped_paths.is_empty());
        assert!(result.error_dirs.is_empty());
    }

    /// Le plafond borne la liste, et le compteur reste exhaustif (#2050).
    #[test]
    fn le_plafond_borne_la_liste_sans_borner_le_compte() {
        let mut liste = Vec::new();
        for i in 0..(PLAFOND_CHEMINS_ECARTES + 25) {
            pousser_chemin_ecarte(&mut liste, format!("/musique/{i}.mpc"));
        }
        assert_eq!(
            liste.len(),
            PLAFOND_CHEMINS_ECARTES,
            "sans plafond, une bibliothèque de 40 000 fichiers écartés produirait \
             un rapport de plusieurs mégaoctets relu à chaque /scan/report"
        );
        // Contre-épreuve : en deçà du plafond, RIEN n'est perdu.
        let mut courte = Vec::new();
        for i in 0..3 {
            pousser_chemin_ecarte(&mut courte, format!("/musique/{i}.mpc"));
        }
        assert_eq!(courte.len(), 3);
        assert_eq!(courte[0], "/musique/0.mpc");
    }

    #[test]
    fn dff_dst_est_inventorie_sans_io_puis_refuse_dans_la_phase_bornee() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_dff_dst_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("album.dff");
        std::fs::write(&path, crate::audio::support::dff_dst_minimal_fixture()).unwrap();

        // L'inventaire ne lit pas l'en-tête : il conserve le DFF pour que la
        // phase de métadonnées, protégée par FILE_TIMEOUT, le classe.
        let listed = list_audio_files(&[dir.to_string_lossy().to_string()]);
        assert_eq!(listed.files, vec![path]);
        assert!(listed.skipped_by_ext.is_empty());

        let (files, stats) = scan_files_parallel(&listed.files, false, None);
        assert_eq!(files.len(), 1);
        assert!(files[0].metadata.is_none());
        assert_eq!(
            files[0]
                .unsupported
                .as_ref()
                .map(|unsupported| unsupported.report_key.as_str()),
            Some("dff-dst")
        );
        assert_eq!(stats.metadata_failed, 0);
        assert!(stats.failed_paths.is_empty());
        assert_eq!(stats.unsupported_by_ext.get("dff-dst"), Some(&1));
        assert!(
            stats
                .unsupported_reasons
                .get("dff-dst")
                .is_some_and(|reason| reason.contains("aucun décodeur DST"))
        );

        // Le serveur emploie le chemin par lots : il doit porter exactement le
        // même verdict et les mêmes compteurs que le chemin parallèle direct.
        let mut batch_files = Vec::new();
        let batch_stats = scan_files_batched(&listed.files, false, 1, |batch, _, _| {
            batch_files.extend(batch);
            EcrituresDuLot::SANS_PERTE
        });
        assert_eq!(batch_files.len(), 1);
        assert!(batch_files[0].unsupported.is_some());
        assert_eq!(batch_stats.metadata_failed, 0);
        // Un import qui ne perd rien laisse les deux compteurs d'écriture à
        // zéro : le résumé ne doit pas devenir alarmiste (#2939).
        assert_eq!(batch_stats.db_insert_failed, 0);
        assert_eq!(batch_stats.db_update_failed, 0);
        assert!(!batch_stats.a_perdu_des_pistes());
        assert_eq!(batch_stats.unsupported_by_ext.get("dff-dst"), Some(&1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_dirs_list() {
        assert!(SKIP_DIRS.contains(&".DS_Store"));
        assert!(SKIP_DIRS.contains(&"@eaDir"));
        assert!(SKIP_DIRS.contains(&"$RECYCLE.BIN"));
    }

    #[test]
    fn list_nonexistent_dir() {
        let result = list_audio_files(&["/tmp/nonexistent_tune_test_dir".into()]);
        // No audio files found; the missing directory is tracked separately.
        assert!(result.files.is_empty());
        assert_eq!(result.missing_dirs.len(), 1);
        assert_eq!(result.missing_dir_reasons.len(), 1);
        assert!(result.error_dirs.is_empty());
    }

    /// La ligne que voit l'utilisateur — celle que `SettingsView.svelte` rend
    /// verbatim — ne doit contenir aucun nom de variante de `std::io::ErrorKind`
    /// ni de `os error N`. C'est tout le sujet de #2357 : JeromeQ a recopié
    /// `Uncategorized — No such device (os error 19)` sans pouvoir en rien
    /// faire. Ici, sur une racine simplement absente, le rendu partait déjà en
    /// `NotFound`.
    #[test]
    fn la_raison_rendue_a_l_ecran_est_une_phrase_pas_un_errno() {
        let result = list_audio_files(&["/tmp/nonexistent_tune_test_dir".into()]);
        let raison = &result.missing_dir_reasons[0];
        for mot in ["NotFound", "Uncategorized", "os error"] {
            assert!(
                !raison.contains(mot),
                "jargon « {mot} » rendu à l'écran : {raison:?}"
            );
        }
        assert!(
            raison.contains("/tmp/nonexistent_tune_test_dir"),
            "le chemin fautif doit être nommé : {raison:?}"
        );
        let bas = raison.to_lowercase();
        assert!(
            bas.contains("n'existe pas") || bas.contains("introuvable") || bas.contains("absent"),
            "la cause doit être dite en français : {raison:?}"
        );
    }

    /// #2356, seconde face : une racine écartée ne doit JAMAIS l'être en
    /// silence. Un chemin configuré qui n'est pas un dossier était sauté avec
    /// un simple `warn!` : absent de `missing_dirs`, il n'apparaissait ni dans
    /// le rapport de scan, ni dans le garde-fou de purge — ses pistes étaient
    /// donc supprimées comme si les fichiers avaient disparu.
    #[test]
    fn une_racine_qui_n_est_pas_un_dossier_est_signalee_et_non_sautee() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_racine_fichier_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let fichier = base.join("ceci_est_un_fichier.txt");
        std::fs::write(&fichier, b"x").unwrap();

        let result = list_audio_files(&[fichier.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            result.missing_dirs.len(),
            1,
            "racine sautée en silence : missing_dirs = {:?}",
            result.missing_dirs
        );
        assert_eq!(result.missing_dir_reasons.len(), 1);
        let raison = &result.missing_dir_reasons[0];
        for mot in ["NotFound", "Uncategorized", "NotADirectory", "os error"] {
            assert!(!raison.contains(mot), "jargon rendu à l'écran : {raison:?}");
        }
        assert!(
            raison.to_lowercase().contains("dossier"),
            "la cause doit être dite : {raison:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_error_subdir_recorded_in_error_dirs() {
        use std::os::unix::fs::PermissionsExt;
        // NOT under temp_dir(): is_tune_temp_file() skips every file inside
        // the system temp dir, which would empty the walk result.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_error_dirs_test");
        let locked = base.join("locked");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("hidden.flac"), b"x").unwrap();
        std::fs::write(base.join("visible.flac"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root (some CI containers): chmod 000 doesn't block the
        // walk, the scenario can't be reproduced — skip.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let result = list_audio_files(&[base.to_string_lossy().to_string()]);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&base);

        // The reachable file is still scanned; the root is NOT "missing"; the
        // unreadable subtree is reported so the prune can protect it instead
        // of deleting its tracks.
        assert_eq!(result.files.len(), 1);
        assert!(result.missing_dirs.is_empty());
        assert!(
            result.error_dirs.iter().any(|d| d.contains("locked")),
            "error_dirs = {:?}",
            result.error_dirs
        );
    }

    #[test]
    fn exclude_patterns_prune_files_and_subtrees() {
        // NOT under temp_dir(): is_tune_temp_file() skips everything there.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_excludes_test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("keep")).unwrap();
        std::fs::create_dir_all(base.join("Incoming")).unwrap();
        std::fs::write(base.join("keep/a.flac"), b"x").unwrap();
        std::fs::write(base.join("Incoming/b.flac"), b"x").unwrap();

        let root = base.to_string_lossy().to_string();
        let all = list_audio_files(&[root.clone()]);
        assert_eq!(all.files.len(), 2);

        // Case-insensitive substring match prunes the whole subtree.
        let filtered = list_audio_files_with_excludes(&[root], &["incoming".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(filtered.files.len(), 1, "files = {:?}", filtered.files);
        assert!(filtered.files[0].to_string_lossy().contains("keep"));
    }

    /// #2203 — le parcours doit rendre compte de lui-même PENDANT qu'il
    /// tourne, pas seulement à la fin.
    ///
    /// La contre-épreuve porte sur le point exact du défaut : il ne suffit pas
    /// qu'une progression soit rendue, il faut qu'elle le soit **avant la
    /// fin**. Un rapport unique déposé une fois le parcours terminé laisserait
    /// l'utilisateur devant le même écran muet — c'est précisément ce que
    /// faisaient déjà `scan_dir_complete` et `audio_files_listed`.
    #[test]
    fn le_parcours_rend_compte_avant_la_fin_et_nomme_le_dossier_courant() {
        // Pas sous temp_dir() : is_tune_temp_file() y écarte tout.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_progression_2203");
        let _ = std::fs::remove_dir_all(&base);
        // Plusieurs dossiers, plusieurs fichiers : sans quoi « avant la fin »
        // n'aurait aucun sens.
        for sous_dossier in ["disque1", "disque2", "disque3"] {
            std::fs::create_dir_all(base.join(sous_dossier)).unwrap();
            for n in 0..4 {
                std::fs::write(base.join(sous_dossier).join(format!("p{n}.flac")), b"x").unwrap();
            }
        }
        let racine = base.to_string_lossy().to_string();

        let mut annonces: Vec<(usize, String, String)> = Vec::new();
        // Cadence nulle : on veut observer le mécanisme, pas attendre 2 s.
        let result = list_audio_files_avec_progression(
            std::slice::from_ref(&racine),
            &[],
            std::time::Duration::ZERO,
            &mut |p| {
                annonces.push((
                    p.fichiers_vus,
                    p.racine.to_string(),
                    p.dossier_courant.to_string(),
                ));
            },
        );
        let _ = std::fs::remove_dir_all(&base);

        // Le parcours rend toujours exactement ce qu'il rendait.
        assert_eq!(result.files.len(), 12, "files = {:?}", result.files);

        assert!(
            !annonces.is_empty(),
            "le parcours n'a rendu AUCUNE progression : c'est le silence de #2203 \
             (3 min 40 sans une ligne chez JP Borderies)"
        );
        // « Avant la fin » : au moins une annonce porte un compte
        // STRICTEMENT inférieur au total. Un unique rapport final en porterait
        // 12 et ne prouverait rien.
        assert!(
            annonces.iter().any(|(vus, _, _)| *vus < 12),
            "aucune annonce ne précède la fin du parcours — un rapport rendu \
             une fois tout terminé laisse l'écran muet exactement comme avant. \
             annonces = {annonces:?}"
        );
        // Le compte ne recule jamais : il agrège toutes les racines.
        for paire in annonces.windows(2) {
            assert!(
                paire[1].0 >= paire[0].0,
                "le compte a reculé : {:?} puis {:?}",
                paire[0],
                paire[1]
            );
        }
        // La racine est nommée, et le dossier courant descend réellement dans
        // l'arborescence — c'est ce qui dit OÙ un parcours s'enlise.
        assert!(
            annonces.iter().all(|(_, r, _)| r == &racine),
            "racine mal rendue : {annonces:?}"
        );
        assert!(
            annonces.iter().any(|(_, _, d)| d.contains("disque1")
                || d.contains("disque2")
                || d.contains("disque3")),
            "le dossier courant ne descend jamais dans l'arborescence : {annonces:?}"
        );
    }

    /// La cadence est ce qui empêche une bibliothèque de 58 000 fichiers de
    /// produire 58 000 lignes de journal. Une cadence longue ⇒ au plus une
    /// annonce sur un parcours qui dure quelques millisecondes.
    #[test]
    fn la_cadence_borne_le_nombre_d_annonces() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tune_walker_cadence_2203");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for n in 0..50 {
            std::fs::write(base.join(format!("p{n}.flac")), b"x").unwrap();
        }
        let racine = base.to_string_lossy().to_string();

        let mut annonces = 0usize;
        let result = list_audio_files_avec_progression(
            std::slice::from_ref(&racine),
            &[],
            std::time::Duration::from_secs(3600),
            &mut |_| annonces += 1,
        );
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(result.files.len(), 50);
        assert!(
            annonces <= 1,
            "la cadence n'est pas respectée : {annonces} annonces pour un \
             parcours de quelques millisecondes"
        );
    }

    #[test]
    fn scan_empty() {
        let (results, stats) = scan_directories(&[], false, None);
        assert!(results.is_empty());
        assert_eq!(stats.total_files, 0);
    }

    #[test]
    fn normalize_path_trailing_slash() {
        assert_eq!(normalize_path("/music/"), "/music");
        assert_eq!(normalize_path("/music"), "/music");
    }

    #[test]
    fn normalize_path_empty() {
        assert_eq!(normalize_path(""), "");
        assert_eq!(normalize_path("  "), "");
    }

    #[test]
    fn normalize_path_root() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_path_whitespace() {
        assert_eq!(normalize_path("  /music/flac  "), "/music/flac");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_backslash() {
        assert_eq!(
            normalize_path("C:\\Users\\Bob\\Music"),
            "C:\\Users\\Bob\\Music"
        );
        assert_eq!(
            normalize_path("C:\\Users\\Bob\\Music\\"),
            "C:\\Users\\Bob\\Music"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_forward_slash() {
        assert_eq!(
            normalize_path("C:/Users/Bob/Music"),
            "C:\\Users\\Bob\\Music"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_drive_root() {
        assert_eq!(normalize_path("C:\\"), "C:\\");
        assert_eq!(normalize_path("D:\\"), "D:\\");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn normalize_path_windows_unc() {
        assert_eq!(normalize_path("\\\\NAS\\Musique"), "\\\\NAS\\Musique");
        assert_eq!(normalize_path("//NAS/Musique"), "\\\\NAS\\Musique");
    }

    // ————————————————————————————————————————————————————————————————
    // #2889 — deux racines imbriquées énuméraient deux fois le même fichier
    // ————————————————————————————————————————————————————————————————

    /// Une racine de bibliothèque de test, **hors du dossier temporaire du
    /// système**, et supprimée par `Drop` (#3030).
    ///
    /// Le « hors de `temp_dir()` » n'est pas un détail de confort : le parcours
    /// écarte tout fichier situé sous le dossier temporaire du système
    /// (`scanner::is_tune_temp_file`, dernière ligne — `path.starts_with(temp_dir())`),
    /// pour qu'une bibliothèque enracinée au-dessus de `%TEMP%` n'indexe pas
    /// les transcodages de Tune. Une fixture posée dans `temp_dir()` rendrait
    /// donc un parcours VIDE — mesuré : les six cas de #2889 rendaient `[]`
    /// avec `tempfile::tempdir()`, un rouge qui ne prouvait rien du défaut
    /// visé. `target/` est le seul emplacement à la fois inscriptible,
    /// hors `temp_dir()`, et déjà ignoré par git.
    fn racine_de_test(etiquette: &str) -> crate::test_scratch::ScratchDir {
        let sous_target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&sous_target).expect("création de target/");
        crate::test_scratch::scratch_dir_in(sous_target, etiquette)
    }

    /// Pose un fichier d'extension audio reconnue. Le parcours ne lit AUCUN
    /// octet — il classe par extension — donc un fichier vide suffit et le test
    /// reste une opération de répertoire, sans fixture binaire.
    fn poser_piste(dossier: &std::path::Path, nom: &str) -> PathBuf {
        std::fs::create_dir_all(dossier).expect("création du dossier de test");
        let chemin = dossier.join(nom);
        std::fs::write(&chemin, b"").expect("écriture de la piste de test");
        chemin
    }

    /// Le fait de base du ticket : avec une racine imbriquée dans une autre,
    /// chaque fichier n'est énuméré qu'une fois.
    ///
    /// Chez JeromeQ, `/mnt/eversolo_nvme` et `/mnt/eversolo_nvme/77A6-799D`
    /// étaient tous deux déclarés. La boucle `for dir in dirs` parcourait la
    /// seconde racine une fois pour elle-même et une fois à travers la
    /// première : chaque fichier du sous-arbre entrait deux fois dans `files`,
    /// donc deux fois dans la phase de métadonnées, la plus coûteuse du scan.
    #[test]
    fn une_racine_imbriquee_n_enumere_pas_deux_fois_le_meme_fichier() {
        // `TempDir` nettoie par `Drop` — jamais de chemin temporaire composé à
        // la main (#3030).
        let base = racine_de_test("p2a2889-imbriquee");
        let parent = base.join("bibliotheque");
        let enfant = parent.join("montage");
        poser_piste(&parent, "au-dessus.flac");
        poser_piste(&enfant, "en-dessous.flac");

        let result = list_audio_files(&[
            parent.to_string_lossy().to_string(),
            enfant.to_string_lossy().to_string(),
        ]);

        let en_dessous = result
            .files
            .iter()
            .filter(|f| f.ends_with("en-dessous.flac"))
            .count();
        assert_eq!(
            en_dessous, 1,
            "le fichier de la racine imbriquée est énuméré {en_dessous} fois : {:?}",
            result.files
        );
        assert_eq!(
            result.files.len(),
            2,
            "l'union des deux racines vaut deux fichiers : {:?}",
            result.files
        );
        // La racine absorbée n'est pas « manquante » : ses fichiers sont bien
        // là, par la racine parente. La confondre avec une racine illisible
        // déclencherait la protection de purge sur un sous-arbre sain.
        assert!(
            result.missing_dirs.is_empty(),
            "racine absorbée comptée comme manquante : {:?}",
            result.missing_dirs
        );
    }

    /// Le TÉMOIN, vert des deux côtés du correctif : deux racines réellement
    /// distinctes restent deux, même quand leurs noms se ressemblent.
    ///
    /// C'est le contrôle de collision demandé par le dépôt : une clé de
    /// déduplication trop large fusionnerait `Musique` et `Musique-2`, ou
    /// `Jazz` et `Jazz Live`, et ferait disparaître une bibliothèque entière.
    #[test]
    fn deux_racines_reellement_distinctes_restent_deux() {
        let base = racine_de_test("p2a2889-distinctes");
        // Des noms qui se ressemblent par préfixe de CHAÎNE — c'est
        // exactement le piège d'un `starts_with` posé sur du texte.
        let noms = [
            "Musique",
            "Musique-2",
            "Musique 2",
            "Musiques",
            "MusiqueBis",
            "Jazz",
            "Jazz Live",
        ];
        let mut racines = Vec::new();
        for nom in noms {
            let dossier = base.join(nom);
            poser_piste(&dossier, "piste.flac");
            racines.push(dossier.to_string_lossy().to_string());
        }

        let result = list_audio_files(&racines);

        assert_eq!(
            result.files.len(),
            noms.len(),
            "{} racines distinctes doivent rendre {} fichiers, obtenu {:?}",
            noms.len(),
            noms.len(),
            result.files
        );
        for nom in noms {
            let attendu = base.join(nom).join("piste.flac");
            assert!(
                result.files.iter().any(|f| f == &attendu),
                "la racine {nom} a été avalée : {:?}",
                result.files
            );
        }
    }

    /// Le cas symétrique : deux racines qui désignent le MÊME dossier par des
    /// chemins différents. La chaîne brute ne les rapproche pas ; le chemin
    /// canonique, si.
    #[test]
    fn deux_chemins_pour_le_meme_dossier_ne_font_qu_une_racine() {
        let base = racine_de_test("p2a2889-meme-dossier");
        let reel = base.join("bibliotheque");
        poser_piste(&reel, "unique.flac");

        // Trois écritures du même dossier : telle quelle, avec un aller-retour
        // `..`, et avec une barre finale.
        let detour = base.join("bibliotheque/../bibliotheque");
        let racines = vec![
            reel.to_string_lossy().to_string(),
            detour.to_string_lossy().to_string(),
            format!("{}/", reel.to_string_lossy()),
        ];

        let result = list_audio_files(&racines);
        assert_eq!(
            result.files.len(),
            1,
            "le même dossier écrit de trois façons rend {:?}",
            result.files
        );
    }

    /// Même cas symétrique, par lien symbolique — la forme qu'on rencontre
    /// réellement sur un serveur : `/music` pointant vers `/mnt/nas/music`.
    #[cfg(unix)]
    #[test]
    fn un_lien_symbolique_vers_une_racine_ne_la_double_pas() {
        let base = racine_de_test("p2a2889-symlink");
        let reel = base.join("bibliotheque");
        poser_piste(&reel, "unique.flac");
        let lien = base.join("raccourci");
        std::os::unix::fs::symlink(&reel, &lien).expect("lien symbolique");

        let result = list_audio_files(&[
            reel.to_string_lossy().to_string(),
            lien.to_string_lossy().to_string(),
        ]);
        assert_eq!(
            result.files.len(),
            1,
            "la racine et son lien symbolique rendent {:?}",
            result.files
        );
    }

    /// Une racine injoignable reste retenue : elle DOIT atteindre la sonde
    /// `read_dir` pour être rapportée dans `missing_dirs` avec son motif — ce
    /// qui déclenche `VerdictPurge::ProtegeIllisible` et empêche la purge de
    /// supprimer ses pistes (#2356). Le dédoublonnage ne doit rien y changer.
    #[test]
    fn une_racine_injoignable_traverse_le_dedoublonnage() {
        let base = racine_de_test("p2a2889-injoignable");
        let reel = base.join("bibliotheque");
        poser_piste(&reel, "unique.flac");
        let absente = base.join("montage-tombe");

        let result = list_audio_files(&[
            reel.to_string_lossy().to_string(),
            absente.to_string_lossy().to_string(),
        ]);
        assert_eq!(result.files.len(), 1);
        assert_eq!(
            result.missing_dirs.len(),
            1,
            "la racine injoignable doit être rapportée : {:?}",
            result.missing_dirs
        );
        assert_eq!(result.missing_dir_reasons.len(), 1);
    }

    /// Une racine parente ILLISIBLE ne doit pas avaler son enfant lisible :
    /// sans quoi le sous-arbre sain ne serait parcouru par personne.
    #[cfg(unix)]
    #[test]
    fn une_racine_parente_illisible_n_avale_pas_son_enfant_lisible() {
        use std::os::unix::fs::PermissionsExt;
        let base = racine_de_test("p2a2889-parent-illisible");
        let parent = base.join("parent");
        let enfant = parent.join("enfant");
        poser_piste(&enfant, "piste.flac");
        // 0o300 : traversable (`x`) mais non listable (`r`) — `read_dir` échoue,
        // le chemin de l'enfant reste atteignable.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300))
            .expect("droits du parent");

        let result = list_audio_files(&[
            parent.to_string_lossy().to_string(),
            enfant.to_string_lossy().to_string(),
        ]);
        // Remettre les droits AVANT les assertions, sinon le `Drop` du
        // `TempDir` échoue à nettoyer et le test fuit un dossier (#3030).
        let _ = std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700));

        assert_eq!(
            result.files.len(),
            1,
            "l'enfant lisible doit rester parcouru : {:?}",
            result.files
        );
        assert_eq!(
            result.missing_dirs.len(),
            1,
            "le parent illisible doit être rapporté : {:?}",
            result.missing_dirs
        );
    }

    /// Le décompte que la revue réclame, mesuré et non estimé : combien de
    /// racines la nouvelle clé ABSORBE, et combien elle en fait COLLISIONNER.
    ///
    /// Une déduplication ne se juge pas aux doublons qu'elle évite : elle se
    /// juge aussi aux racines légitimes qu'elle confond. Les deux chiffres
    /// sortent ici du même jeu de racines.
    #[test]
    fn le_dedoublonnage_compte_ses_absorptions_et_ses_collisions() {
        let base = racine_de_test("p2a2889-comptes");
        let bibliotheque = base.join("Musique");
        let imbriquee = bibliotheque.join("Jazz");
        std::fs::create_dir_all(&imbriquee).expect("arborescence");
        let distinctes = [
            base.join("Musique-2"),
            base.join("Musiques"),
            base.join("Jazz"),
        ];
        for d in &distinctes {
            std::fs::create_dir_all(d).expect("arborescence");
        }

        let racines: Vec<String> = std::iter::once(bibliotheque.clone())
            .chain(std::iter::once(imbriquee.clone()))
            // le même dossier, écrit deux fois
            .chain(std::iter::once(bibliotheque.clone()))
            .chain(distinctes.iter().cloned())
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let verdict = dedoublonner_racines(&racines);

        // CHIFFRE 1 — doublons évités : la racine imbriquée, plus la
        // répétition littérale de la racine parente.
        assert_eq!(
            verdict.absorbees.len(),
            2,
            "absorptions inattendues : {:?}",
            verdict.absorbees
        );
        assert!(
            verdict
                .absorbees
                .iter()
                .any(|a| a.motif == MotifAbsorption::Imbriquee),
            "l'imbrication n'a pas été reconnue : {:?}",
            verdict.absorbees
        );
        assert!(
            verdict
                .absorbees
                .iter()
                .any(|a| a.motif == MotifAbsorption::Identique),
            "la répétition littérale n'a pas été reconnue : {:?}",
            verdict.absorbees
        );

        // CHIFFRE 2 — collisions : aucune racine réellement distincte ne doit
        // disparaître. C'est le chiffre que la règle du dépôt réclame à côté
        // du premier, et il doit valoir zéro.
        let retenues: Vec<&str> = verdict.retenues.iter().map(String::as_str).collect();
        let mut collisions = 0usize;
        for d in &distinctes {
            let attendu = d.to_string_lossy().to_string();
            if !retenues.contains(&attendu.as_str()) {
                collisions += 1;
            }
        }
        assert_eq!(
            collisions, 0,
            "{collisions} racine(s) distincte(s) avalée(s) : attendu {distinctes:?}, retenues {retenues:?}"
        );
        assert_eq!(
            verdict.retenues.len(),
            1 + distinctes.len(),
            "retenues : {retenues:?}"
        );
    }
}
