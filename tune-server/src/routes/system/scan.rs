use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

use std::sync::atomic::{AtomicU64, Ordering};

const SCAN_ACTIVE: u64 = 1;
const SCAN_CANCELLED: u64 = 2;
const SCAN_FLAGS: u64 = SCAN_ACTIVE | SCAN_CANCELLED;

/// Porte unique partagée par les scans manuel, planifié et de démarrage.
///
/// L'état tient dans un seul atomique afin que l'acquisition d'un nouveau scan
/// et la remise à zéro de son annulation soient UNE opération. Deux atomiques
/// séparés laisseraient cette course : le scan A se termine, B démarre et remet
/// l'annulation à zéro, puis une requête Stop destinée à A annule B. Le numéro
/// de génération empêche cette fuite entre propriétaires successifs.
struct ScanGate {
    state: AtomicU64,
}

impl ScanGate {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    /// Acquiert l'unique droit de scanner. Un refus ne modifie aucun bit : en
    /// particulier il ne désarme jamais l'annulation du propriétaire actif.
    fn try_acquire(&self) -> Option<ScanLease<'_>> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & SCAN_ACTIVE != 0 {
                return None;
            }
            let mut generation = (observed & !SCAN_FLAGS).wrapping_add(SCAN_FLAGS + 1);
            if generation == 0 {
                // Théorique après 2^62 scans : zéro reste réservé à l'état
                // initial pour garder les diagnostics lisibles.
                generation = SCAN_FLAGS + 1;
            }
            let acquired = generation | SCAN_ACTIVE;
            match self.state.compare_exchange(
                observed,
                acquired,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ScanLease {
                        gate: self,
                        generation,
                    });
                }
                Err(current) => observed = current,
            }
        }
    }

    /// Attache Stop à la génération active au moment de la requête.
    fn request_cancel(&self) -> bool {
        let mut observed = self.state.load(Ordering::SeqCst);
        if observed & SCAN_ACTIVE == 0 {
            return false;
        }
        let generation = observed & !SCAN_FLAGS;
        loop {
            if observed & !SCAN_FLAGS != generation || observed & SCAN_ACTIVE == 0 {
                return false;
            }
            if observed & SCAN_CANCELLED != 0 {
                return true;
            }
            match self.state.compare_exchange(
                observed,
                observed | SCAN_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(current) => observed = current,
            }
        }
    }

    fn cancel_requested(&self) -> bool {
        self.state.load(Ordering::SeqCst) & (SCAN_ACTIVE | SCAN_CANCELLED)
            == (SCAN_ACTIVE | SCAN_CANCELLED)
    }

    fn release(&self, generation: u64) {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & !SCAN_FLAGS != generation || observed & SCAN_ACTIVE == 0 {
                return;
            }
            match self.state.compare_exchange(
                observed,
                generation,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }
}

/// Jeton non clonable gardé jusqu'à la terminaison réelle de la tâche.
pub(crate) struct ScanLease<'a> {
    gate: &'a ScanGate,
    generation: u64,
}

impl Drop for ScanLease<'_> {
    fn drop(&mut self) {
        self.gate.release(self.generation);
    }
}

static SCAN_GATE: ScanGate = ScanGate::new();

pub(crate) fn try_begin_scan() -> Option<ScanLease<'static>> {
    SCAN_GATE.try_acquire()
}

/// Whether "Stop scan" was requested. Polled by both the manual and the startup
/// scan batch loops so either can be cancelled cooperatively.
pub(crate) fn scan_cancel_requested() -> bool {
    SCAN_GATE.cancel_requested()
}

#[cfg(test)]
mod scan_gate_tests {
    use super::ScanGate;

    /// Reproduit directement #2459 : Stop est demandé sur A, puis une seconde
    /// requête tente de démarrer B. Le refus de B ne doit jamais remettre le
    /// bit d'annulation de A à zéro.
    #[test]
    fn un_second_depart_refuse_ne_desarme_pas_stop() {
        let gate = ScanGate::new();
        let scan_a = gate.try_acquire().expect("le premier scan doit demarrer");

        assert!(gate.request_cancel());
        assert!(gate.cancel_requested());
        assert!(gate.try_acquire().is_none(), "le scan B doit etre refuse");
        assert!(
            gate.cancel_requested(),
            "le refus de B ne doit pas desarmer l'annulation de A"
        );

        drop(scan_a);
        let _scan_c = gate
            .try_acquire()
            .expect("la terminaison reelle de A doit liberer la porte");
        assert!(
            !gate.cancel_requested(),
            "une nouvelle generation ne doit pas heriter de Stop"
        );
    }

    /// Une requête Stop sans propriétaire ne doit pas empoisonner le prochain
    /// scan. C'est l'autre moitié de l'attachement à une génération précise.
    #[test]
    fn stop_sans_scan_actif_ne_fuit_pas_vers_le_suivant() {
        let gate = ScanGate::new();
        assert!(!gate.request_cancel());
        let _scan = gate.try_acquire().expect("le scan doit demarrer");
        assert!(!gate.cancel_requested());
    }

    /// Deux requêtes libérées au même instant ne peuvent obtenir qu'un seul
    /// droit d'écriture. Le gagnant garde son jeton jusqu'à ce que les deux
    /// résultats aient été observés, ce qui rend le test déterministe.
    #[test]
    fn deux_departs_concurrents_n_ont_qu_un_proprietaire() {
        use std::sync::{Arc, Barrier, mpsc};

        let gate = ScanGate::new();
        let depart = Arc::new(Barrier::new(3));
        let maintien = Arc::new(Barrier::new(3));
        let (tx, rx) = mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let depart = depart.clone();
                let maintien = maintien.clone();
                let tx = tx.clone();
                let gate = &gate;
                scope.spawn(move || {
                    depart.wait();
                    let lease = gate.try_acquire();
                    tx.send(lease.is_some()).unwrap();
                    maintien.wait();
                    drop(lease);
                });
            }

            depart.wait();
            let resultats = [rx.recv().unwrap(), rx.recv().unwrap()];
            assert_eq!(resultats.into_iter().filter(|gagne| *gagne).count(), 1);
            maintien.wait();
        });

        assert!(
            gate.try_acquire().is_some(),
            "le jeton gagnant doit liberer la porte en fin de tache"
        );
    }
}

/// Racines qui CONTENAIENT des pistes et n'en découvrent plus AUCUNE.
///
/// Un dossier qui passe de milliers de fichiers à zéro n'est pas vide : il est
/// absent. C'est la forme exacte que prend un partage réseau non monté —
/// Dominique COMET, 0.9.73, NAS OpenMediaVault en SMB : « ma bibliothèque
/// disparaît à chaque redémarrage de Tune » (#1652).
///
/// Les gardes existants ne peuvent pas voir ce cas : ils testent
/// `read_dir(root).is_err()`, c'est-à-dire une racine ILLISIBLE. Or un point de
/// montage qui existe mais sur lequel rien n'est monté est parfaitement
/// lisible — et vide. `read_dir` réussit, `missing_dirs` reste vide, et le
/// nettoyage supprime les pistes comme si les fichiers avaient été effacés.
///
/// Zéro n'est donc pas un résultat de scan crédible : c'est une anomalie, et on
/// refuse d'écrire dessus. Le prix de l'erreur est asymétrique — protéger à
/// tort laisse des lignes périmées qu'un scan suivant nettoiera, supprimer à
/// tort détruit la bibliothèque.
///
/// Une racine qui n'avait AUCUNE piste n'est pas concernée : elle n'a rien à
/// perdre, et c'est le cas normal d'un dossier fraîchement configuré.
/// Nombre de pistes disparues sous un même dossier au-delà duquel on refuse
/// de croire à une suppression volontaire.
///
/// Le garde-fou par RACINE ne voit pas un montage **imbriqué** qui tombe : la
/// racine répond encore, `read_dir` réussit, aucune erreur n'est levée — et
/// tout le sous-arbre part sans un avertissement (#1943).
///
/// Le seuil existe pour ne pas empêcher le geste normal : supprimer un album
/// fait disparaître dix à vingt pistes d'un dossier, et ces fantômes-là
/// doivent bien être nettoyés. Un point de montage qui tombe en emporte des
/// centaines. 100 sépare les deux sans ambiguïté.
pub(crate) const SEUIL_SOUS_ARBRE_VIDE: usize = 100;

/// Dossier parent d'un chemin, quel que soit le séparateur.
///
/// Remonter avec `rfind('/')` seul ne trouve RIEN dans
/// `G:\Musique\Jazz\a.flac` : sous Windows la remontée des parents ne
/// s'exécutait jamais, donc `sous_arbres_vides` rendait TOUJOURS une liste
/// vide et le garde-fou du montage imbriqué (#1943) était décoratif sur cette
/// plateforme. Même famille que #1652/#1943 : un séparateur codé en dur qui
/// neutralise une protection en silence.
///
/// `None` en haut de l'arborescence : `/a.flac` n'a pas de parent nommé, et
/// `G:` n'en a pas non plus.
pub(crate) fn dossier_parent(chemin: &str) -> Option<&str> {
    match chemin.rfind(['/', '\\']) {
        Some(0) | None => None,
        Some(i) => Some(&chemin[..i]),
    }
}

/// Sous-arbres devenus vides d'un coup, sous une racine qui répond encore.
///
/// Pour chaque piste absente du disque, on remonte ses dossiers parents. Un
/// dossier qui a perdu au moins [`SEUIL_SOUS_ARBRE_VIDE`] pistes **et** ne
/// présente plus aucun fichier découvert est traité comme un montage absent :
/// tout ce qu'il contenait est conservé.
///
/// Rend les dossiers les plus HAUTS qui qualifient — inutile de lister aussi
/// leurs enfants, `sous_le_dossier` les couvre.
pub(crate) fn sous_arbres_vides(
    existants: &[&str],
    decouverts: &std::collections::HashSet<String>,
) -> Vec<String> {
    use std::collections::{HashMap, HashSet};

    // Dossiers qui présentent encore au moins un fichier : eux vont bien.
    let mut vivants: HashSet<&str> = HashSet::new();
    for p in decouverts {
        let mut cur = p.as_str();
        while let Some(parent) = dossier_parent(cur) {
            cur = parent;
            if !vivants.insert(cur) {
                break; // déjà marqué : ses ancêtres le sont aussi.
            }
        }
    }

    // Pistes perdues par dossier, tous niveaux confondus.
    let mut perdues: HashMap<&str, usize> = HashMap::new();
    for p in existants {
        if decouverts.contains(*p) {
            continue;
        }
        let mut cur = *p;
        while let Some(parent) = dossier_parent(cur) {
            cur = parent;
            *perdues.entry(cur).or_insert(0) += 1;
        }
    }

    let mut candidats: Vec<&str> = perdues
        .into_iter()
        .filter(|(d, n)| *n >= SEUIL_SOUS_ARBRE_VIDE && !vivants.contains(*d))
        .map(|(d, _)| d)
        .collect();
    // Du plus court au plus long, pour ne garder que les ancêtres.
    candidats.sort_by_key(|d| d.len());
    let mut retenus: Vec<String> = Vec::new();
    for d in candidats {
        if !retenus.iter().any(|r| sous_le_dossier(d, r)) {
            retenus.push(d.to_string());
        }
    }
    retenus.sort();
    retenus
}

pub(crate) fn roots_gone_empty(
    roots: &[String],
    existing_paths: &[&str],
    discovered_paths: &std::collections::HashSet<String>,
) -> Vec<String> {
    roots
        .iter()
        .filter(|root| {
            // Via `sous_le_dossier`, et non un préfixe reconstruit ici : la
            // duplication EST la cause. Ce filtre codait `/` en dur, donc sous
            // Windows `had` était toujours faux et ce garde-fou — celui qui
            // empêche d'effacer la bibliothèque quand un partage n'est pas
            // monté (#1652) — ne se déclenchait JAMAIS.
            let had = existing_paths.iter().any(|p| sous_le_dossier(p, root));
            let has = discovered_paths.iter().any(|p| sous_le_dossier(p, root));
            had && !has
        })
        .cloned()
        .collect()
}

/// Le chemin `path` est-il ce répertoire, ou sous lui ?
///
/// `starts_with` seul ne suffit pas : `/mnt/music2` est un préfixe de
/// `/mnt/music22`, et la protection s'appliquerait alors à un dossier voisin
/// — ou pire, ne s'appliquerait pas là où on la croit. Il faut donc exiger
/// qu'un SÉPARATEUR suive le préfixe.
///
/// Les deux séparateurs sont acceptés : `tracks.file_path` contient des
/// ANTISLASHS sous Windows. `normalize_path` (`tune-core/src/scanner/walker.rs`)
/// fait `replace('/', "\\")` sous `cfg(windows)`, et `track_repo.rs` le dit —
/// « the server's `MAIN_SEPARATOR` is the separator stored in
/// `tracks.file_path` », avec l'exemple `G:\Blues 2\%`.
pub(crate) fn sous_le_dossier(path: &str, dossier: &str) -> bool {
    let d = dossier.trim_end_matches(['/', '\\']);
    if path == d {
        return true;
    }
    path.strip_prefix(d)
        .is_some_and(|reste| reste.starts_with('/') || reste.starts_with('\\'))
}

/// Ce que la purge de fin de scan a le droit de faire d'une piste absente du
/// disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictPurge {
    /// Le fichier a vraiment disparu d'une racine saine et lue : on retire.
    Supprimer,
    /// La racine est absente, illisible, ou s'est vidée d'un coup — un montage
    /// qui n'est pas là ne prouve rien sur le contenu.
    ProtegeIllisible,
    /// La piste n'est sous AUCUNE racine configurée. Elle n'est pas
    /// « disparue » : elle est hors périmètre. C'est le trou par lequel
    /// 21 277 pistes de Yacine ont été supprimées (#1943) — un point de
    /// montage avait changé, l'ancienne racine n'était plus configurée, donc
    /// aucune des trois protections ne pouvait la couvrir : elle n'était ni
    /// manquante, ni en erreur, ni vidée, puisque personne n'y était allé.
    HorsPerimetre,
}

/// Décider du sort d'une piste absente du disque, en un seul endroit.
///
/// Cette règle existait en DEUX copies — `routes/system/scan.rs` et
/// `auto_scan.rs` — portant les mêmes trous. Les faire diverger encore serait
/// reproduire #1943 ; les faire vivre ici les corrige des deux côtés à la fois.
pub(crate) fn verdict_purge(
    db_path: &str,
    racines_configurees: &[String],
    missing_dirs: &[String],
    error_dirs: &[String],
    emptied_roots: &[String],
    sous_arbres_vides: &[String],
) -> VerdictPurge {
    if missing_dirs
        .iter()
        .chain(error_dirs.iter())
        .chain(emptied_roots.iter())
        .chain(sous_arbres_vides.iter())
        .any(|d| sous_le_dossier(db_path, d))
    {
        return VerdictPurge::ProtegeIllisible;
    }
    // Une liste de racines VIDE ne veut pas dire « tout est hors périmètre » :
    // elle veut dire qu'on ne sait rien. Ne rien supprimer dans ce cas.
    if racines_configurees.is_empty()
        || !racines_configurees
            .iter()
            .any(|r| sous_le_dossier(db_path, r))
    {
        return VerdictPurge::HorsPerimetre;
    }
    VerdictPurge::Supprimer
}

/// Part maximale de la bibliothèque locale qu'une seule purge peut retirer.
///
/// Aucun plafond n'existait : rien n'empêchait une purge de 100 %. Chez
/// Yacine, 21 277 lignes sur 70 346 — 30 % — sont parties en un cycle sans
/// que rien ne s'y oppose. Une disparition massive est bien plus souvent un
/// montage absent qu'une suppression réelle de fichiers ; au-delà de ce
/// seuil on refuse et on demande à l'utilisateur, plutôt que d'agir.
pub(crate) const PART_MAX_PURGE: f64 = 0.20;

/// La purge dépasse-t-elle le plafond ? `candidats` sont les pistes qui
/// seraient retirées, `total` la population locale examinée.
pub(crate) fn purge_trop_massive(candidats: usize, total: usize) -> bool {
    // En deçà de 50 pistes, un pourcentage n'a pas de sens : retirer 10 pistes
    // sur 20 est banal quand on range sa bibliothèque à la main.
    if total < 50 {
        return false;
    }
    (candidats as f64) / (total as f64) > PART_MAX_PURGE
}

/// Le plafond doit-il refuser cette purge ?
///
/// Le plafond seul était une IMPASSE : au-delà de 20 % il refusait, et le
/// refus se rejouait à l'identique à chaque scan. Le message envoyait pourtant
/// l'utilisateur « relancer le scan une fois les montages vérifiés » — un
/// geste qui ne pouvait pas aboutir, puisque relancer ne change rien au
/// pourcentage. Quelqu'un qui a VRAIMENT effacé un tiers de ses fichiers
/// n'avait aucun moyen de le dire.
///
/// `confirmee` est le nombre de pistes que l'utilisateur a explicitement
/// accepté de perdre (`?confirm_purge=N` sur `/scan`). Trois propriétés :
///
/// 1. **Explicite** : on confirme un NOMBRE, pas un « oui ». Le refus publie
///    ce nombre (log et `scan_result`), donc le geste demandé est exact.
/// 2. **Non rejouable par accident** : une URL confirmant 21 277 pistes
///    n'autorise pas une purge ultérieure de 40 000. Un `?confirm_purge`
///    oublié dans un signet ne peut pas emporter une bibliothèque entière —
///    il ne couvre que l'ampleur déjà constatée.
/// 3. **Jamais une nouvelle impasse** : le compte peut avoir bougé entre le
///    refus et la confirmation (une piste réapparaît, une autre est ajoutée).
///    On honore donc la confirmation tant qu'on ne retire pas PLUS que ce qui
///    a été autorisé — exiger l'égalité stricte recréerait l'impasse qu'on
///    corrige ici.
///
/// Ce drapeau ne lève QUE le plafond volumétrique. Les protections de
/// `verdict_purge` — racine absente, illisible, vidée (#1652), sous-arbre
/// vidé (#1943) — s'appliquent AVANT et ne sont pas concernées : les pistes
/// qu'elles couvrent ne sont jamais dans `candidats`.
pub(crate) fn purge_refusee(candidats: usize, total: usize, confirmee: Option<u64>) -> bool {
    if !purge_trop_massive(candidats, total) {
        return false;
    }
    !matches!(confirmee, Some(autorise) if candidats as u64 <= autorise)
}

/// Pre-scan skip decision: does `path` need (re)scanning, or is it unchanged
/// since the last scan and safe to skip?
///
/// Returns `true` if the file is new, or its mtime/size differ from what the DB
/// last recorded for it; `false` if it's unchanged (skip — don't re-read tags).
///
/// The lookup key is NFC-normalized because the stored `file_path`s (and the
/// `discovered_paths` set) are NFC, while a filename on disk may be NFD (a FR
/// library ripped on macOS, copied to a Synology, read back over SMB). Skipping
/// this normalization was the "scan interminable" bug: every NFD-named file
/// missed the map, failed the skip, and lofty re-read its tags (heavy embedded
/// art) over slow SMB on EVERY scan (Xavier, DS214/18.5k FR).
///
/// The manual scan and the auto/watcher scan MUST share this one implementation
/// so they can't diverge again — they previously held two copies and only one
/// received the NFC fix.
pub(crate) fn file_needs_scan(
    path: &std::path::Path,
    existing_tracks: &std::collections::HashMap<String, (i64, Option<f64>, Option<i64>)>,
) -> bool {
    let path_str: String = path.to_string_lossy().nfc().collect();
    if let Some(&(_, existing_mtime, existing_size)) = existing_tracks.get(path_str.as_str()) {
        if let Ok(file_meta) = path.metadata() {
            let mtime = file_meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let unchanged = existing_mtime.map_or(false, |m| (m - mtime as f64).abs() <= 0.5)
                && existing_size.map_or(false, |s| s == file_meta.len() as i64);
            return !unchanged;
        }
    }
    true
}

#[derive(Deserialize)]
pub(super) struct ScanQuery {
    /// When true, re-process ALL discovered files (bypass the unchanged-file
    /// skip) so stale album_id assignments get re-resolved by (title, artist).
    /// Self-heals DBs corrupted by the old title-only album merge, where a
    /// track's album_id points at a wrong same-titled album. Slower (re-reads
    /// every file's metadata); default false keeps the fast incremental scan.
    force: Option<bool>,
    /// Alias for `force` sent by the clients' "Full scan / Scan complet" button.
    /// The web/Flutter clients pass `?full=true`; without this field serde
    /// silently dropped it, so "Scan complet" behaved like an ordinary
    /// incremental scan and could never re-resolve broken album/artist links —
    /// a rescan then skipped every unchanged file, so only "Vider la
    /// bibliothèque" + cold scan repaired the DB (Yacine, Synology ARM64).
    full: Option<bool>,
    /// Nombre de pistes que l'utilisateur autorise explicitement la purge à
    /// retirer, malgré le plafond volumétrique (`?confirm_purge=21277`).
    ///
    /// DÉLIBÉRÉMENT distinct de `force`/`full`. `force` est le bouton « Scan
    /// complet », que l'on clique pour relire ses fichiers — c'est exactement
    /// ce que clique quelqu'un dont le NAS était hors ligne, pour réparer sa
    /// bibliothèque. Y accrocher l'autorisation de supprimer en masse
    /// recréerait #1943 par la porte de service.
    ///
    /// C'est un NOMBRE et non un booléen : voir `purge_refusee`. Confirmer une
    /// ampleur constatée, ce n'est pas signer un blanc-seing permanent.
    confirm_purge: Option<u64>,
    /// Targeted scan: when set, only this sub-directory is walked instead of
    /// re-walking every configured music dir. On a network mount (SMB/NFS) the
    /// live `notify` watcher receives no events, so the only way to pick up a
    /// few new tracks was a full re-walk of the whole NAS (stat of every file
    /// = a round-trip each) — minutes to hours for 3 new tracks. Point the scan
    /// at just the folder that changed. The path MUST be inside a configured
    /// music dir; the deleted-track prune is scoped to this sub-tree so tracks
    /// elsewhere are never touched.
    path: Option<String>,
}

pub(super) async fn trigger_scan(
    State(state): State<AppState>,
    Query(q): Query<ScanQuery>,
) -> impl IntoResponse {
    let force = q.force.unwrap_or(false) || q.full.unwrap_or(false);
    // Targeted sub-folder scan (empty/blank string = full scan as before).
    let targeted_req: Option<String> = q
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| tune_core::scanner::walker::normalize_path(s));
    if spawn_library_scan_confirmee(state, force, q.confirm_purge, targeted_req).await {
        (StatusCode::ACCEPTED, Json(json!({ "status": "scanning" })))
    } else {
        (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "already_scanning",
                "scanning": true,
            })),
        )
    }
}

/// Ce qu'il est advenu de l'enrichissement automatique lancé après un scan.
///
/// Cette passe est le SEUL chemin qui remplit `artists.image_path` tout seul,
/// et elle est doublement conditionnée : le réglage `enrich_on_scan`, et
/// `Feature::AutoEnrichment`, réservée au Premium (`tune-core/src/license.rs`,
/// `all_premium()`). Quand elle ne part pas, elle ne laissait qu'une ligne
/// `info` au journal — si bien qu'une installation sans licence scanne, ne voit
/// jamais apparaître une seule vignette d'artiste, et n'a **aucun moyen** de
/// savoir que la passe n'a pas eu lieu (#2507, Reivax66, TuneOS Fedora sans
/// licence : « les vignettes des artistes ne s'affichent pas »).
///
/// Le rapport de fin de scan porte donc le motif, exactement pour la raison qui
/// avait fait ajouter `purge_refused` : *un refus qui ne vit que dans les logs
/// n'existe pas pour l'utilisateur*.
///
/// Ce type ne DÉCIDE d'aucune règle d'offre — il ne fait que nommer celle que
/// le code applique déjà. Le bouton manuel « Enrichir les images artistes »
/// (`POST /library/artwork/enrich-artists`) ne passe pas par ici et n'est,
/// lui, soumis à aucune licence ; ce n'est pas à ce type de le changer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuiteDuScan {
    /// La passe a été lancée.
    Demarree,
    /// `enrich_on_scan = false` : l'utilisateur l'a éteinte dans les Réglages.
    EteinteParReglage,
    /// Offre gratuite : `Feature::AutoEnrichment` n'est pas accordée.
    ReserveeAuPremium,
}

impl SuiteDuScan {
    /// Les deux conditions, dans l'ordre où le code les applique.
    ///
    /// Le manque de licence l'emporte sur le réglage éteint : c'est le refus
    /// que l'utilisateur ne peut PAS lever depuis les Réglages, donc le seul
    /// qui mérite d'être annoncé en premier. Annoncer « vous l'avez éteinte »
    /// à un compte gratuit l'enverrait rallumer un interrupteur qui ne change
    /// rien.
    pub(crate) fn decider(enrich_on_scan: bool, sous_licence: bool) -> Self {
        match (sous_licence, enrich_on_scan) {
            (false, _) => Self::ReserveeAuPremium,
            (true, false) => Self::EteinteParReglage,
            (true, true) => Self::Demarree,
        }
    }

    /// La passe part-elle ?
    pub(crate) fn demarree(self) -> bool {
        matches!(self, Self::Demarree)
    }

    /// Code stable du motif, `None` quand la passe part. C'est ce que lisent
    /// le client et le journal ; il ne doit pas changer sans changer les deux.
    pub(crate) fn motif(self) -> Option<&'static str> {
        match self {
            Self::Demarree => None,
            Self::EteinteParReglage => Some("disabled_by_setting"),
            Self::ReserveeAuPremium => Some("premium_required"),
        }
    }

    /// Le bloc publié dans les trois rapports de fin de scan.
    ///
    /// Construit UNE fois et inséré trois fois : les trois `json!` sont des
    /// copies manuelles qui ont déjà divergé deux fois (#2012, #2146), et une
    /// clé posée dans deux d'entre eux sur trois ne casse aucune compilation.
    pub(crate) fn rapport(self) -> Value {
        json!({
            "started": self.demarree(),
            "skipped_reason": self.motif(),
        })
    }
}
/// Spawn a background library scan (fire-and-forget). Shared by the `/scan`
/// endpoint and by `add_music_dir`, so a folder added in Settings is scanned
/// right away instead of only at the next restart (Jean-Pierre: newly-added
/// folders stayed invisible until the app was restarted).
///
/// Cette signature ne peut PAS exprimer de confirmation de purge, et c'est
/// volontaire : ses appelants (ajout d'un dossier, import, réglages) sont des
/// gestes de RÉPARATION de bibliothèque. Aucun d'eux ne doit pouvoir autoriser
/// une suppression de masse. Seul `trigger_scan`, qui porte une intention
/// explicite de l'utilisateur, passe par `spawn_library_scan_confirmee`.
pub(crate) async fn spawn_library_scan(
    state: AppState,
    force: bool,
    targeted_req: Option<String>,
) -> bool {
    spawn_library_scan_confirmee(state, force, None, targeted_req).await
}

/// Comme `spawn_library_scan`, mais peut lever le plafond volumétrique à
/// hauteur de ce que l'utilisateur a explicitement confirmé. Voir
/// `purge_refusee` : ce n'est PAS `force`.
pub(crate) async fn spawn_library_scan_confirmee(
    state: AppState,
    force: bool,
    purge_confirmee: Option<u64>,
    targeted_req: Option<String>,
) -> bool {
    let Some(scan_lease) = try_begin_scan() else {
        tracing::warn!("scan_start_rejected_already_running");
        return false;
    };
    if force {
        tracing::info!("scan_force_full_reresolve — bypassing unchanged-file skip");
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "scanning") {
        tracing::warn!(error = %e, "scan_status_set_failed");
    }
    if let Err(e) = settings.set("scan_started_at", &chrono_now()) {
        tracing::warn!(error = %e, "scan_started_at_set_failed");
    }

    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    // Auto-enrichment after a scan needs BOTH premium AND the user's opt-in.
    // It was previously forced on every Premium account, so a scan of a large
    // library triggered ~20 min of artist-image downloads the user never asked
    // for and could not turn off (JF Paquet: tags already complete, machine
    // busy). Honour the `enrich_on_scan` setting (default on = unchanged
    // behaviour) so it can be disabled from Settings.
    let enrich_on_scan = SettingsRepo::with_backend(state.backend.clone())
        .get("enrich_on_scan")
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true);
    // La licence est interrogée MÊME quand le réglage vaut `false` : sans cela
    // le rapport ne saurait pas dire si le compte est Premium, et l'utilisateur
    // qui rallume le réglage ne découvrirait le second refus qu'au scan suivant.
    let enrichissement_sous_licence = state
        .license
        .check_feature(tune_core::license::Feature::AutoEnrichment)
        .await;
    let suite_du_scan = SuiteDuScan::decider(enrich_on_scan, enrichissement_sous_licence);
    tokio::spawn(async move {
        // Le droit reste détenu jusqu'à la fin RÉELLE de la tâche, y compris si
        // spawn_blocking panique. Le Drop du jeton libère alors la génération.
        let _scan_lease = scan_lease;
        let db_for_panic = db.clone();
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
        let raw_dirs = super::get_music_dirs_list(&db);
        if raw_dirs.is_empty() {
            tracing::warn!("scan_aborted_no_dirs — no music directories configured");
            if let Err(e) = SettingsRepo::with_backend(db).set("scan_status", "idle") {
                tracing::warn!(error = %e, "scan_status_reset_failed");
            }
            // Emit a completion event so the client clears the "scanning" banner.
            // The web UI only drops the banner on `library.scan.completed`; the
            // normal path emits it at the end, but this early return was silent —
            // leaving the panel stuck at "0 scanned, 0 added" forever, with a
            // Stop button that does nothing because the scan already ended
            // (macOS user with no folder yet, #1129).
            event_bus.emit(
                "library.scan.completed",
                json!({
                    "total_files": 0,
                    "inserted": 0,
                    "updated": 0,
                    "skipped": 0,
                    "no_dirs": true,
                }),
            );
            return;
        }

        // Normalize paths for cross-platform compatibility (Windows backslashes, etc.)
        let music_dirs: Vec<String> = raw_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();

        // Resolve a targeted sub-folder scan. The path must be inside a
        // configured music dir (defence against scanning arbitrary paths); if it
        // is not, fall back to a full scan rather than silently doing nothing.
        let targeted: Option<String> = targeted_req.as_ref().and_then(|p| {
            // Via `sous_le_dossier` : TROISIÈME occurrence du même défaut de
            // séparateur, après `sous_le_dossier` lui-même et `roots_gone_empty`
            // (#2016). Le motif construit ici était `{root}/`, avec `/` codé en
            // dur, alors que `music_dirs` et les chemins de la base portent des
            // ANTISLASHS sous Windows.
            //
            // Conséquence mesurée sur .42 (Windows, `D:\data\music`) : AUCUN
            // scan ciblé ne pouvait aboutir — chacun retombait en scan complet,
            // silencieusement, en journalisant « outside music dirs » pour un
            // chemin qui était pourtant dedans.
            if music_dirs.iter().any(|root| sous_le_dossier(p, root)) {
                Some(p.clone())
            } else {
                tracing::warn!(path = %p, dirs = ?music_dirs, "scan_targeted_path_outside_music_dirs — falling back to full scan");
                None
            }
        });
        let scan_dirs: Vec<String> = match &targeted {
            Some(p) => vec![p.clone()],
            None => music_dirs.clone(),
        };

        tracing::info!(
            dirs = ?scan_dirs,
            targeted = ?targeted,
            platform = std::env::consts::OS,
            "scan_starting"
        );

        // Surface an "indexing" phase IMMEDIATELY, before the directory walk and
        // the mtime/size stat pass below. On a large library over a NAS (SMB)
        // both are slow (a 58k-file walk + per-file stat) and used to run in
        // total silence — the panel showed nothing and the Stop button never
        // appeared, so the scan read as "interminable / frozen" (forum, v0.9.12
        // Win11/NAS/58k). This gives the UI an indeterminate panel + a working
        // Stop from t=0. `total: 0` marks it indeterminate until discovery ends.
        event_bus.emit(
            "library.scan.started",
            json!({ "music_dirs": &music_dirs, "phase": "indexing", "total": 0 }),
        );
        event_bus.emit(
            "library.scan.progress",
            json!({ "phase": "indexing", "scanned": 0i64, "added": 0i64, "total": 0i64 }),
        );

        let exclude_patterns = crate::auto_scan::scan_exclude_patterns(&db);
        if !exclude_patterns.is_empty() {
            tracing::info!(patterns = ?exclude_patterns, "scan_exclude_paths_active");
        }
        // Le parcours des dossiers est la phase la plus longue d'un scan sur
        // un partage réseau, et c'était la seule totalement muette : dans la
        // boucle de `walker.rs` aucune trace n'est émise hors chemin d'erreur,
        // et le premier `info!` — `scan_dir_complete` — n'arrive qu'une fois
        // la racine ENTIÈREMENT parcourue. Sur le journal de JP Borderies
        // (3 226 pistes sur un Synology en SMB) cela donne 3 min 40 sans une
        // ligne : indiscernable d'un blocage, pour lui comme pour nous
        // (#2203). Il a annulé, redémarré, puis renoncé — alors que le scan
        // travaillait.
        //
        // On ne change RIEN au parcours : même ordre, mêmes exclusions, même
        // liste rendue. On lit seulement, à cadence fixe, le compte que la
        // boucle tenait déjà sans le dire.
        //
        // `total: 0` marque la barre comme indéterminée : pendant le parcours
        // le total est INCONNU par construction — on ne sait combien de
        // fichiers il y a qu'une fois qu'on les a tous vus. Le client rend
        // alors « n fichiers » et une barre indéterminée, ce qu'il sait déjà
        // faire (SettingsView.svelte) : aucun changement web n'est requis.
        let list_result = tune_core::scanner::walker::list_audio_files_avec_progression(
            &scan_dirs,
            &exclude_patterns,
            tune_core::scanner::walker::CADENCE_PROGRESSION_PARCOURS,
            &mut |p| {
                event_bus.emit(
                    "library.scan.progress",
                    json!({
                        "phase": "indexing",
                        "scanned": p.fichiers_vus as i64,
                        "added": 0i64,
                        "total": 0i64,
                        "current_dir": p.dossier_courant,
                    }),
                );
            },
        );
        let missing_dirs = list_result.missing_dirs;
        let missing_dir_reasons = list_result.missing_dir_reasons;
        let error_dirs = list_result.error_dirs;
        let mut skipped_by_ext = list_result.skipped_by_ext;
        let mut skipped_reasons = list_result.skipped_reasons;
        let files = list_result.files;
        let total_discovered = files.len();

        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().nfc().collect::<String>())
            .collect();

        // Warn loudly for any CONFIGURED root (full scan only) that is reachable
        // yet yielded zero audio files — a mis-pointed or wrong-level music
        // folder. Yacine's real files live under /volume1/daphile_remote/HDD, but
        // /volume1/daphile_remote/Music and the Freebox mount were configured and
        // are empty, so the scan reported discovered=0 and the library looked
        // permanently "stuck". `missing_dirs` (unreachable/unmounted, reported
        // separately with a reason) are excluded here: this flags only roots that
        // ARE reachable but contain nothing.
        if targeted.is_none() {
            for dir in &scan_dirs {
                if missing_dirs.iter().any(|m| m == dir) {
                    continue;
                }
                // `sous_le_dossier` et non un préfixe reconstruit : `{}/` code
                // le séparateur en dur, donc sous Windows AUCUN chemin n'était
                // jamais vu sous sa racine et ce test rendait toujours faux.
                // Il journalisait alors « configured music folder […] contains
                // no audio files » sur une racine pleine — message faux, et
                // faux systématiquement (constaté sur .42, `D:\data\music`).
                //
                // La normalisation NFC reste nécessaire : `discovered_paths`
                // est en NFC, `dir` vient des réglages et peut être en NFD.
                let dir_nfc: String = dir.nfc().collect();
                let has_audio = discovered_paths
                    .iter()
                    .any(|p| sous_le_dossier(p, &dir_nfc));
                if !has_audio {
                    tracing::warn!(
                        dir = %dir,
                        "scan_root_no_audio_files — configured music folder is reachable but contains no audio files (wrong path or empty). Check that it points at the folder holding your music."
                    );
                }
            }
        }

        let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(db.clone());

        // "Separate albums by quality" — when on (default), a quality suffix is
        // appended to the album title so CD and Hi-Res versions become distinct
        // albums. The manual scan must honour it just like the file-watcher
        // (auto_scan) does, otherwise the two paths disagree (Fabien).
        let quality_split = SettingsRepo::with_backend(db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        // Load existing tracks BEFORE scanning to skip unchanged files.
        // A DB read error must ABORT the scan, not degrade into an empty map:
        // with an empty map every file on disk looks new, so a transient DB
        // hiccup would re-insert the whole library as duplicates.
        let existing_tracks = match track_repo.get_all_local_file_info() {
            Ok(map) => map,
            Err(e) => {
                tracing::error!(error = %e, "scan_aborted_existing_tracks_read_failed");
                let settings = SettingsRepo::with_backend(db.clone());
                settings.set("scan_status", "idle").ok();
                event_bus.emit(
                    "library.scan.completed",
                    json!({
                        "total_files": 0,
                        "inserted": 0,
                        "updated": 0,
                        "skipped": 0,
                        "error": format!("database read failed: {e}"),
                    }),
                );
                return;
            }
        };

        // `audio_hash` is a cheap candidate selector, not proof of identity.
        // Keep the candidate paths so every decision that can hide a track is
        // confirmed byte-for-byte (#2664).
        let mut known_hashes = track_repo
            .get_existing_audio_hash_album_paths()
            .unwrap_or_default();

        // Quick stat pass: skip files whose mtime+size haven't changed.
        // Parallelised: each `path.metadata()` is a blocking stat that, over a
        // NAS/SMB mount, carries real round-trip latency; doing 58k of them
        // sequentially was a multi-minute silent stall before the first batch
        // (forum: v0.9.12 Win11/NAS/58k, "scan interminable"). rayon fans the
        // stats across the pool, and SCAN_CANCEL is honoured here too so Stop
        // aborts during this phase, not only during batch processing.
        use rayon::prelude::*;
        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_par_iter()
            .filter(|path| {
                if scan_cancel_requested() {
                    return false;
                }
                // Force mode: re-process everything so album_id is re-resolved.
                if force {
                    return true;
                }
                // Shared with auto_scan so the manual and watcher scans can't
                // diverge on the NFC key handling (the "scan interminable" bug).
                file_needs_scan(path, &existing_tracks)
            })
            .collect();
        let pre_skipped = (total_discovered - files_to_scan.len()) as i64;

        tracing::info!(
            total = total_discovered,
            changed = files_to_scan.len(),
            unchanged = pre_skipped,
            "pre_scan_filter_complete"
        );

        event_bus.emit(
            "library.scan.started",
            json!({
                "music_dirs": &music_dirs,
                "total": total_discovered,
                "to_scan": files_to_scan.len(),
                "unchanged": pre_skipped,
            }),
        );

        // Emit an immediate progress event so the panel shows "0 / total" and a
        // determinate bar right away, instead of sitting at "0 fichiers, 0
        // ajoutés" until the first batch commits — which on a large/slow NAS is
        // many seconds and reads as "stuck / doing nothing" (bug #1129). The
        // per-batch emit below only fires once `processed > 0`, so without this
        // the very start of a scan has no counter at all.
        event_bus.emit(
            "library.scan.progress",
            json!({
                "phase": "files",
                "scanned": pre_skipped,
                "added": 0i64,
                "total": total_discovered as i64,
                "inserted": 0i64,
                "updated": 0i64,
                "skipped": pre_skipped,
            }),
        );

        // --- Batched scan + import ---
        // Parse metadata in parallel (rayon) in chunks of SCAN_BATCH_SIZE,
        // then batch-insert/update each chunk in its own transaction.
        // This gives progressive availability: tracks are queryable after
        // each batch commits, not only when the entire scan finishes.

        let cache_dir = crate::routes::library::artwork_cache_dir();
        let mut inserted = 0i64;
        let mut updated = 0i64;
        let mut db_insert_failed = 0i64;
        let mut db_update_failed = 0i64;
        // `skipped` stays the aggregate the UI already shows. Each cause is
        // broken out below, including only those duplicate candidates whose
        // complete files were confirmed byte-for-byte.
        let mut skipped = pre_skipped;
        let mut skipped_unchanged = pre_skipped;
        let mut skipped_duplicate = 0i64;
        let mut skipped_no_metadata = 0i64;
        let mut skipped_unsupported = 0i64;
        let total_to_scan = files_to_scan.len() as i64;
        let total = total_to_scan + pre_skipped;
        let mut last_progress_emit = std::time::Instant::now();
        let scan_timer_start = std::time::Instant::now();

        // Shared artist/album resolver + Track builder, identical to the auto/
        // startup + watcher scans. Owns the cross-batch caches (artist, album,
        // covers, per-folder album-artist pinning), the per-batch compilation
        // decision, and the artwork-extracted counter.
        let mut importer =
            crate::scan_import::TrackImporter::new(db.clone(), quality_split, cache_dir.clone());

        let batch_size = tune_core::scanner::walker::SCAN_BATCH_SIZE;

        // Process files in batches: parse metadata in parallel, then insert in a transaction
        let scan_stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            batch_size,
            |batch, batch_idx, _total_files| {
                // Cooperative cancellation: once "Stop scan" was pressed, skip
                // all remaining batches so the loop drains quickly and the scan
                // stops (bug #1129 — the old cancel only flipped scan_status but
                // the batch loop kept inserting). Files for the remaining
                // batches were already read by the walker, but no DB work is
                // done for them.
                if scan_cancel_requested() {
                    return;
                }
                // Collect tracks to batch-insert and batch-update
                let mut to_insert: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len());
                let mut to_update: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len() / 4);

                // BEGIN transaction for this batch (SQLite only — PG uses autocommit
                // to avoid "current transaction is aborted" cascading failures)
                let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
                let sqlite_write_guard = (!is_pg).then(crate::sqlite_write_gate::scan_batch);
                if !is_pg {
                    // Se nommer : tout `write_tx` concurrent echouera tant que ce
                    // lot tient la connexion, et sans cette etiquette son message
                    // n'apprend rien (#1997).
                    tune_core::db::tx_holder::declarer("scan:lot");
                    if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                        // A failed BEGIN means a transaction is already open on
                        // the shared connection (a previous batch that didn't
                        // commit). Roll it back and retry so the connection
                        // recovers instead of staying poisoned — which would make
                        // every playback set_queue fail for the rest of the
                        // session (Yves: stuck on the last track during a scan).
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_begin_failed");
                        let _ = db.execute_batch("ROLLBACK");
                        let _ = db.execute_batch("BEGIN IMMEDIATE");
                    }
                }

                // Resolve artists/albums and build the track rows for this batch
                // via the shared importer — the same logic (compilation
                // flattening, classical-soloist album-artist pinning, mbid album
                // resolution, embedded-cover preference, artist images) as the
                // auto/startup + watcher scans. The importer owns the cross-batch
                // caches and the per-(folder,album) compilation decision.
                importer.begin_batch(&batch);

                for sf in &batch {
                    if let Some(unsupported) = &sf.unsupported {
                        tracing::info!(
                            path = %sf.path,
                            format = %unsupported.report_key,
                            reason = unsupported.reason,
                            "scan_track_skipped_unsupported"
                        );
                        skipped += 1;
                        skipped_unsupported += 1;
                        continue;
                    }
                    if sf.metadata.is_none() {
                        tracing::warn!(path = %sf.path, "scan_track_skipped_no_metadata");
                        // Counted in the aggregate too, so `processed` can
                        // actually reach `total` — before this, every failed
                        // file made the progress bar stop short of 100%.
                        skipped += 1;
                        skipped_no_metadata += 1;
                        continue;
                    }

                    // Early-exit: skip unchanged files BEFORE resolving artist/album.
                    // Without this, get_or_create_with_mbid can create a ghost album
                    // entry (with cover art but no tracks) for files that are ultimately
                    // skipped — the root cause of "duplicate covers after rescan" (#593).
                    // Force mode bypasses this so album_id gets re-resolved.
                    if !force {
                        if let Some(&(_existing_id, existing_mtime, existing_size)) =
                            existing_tracks.get(&sf.path)
                        {
                            let file_changed = existing_mtime
                                .map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
                                || existing_size.map_or(true, |s| s != sf.file_size as i64);
                            if !file_changed {
                                skipped += 1;
                                skipped_unchanged += 1;
                                continue;
                            }
                        }
                    }

                    let Some((mut track, _album_id)) = importer.import(sf) else {
                        continue;
                    };

                    // File already exists and has changed → batch update;
                    // otherwise a new file → batch insert. (Unchanged files were
                    // already skipped by the early-exit above.)
                    if let Some(&(existing_id, _, _)) = existing_tracks.get(&sf.path) {
                        track.id = Some(existing_id);
                        to_update.push(track);
                    } else {
                        // The sampled hash only narrows the candidates. A track
                        // is skipped solely when a complete byte comparison
                        // confirms an exact copy in the same album.
                        if let (Some(hash), Some(aid)) = (&track.audio_hash, track.album_id) {
                            let key = (hash.clone(), aid);
                            let candidates = known_hashes.get(&key).cloned().unwrap_or_default();
                            if let Some(existing_path) =
                                tune_core::scanner::hasher::find_byte_identical_path(
                                    std::path::Path::new(&sf.path),
                                    &candidates,
                                )
                            {
                                tracing::debug!(
                                    audio_hash = %hash,
                                    album_id = aid,
                                    path = %sf.path,
                                    existing_path = %existing_path,
                                    "skip_duplicate_audio_hash"
                                );
                                skipped += 1;
                                skipped_duplicate += 1;
                                continue;
                            }
                            if !candidates.is_empty() {
                                tracing::warn!(
                                    audio_hash = %hash,
                                    album_id = aid,
                                    path = %sf.path,
                                    candidates = candidates.len(),
                                    "audio_hash_candidate_not_byte_identical"
                                );
                            }
                        }
                        to_insert.push(track);
                    }
                }

                // Collect extended metadata for tracks in this batch
                let mut extended_meta_paths: Vec<String> = Vec::new();
                for sf in &batch {
                    if sf.metadata.is_some() {
                        extended_meta_paths.push(sf.path.clone());
                    }
                }

                // Batch insert + update using prepared statements. Per-row
                // failures inside create_batch/update_batch are logged there
                // and swallowed — count the shortfall so the report shows
                // tracks that were scanned but never made it into the DB.
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as i64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as i64;
                // Only successful whole batches enter the in-memory index. A
                // failed insert must never cause a later file to be hidden.
                if batch_inserted == to_insert.len() as i64 {
                    for track in &to_insert {
                        if let (Some(hash), Some(album_id), Some(path)) =
                            (&track.audio_hash, track.album_id, &track.file_path)
                        {
                            known_hashes
                                .entry((hash.clone(), album_id))
                                .or_default()
                                .push(path.clone());
                        }
                    }
                }
                db_insert_failed += to_insert.len() as i64 - batch_inserted;
                db_update_failed += to_update.len() as i64 - batch_updated;
                inserted += batch_inserted;
                updated += batch_updated;

                // Store extended metadata (composer, conductor, ReplayGain, MusicBrainz, etc.)
                // in the track_metadata table. Read extended tags and batch-insert.
                {
                    let meta_repo = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();

                    for path_str in &extended_meta_paths {
                        let path = std::path::Path::new(path_str);
                        // Look up the track_id by file_path
                        if let Ok(Some(track)) = track_repo.get_by_path(path_str) {
                            if let Some(track_id) = track.id {
                                let ext_meta = tune_core::metadata::read_extended_metadata(path);
                                if !ext_meta.is_empty() {
                                    meta_entries.push((track_id, ext_meta));
                                }
                            }
                        }
                    }

                    if !meta_entries.is_empty() {
                        if let Err(e) = meta_repo.set_batch_multi(&meta_entries) {
                            tracing::warn!(error = %e, "scan_extended_metadata_insert_failed");
                        }
                    }
                }

                // Update track_count + album stats for albums touched in this batch
                // so albums are never visible with 0 tracks between batches.
                {
                    let touched_album_ids: std::collections::HashSet<i64> = to_insert
                        .iter()
                        .chain(to_update.iter())
                        .filter_map(|t| t.album_id)
                        .collect();
                    if !touched_album_ids.is_empty() {
                        let ids_csv: String = touched_album_ids
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        db.execute_batch(&format!(
                            "UPDATE albums SET track_count = \
                             (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id) \
                             WHERE id IN ({ids_csv});\
                             UPDATE albums SET \
                             format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                             sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                             bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                             genre = COALESCE(NULLIF(albums.genre, ''), (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1)), \
                             disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id)) \
                             WHERE id IN ({ids_csv})"
                        )).ok();
                    }
                }

                // COMMIT this batch -- tracks + album stats are now queryable
                if !is_pg {
                    // Liberer meme si le COMMIT echoue : une etiquette perimee
                    // accuserait un innocent au prochain incident.
                    tune_core::db::tx_holder::liberer();
                    if let Err(e) = db.execute_batch("COMMIT") {
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_commit_failed");
                        // Don't leave a half-open transaction poisoning the
                        // shared connection for subsequent writes.
                        let _ = db.execute_batch("ROLLBACK");
                    }
                }
                drop(sqlite_write_guard);

                // Emit progress after each batch
                let processed = inserted + updated + skipped;
                let elapsed = last_progress_emit.elapsed();
                if processed > 0
                    && (batch_idx % 2 == 0 || elapsed >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();

                    // Compute scan rate and ETA
                    let elapsed_secs = scan_timer_start.elapsed().as_secs_f64().max(0.001);
                    let tracks_per_second = processed as f64 / elapsed_secs;
                    let remaining = (total - processed).max(0);
                    let eta_seconds = if tracks_per_second > 0.0 {
                        (remaining as f64 / tracks_per_second) as u64
                    } else {
                        0
                    };

                    event_bus.emit(
                        "library.scan.progress",
                        json!({
                            "phase": "files",
                            "scanned": processed,
                            "added": inserted,
                            "total": total,
                            "batch": batch_idx,
                            "inserted": inserted,
                            "updated": updated,
                            "skipped": skipped,
                            "tracks_per_second": (tracks_per_second * 10.0).round() / 10.0,
                            "eta_seconds": eta_seconds,
                        }),
                    );
                }
            },
        );

        // Les extensions manifestement non prises en charge sont comptées dès
        // le parcours. Le cas DFF/DST exige une lecture d'en-tête : elle est
        // faite dans la phase bornée ci-dessus, puis fusionnée dans le même
        // contrat de rapport.
        for (format, count) in &scan_stats.unsupported_by_ext {
            *skipped_by_ext.entry(format.clone()).or_insert(0) += count;
        }
        skipped_reasons.extend(scan_stats.unsupported_reasons.clone());

        // Album covers extracted during the scan (owned by the importer).
        let artwork_extracted = importer.artwork_extracted() as i64;

        // Prune tracks whose files no longer exist on disk.
        // SAFETY: skip tracks in missing directories — the volume/NAS may
        // simply be unmounted. Deleting them would wipe the entire library.
        // Same protection for `error_dirs`: a subtree where the WALK itself
        // errored (unreadable subfolder, SMB stall mid-scan) has files that
        // exist but never made it into `discovered_paths`.
        // A cancelled scan never prunes: Stop must never be destructive.
        // Hissé hors du bloc : la réconciliation des favoris, plus bas, doit
        // savoir qu'une racine s'est vidée. Elle l'ignorait, et supprimait
        // DÉFINITIVEMENT les favoris de pistes pourtant conservées (#1943).
        // Hissés pour le rapport : l'écran n'affichait que « purge terminée »,
        // et seul `journalctl` portait l'alerte (#1943, cf. #1190). Un
        // utilisateur qui ne voit rien croit que tout va bien.
        let mut racines_videes: Vec<String> = Vec::new();
        let mut sous_arbres_proteges: Vec<String> = Vec::new();
        let mut pistes_hors_perimetre = 0i64;
        let mut pistes_protegees = 0i64;
        // Pistes réellement retirées de la base. Hissé pour la même raison que
        // ses voisines, et pour une de plus : le compte existait déjà, mais il
        // mourait avec le bloc `else` ci-dessous. Il ne sortait que par le
        // journal et par un `library.scan.progress` que le client efface dès
        // l'arrivée de `library.scan.completed` — si bien que le bandeau de fin
        // de scan annonçait « 0 supprimés » quoi que la purge ait fait (#2146).
        let mut pistes_supprimees = 0i64;
        // > 0 quand le plafond a refusé : c'est le nombre à renvoyer dans
        // `?confirm_purge=` pour sortir de l'impasse. 0 = aucun refus.
        let mut purge_refusee_candidats = 0i64;
        if scan_cancel_requested() {
            tracing::info!("post_scan_prune_skipped_cancelled");
        } else {
            // Racines devenues vides : un partage non monté est LISIBLE et
            // vide, donc invisible pour `missing_dirs`. Sans ce garde, le
            // nettoyage ci-dessous efface la bibliothèque entière (#1652).
            let existing_refs: Vec<&str> =
                existing_tracks.keys().map(|s| s.as_str()).collect();
            racines_videes = roots_gone_empty(&scan_dirs, &existing_refs, &discovered_paths);
            let emptied_roots = &racines_videes;
            // Un montage IMBRIQUÉ qui tombe laisse la racine répondre : ni
            // `missing_dirs`, ni `error_dirs`, ni `emptied_roots` ne le voient,
            // et tout le sous-arbre partait sans un mot (#1943).
            sous_arbres_proteges = sous_arbres_vides(&existing_refs, &discovered_paths);
            let sous_arbres = &sous_arbres_proteges;
            if !sous_arbres.is_empty() {
                tracing::error!(
                    dossiers = ?sous_arbres,
                    seuil = SEUIL_SOUS_ARBRE_VIDE,
                    "post_scan_sous_arbre_vide — ces dossiers ont perdu leurs pistes d'un coup \
                     alors que leur racine répond. Montage imbriqué absent ? CONSERVÉES."
                );
            }
            if !emptied_roots.is_empty() {
                tracing::error!(
                    roots = ?emptied_roots,
                    "post_scan_root_went_empty — ce dossier contenait des pistes et n'en présente plus aucune. Montage absent ? Les pistes sont CONSERVÉES."
                );
            }
            let mut pruned = 0i64;
            let mut protected = 0i64;
            let mut hors_perimetre = 0i64;
            // Décider AVANT de supprimer : le plafond volumétrique a besoin de
            // connaître l'ampleur totale, et une suppression au fil de la
            // boucle ne se rattrape pas.
            let mut a_supprimer: Vec<i64> = Vec::new();
            let mut examinees = 0usize;
            for (db_path, &(track_id, _, _)) in &existing_tracks {
                // Targeted scan: only consider tracks under the scanned sub-tree.
                // `discovered_paths` only holds files below that folder, so a
                // track anywhere else would look "missing" and get wrongly
                // deleted — pruning the whole library except the sub-folder.
                if let Some(ref t) = targeted {
                    if !sous_le_dossier(db_path, t) {
                        continue;
                    }
                }
                examinees += 1;
                if !discovered_paths.contains(db_path.as_str()) {
                    match verdict_purge(
                        db_path,
                        &scan_dirs,
                        &missing_dirs,
                        &error_dirs,
                        emptied_roots,
                        sous_arbres,
                    ) {
                        VerdictPurge::ProtegeIllisible => protected += 1,
                        VerdictPurge::HorsPerimetre => hors_perimetre += 1,
                        VerdictPurge::Supprimer => a_supprimer.push(track_id),
                    }
                }
            }
            if purge_refusee(a_supprimer.len(), examinees, purge_confirmee) {
                // Le nombre exact est publié — log ET `scan_result` — parce
                // que c'est lui qu'il faut renvoyer pour confirmer. Un refus
                // qui ne dit pas quoi faire est l'impasse qu'on corrige.
                purge_refusee_candidats = a_supprimer.len() as i64;
                tracing::error!(
                    candidats = a_supprimer.len(),
                    examinees,
                    plafond = PART_MAX_PURGE,
                    confirmee = ?purge_confirmee,
                    "post_scan_purge_refusee_trop_massive — une disparition de cette ampleur est \
                     bien plus souvent un montage absent qu'une suppression réelle. Les pistes \
                     sont CONSERVÉES. Vérifier les montages puis relancer un scan : si tout est \
                     revenu, il n'y aura plus rien à retirer. Si ces pistes ont VRAIMENT été \
                     supprimées, relancer avec `?confirm_purge={}` — relancer sans ce paramètre \
                     donnera exactement le même refus.",
                    a_supprimer.len()
                );
                protected += a_supprimer.len() as i64;
                a_supprimer.clear();
            } else if purge_trop_massive(a_supprimer.len(), examinees) {
                tracing::warn!(
                    candidats = a_supprimer.len(),
                    examinees,
                    confirmee = ?purge_confirmee,
                    "post_scan_purge_massive_confirmee — le plafond est franchi sur confirmation \
                     explicite de l'utilisateur."
                );
            }
            for track_id in a_supprimer {
                if track_repo.delete(track_id).is_ok() {
                    pruned += 1;
                }
            }
            pistes_hors_perimetre = hors_perimetre;
            pistes_protegees = protected;
            pistes_supprimees = pruned;
            if hors_perimetre > 0 {
                tracing::warn!(
                    hors_perimetre,
                    racines = ?scan_dirs,
                    "post_scan_tracks_hors_perimetre — ces pistes ne sont sous aucune racine \
                     configurée. Elles sont CONSERVÉES : un point de montage qui a changé n'est \
                     pas un fichier supprimé (#1943)."
                );
            }
            if protected > 0 {
                tracing::warn!(
                    protected,
                    missing = ?missing_dirs,
                    walk_errors = ?error_dirs,
                    emptied = ?emptied_roots,
                    "post_scan_tracks_protected_unreadable_dirs"
                );
            }
            if pruned > 0 {
                tracing::info!(pruned, "post_scan_stale_tracks_removed");
                event_bus.emit(
                    "library.scan.progress",
                    json!({ "phase": "prune", "pruned": pruned }),
                );
            }
        }

        // Backfill + album stats in a single transaction (SQLite only)
        let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
        let sqlite_write_guard = (!is_pg).then(crate::sqlite_write_gate::scan_batch);
        if !is_pg {
            tune_core::db::tx_holder::declarer("scan:post-traitement");
            if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                tracing::warn!(error = %e, "post_scan_begin_failed");
                let _ = db.execute_batch("ROLLBACK");
                let _ = db.execute_batch("BEGIN IMMEDIATE");
            }
        }
        {
            if let Err(e) = db.execute(
                "UPDATE tracks SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET track_count = \
                 (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_count_update_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET \
                 format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                 sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                 bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                 genre = COALESCE(NULLIF(albums.genre, ''), (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1)), \
                 genres = COALESCE(NULLIF(albums.genres, ''), (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1)), \
                 disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id))",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_quality_update_failed");
            }

            // Full scan only: realign each album's derived genre with its tracks.
            // The COALESCE above is fill-only (it never overwrites a value once
            // set), so an album whose genre was set once and then went stale —
            // e.g. stuck on "Folk" while its tracks are now "Folk-Punk" (Yves
            // Scordia) — never self-corrected. A forced full scan is an explicit
            // "rebuild from the files" action, so overwrite genre/genres from the
            // tracks; incremental scans keep the fill-only behaviour so values
            // persist between full scans. The EXISTS guard avoids nulling an
            // album genre when no track carries one.
            if force {
                // Pick the album genre by MAJORITY VOTE across its tracks, with a
                // deterministic tie-break, instead of an arbitrary `LIMIT 1` track.
                // A bare `LIMIT 1` (no ORDER BY) let SQLite return any row, so a
                // multi-genre album — or one track carrying a stray tag — got a
                // random genre that could differ per album and change between
                // scans (#1160/#1161). `genres` is rebuilt from the SAME chosen
                // genre so the two columns can never disagree (previously they
                // came from two independent subqueries, which is how an album
                // tagged "Alternatif & Indé" surfaced a stale "singer; Songwriter"
                // genres value from an unrelated track — #1160).
                if let Err(e) = db.execute(
                    "UPDATE albums SET \
                     genre = (SELECT t.genre FROM tracks t \
                              WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' \
                              GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre ASC LIMIT 1), \
                     genres = '[\"' || REPLACE( \
                                 (SELECT t.genre FROM tracks t \
                                  WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' \
                                  GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre ASC LIMIT 1), \
                                 '\"', '\\\"') || '\"]' \
                     WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '')",
                    &[],
                ) {
                    tracing::warn!(error = %e, "post_scan_album_genre_refresh_failed");
                }
            }
            // Merge duplicate local albums (same title, case-insensitive).
            // After a rescan, tag changes can create a second album entry for
            // tracks that already belonged to an existing album (e.g. when
            // album_artist changed). Merging moves all tracks to the album
            // with the most tracks, so the orphan cleanup below can delete the
            // now-empty duplicate. This is the definitive fix for bug #593
            // ("Doublons pochettes albums apres rescan").
            {
                let dupe_rows = db.query_many(
                    "SELECT LOWER(title), GROUP_CONCAT(id) FROM albums \
                     WHERE source = 'local' \
                     GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1",
                    &[],
                ).unwrap_or_default();
                let dupes: Vec<(String, String)> = dupe_rows.iter().map(|r| {
                    (r[0].as_string().unwrap_or_default(), r[1].as_string().unwrap_or_default())
                }).collect();
                let mut merged_albums = 0usize;
                for (_title, ids_str) in &dupes {
                    let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
                    if ids.len() < 2 {
                        continue;
                    }
                    // Keep the album with the most tracks
                    let mut best_id = ids[0];
                    let mut best_count = 0i64;
                    for &aid in &ids {
                        let cnt = db.query_one(
                            "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                            &[&aid],
                        ).ok().flatten().and_then(|r| r[0].as_i64()).unwrap_or(0);
                        if cnt > best_count {
                            best_count = cnt;
                            best_id = aid;
                        }
                    }
                    for &aid in &ids {
                        if aid != best_id {
                            db.execute(
                                "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                                &[&best_id, &aid],
                            ).ok();
                            db.execute(
                                "DELETE FROM albums WHERE id = ?",
                                &[&aid],
                            ).ok();
                            merged_albums += 1;
                        }
                    }
                }
                if merged_albums > 0 {
                    // Refresh track_count for albums that received tracks from merged duplicates
                    db.execute_batch(
                        "UPDATE albums SET track_count = \
                         (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                    ).ok();
                    tracing::info!(merged_albums, "post_scan_duplicate_albums_merged");
                }
            }
            // Remove orphan albums with 0 tracks (created by interrupted scans or tag changes)
            let orphan_albums = db.execute(
                "DELETE FROM albums WHERE id IN (\
                 SELECT a.id FROM albums a \
                 LEFT JOIN tracks t ON t.album_id = a.id \
                 WHERE t.id IS NULL AND a.source = 'local')",
                &[],
            ).unwrap_or(0);
            if orphan_albums > 0 {
                tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
            }
        }
        if !is_pg {
            tune_core::db::tx_holder::liberer();
            if let Err(e) = db.execute_batch("COMMIT") {
                tracing::warn!(error = %e, "post_scan_commit_failed");
                let _ = db.execute_batch("ROLLBACK");
            }
        }
        drop(sqlite_write_guard);

        // Clean up orphan albums (album rows with no tracks). A full rescan
        // after removing files from disk — or the duplicate-album grouping —
        // can leave album rows behind that no track references. Without this
        // they linger with their cover art and inflate the total album count
        // even though they have no tracks (Alain: emptied library + full
        // rescan still shows removed albums' covers in double/triple). The
        // incremental auto-scan already purges these; the full scan did not.
        let orphan_albums = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone())
            .delete_orphans()
            .unwrap_or(0);
        if orphan_albums > 0 {
            tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
        }

        // Une réparation d'attribution ne se fonde que sur une vue complète et
        // saine de la bibliothèque. Un scan ciblé, annulé ou privé d'une racine
        // n'a pas assez d'information pour conclure sans risque (#2458).
        let full_scan_ok = !scan_cancel_requested()
            && targeted.is_none()
            && missing_dirs.is_empty()
            && error_dirs.is_empty()
            && racines_videes.is_empty();
        if full_scan_ok {
            match tune_core::db::album_repo::AlbumRepo::with_backend(db.clone())
                .repair_empty_mbid_artist_collapses()
            {
                Ok(repaired) if repaired > 0 => {
                    tracing::warn!(repaired, "post_scan_album_artists_repaired")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "post_scan_album_artist_repair_failed"),
            }
        }

        // Clean up orphan artists left behind after tag corrections
        let orphan_artists = ArtistRepo::with_backend(db.clone()).cleanup_orphans().unwrap_or(0);
        if orphan_artists > 0 {
            tracing::info!(orphan_artists, "post_scan_orphan_artists_cleaned");
        }

        // Réconciliation des favoris : un rescan qui a recréé albums/pistes
        // sous de nouveaux rowids (racines music déplacées, library clear,
        // fusion de doublons ci-dessus) laisse des favoris orphelins → cœurs
        // éteints et filtre « Favoris » vide (bug .18, v0.9.50). On re-rattache
        // par identité (instantané titre/artiste/chemin, historique d'écoute en
        // secours) ; un favori vraiment introuvable n'est supprimé qu'après un
        // scan COMPLET et sain (pas ciblé, pas annulé, aucune racine
        // manquante/illisible) — jamais sur un scan partiel.
        {
            // `emptied_roots` fait partie de la condition depuis #1943 : il
            // manquait ici alors qu'il protégeait déjà la boucle de purge.
            // Conséquence vécue — une racine vidée par un montage absent
            // laissait `full_scan_ok = true`, et la réconciliation supprimait
            // DÉFINITIVEMENT les favoris des pistes conservées. Une purge de
            // pistes se répare par un rescan ; une perte de favoris, non.
            match tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(db.clone())
                .run(full_scan_ok)
            {
                Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
                    tracing::info!(
                        scanned = stats.scanned,
                        snapshots = stats.snapshots_backfilled,
                        relinked = stats.relinked,
                        deduplicated = stats.deduplicated,
                        deleted = stats.deleted,
                        unresolved = stats.unresolved,
                        "post_scan_favorites_reconciled"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "post_scan_favorites_reconcile_failed"),
            }
        }

        // Backfill embedded cover art for local albums still missing a cover.
        // The incremental scan only extracts covers from files it re-processed;
        // unchanged files are skipped, so an improved embedded-art extractor
        // (e.g. DSF ID3v2 covers — Thibaud) never reaches an existing library.
        // Re-extract embedded art (local only, never the network) so those
        // albums self-heal without a forced full rescan.
        let covers_backfilled =
            tune_core::library::artwork::backfill_embedded_covers(&db, &cache_dir);
        if covers_backfilled > 0 {
            tracing::info!(covers_backfilled, "post_scan_embedded_covers_backfilled");
            event_bus.emit(
                "library.scan.progress",
                json!({ "phase": "artwork", "artwork_backfilled": covers_backfilled }),
            );
        }

        // Rebuild FTS indexes so search reflects the current library state.
        // The FTS tables are contentless (content='') and rely on triggers,
        // but manual DB edits or batch operations can leave them stale.
        // A full rebuild after scan guarantees consistency.
        // FTS rebuild + WAL checkpoint are SQLite-specific operations
        if db.engine() == tune_core::db::engine::Engine::Sqlite {
            db.execute_batch(
                "INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all');\
                 INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
                 SELECT t.id, t.title, ar.name, al.title, t.genre, t.composer \
                 FROM tracks t LEFT JOIN artists ar ON t.artist_id = ar.id LEFT JOIN albums al ON t.album_id = al.id;\
                 INSERT INTO albums_fts(albums_fts) VALUES('delete-all');\
                 INSERT INTO albums_fts(rowid, title, artist_name, genre) \
                 SELECT a.id, a.title, ar.name, a.genre FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id;\
                 INSERT INTO artists_fts(artists_fts) VALUES('delete-all');\
                 INSERT INTO artists_fts(rowid, name, sort_name) SELECT id, name, sort_name FROM artists;\
                 PRAGMA wal_checkpoint(PASSIVE);",
            ).ok();
            tracing::info!("post_scan_fts_rebuilt");

        }

        // Populate cloud sync changelog with all new/updated entities
        tune_core::cloud::library_sync::populate_changelog_after_scan(&db);

        // Turn any .m3u/.m3u8/.pls files found in the scanned dirs into local
        // playlists (Bertrand). Runs after import so every track is in the DB to
        // match against; idempotent by playlist name so a re-scan never dupes.
        let pl = tune_core::library::playlist_scan::import_local_playlists(&db, &scan_dirs);
        if pl.playlists_created > 0 {
            event_bus.emit(
                "library.playlists.imported",
                json!({ "playlists": pl.playlists_created, "tracks": pl.tracks_added }),
            );
        }

        // Mirror hand-made compilation folders (tracks spanning several albums)
        // into local playlists — opt-in via scan_folder_playlists (Frédéric).
        if tune_core::library::folder_playlists::folder_playlists_enabled(&db) {
            tune_core::library::folder_playlists::sync_folder_playlists(&db);
        }

        let settings = SettingsRepo::with_backend(db.clone());
        if let Err(e) = settings.set("scan_status", "idle") {
            tracing::warn!(error = %e, "scan_status_idle_failed");
        }
        tracing::info!(
            discovered = total_discovered,
            parsed = scan_stats.total_files,
            timeout = scan_stats.metadata_timeout,
            inserted,
            updated,
            skipped,
            skipped_unchanged,
            skipped_duplicate,
            skipped_no_metadata,
            skipped_unsupported,
            db_insert_failed,
            db_update_failed,
            artwork = artwork_extracted,
            orphan_artists,
            "scan_and_import_complete"
        );

        settings
            .set(
                "scan_result",
                &json!({
                    "total_files": total_discovered,
                    "missing_dirs": missing_dirs.clone(),
                    "missing_dir_reasons": missing_dir_reasons.clone(),
                    "error_dirs": error_dirs.clone(),
                    // #1943 : ce que la purge a REFUSÉ de faire, et pourquoi.
                    // (Ces quatre clés étaient écrites DEUX fois — deux
                    // sessions ont ajouté le même bloc ; `json!` gardait
                    // silencieusement la dernière.)
                    // Ce que la purge a effectivement retiré. Le client lit
                    // cette clé pour le bandeau de fin de scan (#2146).
                    "removed": pistes_supprimees,
                    "emptied_roots": racines_videes.clone(),
                    "protected_subtrees": sous_arbres_proteges.clone(),
                    "tracks_protected": pistes_protegees,
                    "tracks_out_of_scope": pistes_hors_perimetre,
                    // Le plafond volumétrique a-t-il refusé, et sur quel
                    // nombre ? C'est ce nombre qu'il faut renvoyer dans
                    // `?confirm_purge=` pour autoriser la purge. 0 = pas de
                    // refus. Sans ça, le refus n'existe que dans les logs :
                    // l'utilisateur ne peut pas connaître le geste à faire.
                    "purge_refused": purge_refusee_candidats > 0,
                    "purge_refused_candidates": purge_refusee_candidats,
                    "parsed": scan_stats.total_files,
                    "metadata_ok": scan_stats.metadata_ok,
                    "metadata_failed": scan_stats.metadata_failed,
                    "metadata_timeout": scan_stats.metadata_timeout,
                    "inserted": inserted,
                    "updated": updated,
                    "skipped": skipped,
                    "skipped_unchanged": skipped_unchanged,
                    "skipped_duplicate": skipped_duplicate,
                    "skipped_no_metadata": skipped_no_metadata,
                    "skipped_unsupported": skipped_unsupported,
                    "db_insert_failed": db_insert_failed,
                    "db_update_failed": db_update_failed,
                    "artwork_extracted": artwork_extracted,
                    "auto_enrichment": suite_du_scan.rapport(),
                    "failed_paths": scan_stats.failed_paths,
                })
                .to_string(),
            )
            .ok();

        event_bus.emit(
            "library.scan.completed",
            json!({
                "total_files": total_discovered,
                "missing_dirs": missing_dirs.clone(),
                "missing_dir_reasons": missing_dir_reasons.clone(),
                "error_dirs": error_dirs.clone(),
                "removed": pistes_supprimees,
                "emptied_roots": racines_videes.clone(),
                "protected_subtrees": sous_arbres_proteges.clone(),
                "tracks_protected": pistes_protegees,
                "tracks_out_of_scope": pistes_hors_perimetre,
                "purge_refused": purge_refusee_candidats > 0,
                "purge_refused_candidates": purge_refusee_candidats,
                "parsed": scan_stats.total_files,
                "metadata_ok": scan_stats.metadata_ok,
                "metadata_timeout": scan_stats.metadata_timeout,
                "inserted": inserted,
                "updated": updated,
                "skipped": skipped,
                "skipped_unchanged": skipped_unchanged,
                "skipped_duplicate": skipped_duplicate,
                "skipped_no_metadata": skipped_no_metadata,
                "skipped_unsupported": skipped_unsupported,
                "db_insert_failed": db_insert_failed,
                "db_update_failed": db_update_failed,
                "artwork_extracted": artwork_extracted,
                "auto_enrichment": suite_du_scan.rapport(),
                "failed_paths": scan_stats.failed_paths,
            }),
        );

        // Launch batch artwork enrichment as a background task
        // This fetches covers from MusicBrainz Cover Art Archive for albums
        // that don't have embedded cover art.
        // Write scan report JSON for the /scan/report endpoint
        let report = serde_json::json!({
            "total_files": total_discovered,
            "missing_dirs": missing_dirs.clone(),
            "missing_dir_reasons": missing_dir_reasons.clone(),
            "error_dirs": error_dirs.clone(),
            "removed": pistes_supprimees,
            "emptied_roots": racines_videes.clone(),
            "protected_subtrees": sous_arbres_proteges.clone(),
            "tracks_protected": pistes_protegees,
            "tracks_out_of_scope": pistes_hors_perimetre,
            "purge_refused": purge_refusee_candidats > 0,
            "purge_refused_candidates": purge_refusee_candidats,
            "parsed": scan_stats.total_files,
            "metadata_ok": scan_stats.metadata_ok,
            "metadata_failed": scan_stats.metadata_failed,
            "metadata_timeout": scan_stats.metadata_timeout,
            "inserted": inserted,
            "updated": updated,
            "skipped": skipped,
            "skipped_unchanged": skipped_unchanged,
            "skipped_duplicate": skipped_duplicate,
            "skipped_no_metadata": skipped_no_metadata,
            "skipped_unsupported": skipped_unsupported,
            "db_insert_failed": db_insert_failed,
            "db_update_failed": db_update_failed,
            "artwork_extracted": artwork_extracted,
            "auto_enrichment": suite_du_scan.rapport(),
            "failed_paths": scan_stats.failed_paths,
            // Fichiers audio rencontrés mais dont Tune ne lit pas le format,
            // comptés par extension ({"mpc": 280, "cue": 132}). Presque toujours
            // vide ; quand il ne l'est pas, c'est la seule chose qui explique à
            // l'utilisateur pourquoi des albums manquent, au lieu de le laisser
            // chercher un bug de scanner (#1763).
            "skipped_unsupported_by_ext": skipped_by_ext,
            // Motif lisible associé à chaque compteur. Additif pour les clients
            // existants qui ne connaissent que `skipped_unsupported_by_ext`.
            "skipped_unsupported_reasons": skipped_reasons,
        });
        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(&report_path, json).ok();
        }

        // Auto enrichment after scan: Premium only
        if suite_du_scan.demarree() {
            let enrich_db = db.clone();
            let artist_cache_dir = cache_dir.clone();
            let artist_mbid_db = db.clone();
            let artist_enrich_db = db.clone();
            handle.spawn(async move {
                tune_core::library::artwork::batch_enrich_artwork(enrich_db, cache_dir).await;
            });

            handle.spawn(async move {
                // Resolve MusicBrainz IDs BEFORE fetching artist images. The
                // image cascade only enriches artists that already have an MBID
                // (ArtistRepo::list_without_image filters on musicbrainz_id IS
                // NOT NULL), so a library scanned from files without MB tags
                // gets ZERO artist images despite Premium — the candidate list
                // is empty. Mirror the manual enrichment route (system/enrich.rs):
                // match MBIDs first, then fetch images (Fabien: 0 image on 1183
                // artists, none carrying an MBID).
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                tune_core::metadata::matcher::batch_match_artist_mbids(artist_mbid_db).await;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                tune_core::library::artwork::batch_enrich_artist_artwork(artist_enrich_db, artist_cache_dir).await;
            });
        } else {
            tracing::info!(
                enrich_on_scan,
                licensed = enrichissement_sous_licence,
                motif = suite_du_scan.motif().unwrap_or("none"),
                "auto_enrichment_after_scan_skipped (needs Premium + enrich_on_scan)"
            );
        }
        }).await;
        if let Err(e) = result {
            tracing::error!("scan_task_panicked — {:?}", e);
            if let Err(e2) = SettingsRepo::with_backend(db_for_panic).set("scan_status", "idle") {
                tracing::warn!(error = %e2, "scan_status_panic_reset_failed");
            }
        }
    });
    true
}

pub(super) async fn scan_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let scanning = status == "scanning";
    let result = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Json(json!({
        "status": status,
        "scanning": scanning,
        "result": result,
    }))
}

pub(super) async fn scan_cancel(State(state): State<AppState>) -> impl IntoResponse {
    // Signal the running batch loop to stop processing further batches. The scan
    // task then drains its remaining (no-op) batches and runs its normal
    // completion path, which resets scan_status to "idle" and emits
    // library.scan.completed. Without this flag the endpoint only flipped the
    // status string while the scan kept inserting for minutes (bug #1129).
    if SCAN_GATE.request_cancel() {
        tracing::info!("scan_cancel_requested");
    } else {
        tracing::info!("scan_cancel_ignored_no_active_scan");
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "idle") {
        tracing::warn!(error = %e, "scan_cancel_status_reset_failed");
    }
    // Clear the client's "scanning" banner immediately. The batch loop's own
    // completion event only fires if the scan is *in* that loop — but if it is
    // stuck earlier (walker enumerating a slow/inaccessible NAS path, macOS
    // folder-permission stall) or has already ended, SCAN_CANCEL is a no-op and
    // no completion event is ever emitted, so "Stop scan" does nothing visible
    // (#1129). Emitting here guarantees the banner drops on Stop. A duplicate
    // event from the draining loop is harmless (the UI just clears twice).
    state
        .event_bus
        .emit("library.scan.completed", json!({ "cancelled": true }));
    StatusCode::NO_CONTENT
}

/// Clé de réglage portant la date (locale, ISO) de la dernière occurrence
/// HONORÉE du scan programmé.
///
/// Une date, pas un horodatage : l'unité de l'ordonnanceur est le jour. Deux
/// dates se comparent sans rien savoir du fuseau, de l'heure d'été, ni de la
/// durée pendant laquelle le processus a été absent.
const CLE_DERNIERE_OCCURRENCE: &str = "scan_schedule_last_run";

/// Motifs d'inscription sans scan. Ce sont les événements journalisés tels
/// quels — un test les cite, ils ne doivent pas diverger silencieusement.
const MOTIF_AMORCE: &str = "scheduled_scan_baseline";
const MOTIF_SCAN_DE_DEMARRAGE: &str = "scheduled_scan_covered_by_startup_scan";

/// Ce que l'ordonnanceur doit faire à ce réveil.
#[derive(Debug, PartialEq, Eq)]
enum Ordre {
    /// Désactivé, horaire illisible, ou occurrence déjà honorée.
    Rien,
    /// Inscrire l'occurrence SANS scanner ; le motif part au journal.
    Noter(time::Date, &'static str),
    /// Inscrire l'occurrence ET lancer le scan.
    Scanner(time::Date),
}

/// La dernière occurrence dont l'heure est passée : celle d'aujourd'hui si
/// l'heure programmée est déjà atteinte, celle d'hier sinon.
///
/// C'est le cœur du correctif de #2469. L'ancienne boucle exigeait que
/// l'horloge affiche EXACTEMENT la minute programmée :
///
/// ```text
/// if now.hour() != sh || now.minute() != sm { continue; }
/// ```
///
/// Une machine éteinte à 21 h 00 — ou simplement endormie, cas que l'issue
/// signalait comme non examiné — perdait le rendez-vous sans trace. La question
/// posée ici n'est plus « est-il 21 h 00 ? » mais « l'occurrence de 21 h 00 la
/// plus récente a-t-elle eu lieu ? », à laquelle un démarrage tardif et un
/// réveil de veille répondent aussi bien qu'un tour de boucle à l'heure juste.
fn occurrence_due(now: time::OffsetDateTime, sh: u8, sm: u8) -> Option<time::Date> {
    if (now.hour(), now.minute()) >= (sh, sm) {
        Some(now.date())
    } else {
        now.date().previous_day()
    }
}

/// Toute la décision de l'ordonnanceur, séparée de l'état ambiant pour être
/// jugée sur une horloge injectée.
///
/// Le retard ne s'accumule PAS : quel que soit le nombre de jours manqués, une
/// seule occurrence est due — la plus récente. Six jours de vacances coûtent
/// donc un scan, pas six. C'est aussi la raison pour laquelle aucune fenêtre de
/// rattrapage n'est nécessaire : rattraper une occurrence vieille d'une semaine
/// coûte exactement le prix d'une occurrence vieille d'un jour, et c'est
/// précisément au retour de vacances que la bibliothèque est la plus périmée.
///
/// `scan_de_demarrage` vaut vrai UNIQUEMENT au premier tour, et seulement si un
/// scan de démarrage a été lancé (`TUNE_AUTO_SCAN`). Ce scan-là fait déjà le
/// travail de l'occurrence : elle est donc inscrite sans en lancer un second.
/// C'est ce qui interdit la cascade au démarrage sans dépendre d'une course —
/// le fait est connu au boot, pas mesuré 30 secondes plus tard.
fn decider(
    active: bool,
    horaire: &str,
    now: time::OffsetDateTime,
    dernier: Option<&str>,
    scan_de_demarrage: bool,
) -> Ordre {
    if !active {
        return Ordre::Rien;
    }
    let Some((sh, sm)) = parse_hhmm(horaire) else {
        return Ordre::Rien;
    };
    let Some(due) = occurrence_due(now, sh, sm) else {
        return Ordre::Rien;
    };
    match dernier.and_then(parse_date_iso) {
        // Aucune trace exploitable : ni au premier démarrage, ni à la première
        // montée de version qui porte ce code, ni si la valeur est illisible.
        // Une absence n'est pas une occurrence manquée — elle ne prouve rien.
        // On l'inscrit comme point de départ, ce qui interdit le scan-surprise
        // à la mise à jour ; l'occurrence SUIVANTE, elle, est décidable.
        None => Ordre::Noter(due, MOTIF_AMORCE),
        Some(honoree) if honoree < due => {
            if scan_de_demarrage {
                Ordre::Noter(due, MOTIF_SCAN_DE_DEMARRAGE)
            } else {
                Ordre::Scanner(due)
            }
        }
        Some(_) => Ordre::Rien,
    }
}

/// L'état que la boucle traîne d'un tour à l'autre. Un seul champ — mais c'est
/// celui dont l'oubli remettrait l'option en panne : un drapeau de démarrage
/// jamais consommé couvrirait TOUTES les occurrences à venir, et le scan
/// programmé redeviendrait muet sur les machines en `TUNE_AUTO_SCAN`. Il vit
/// donc ici, hors de la boucle `tokio`, pour qu'un test puisse enchaîner deux
/// tours et le constater.
struct Ordonnanceur {
    /// Vrai tant que le premier tour n'a pas eu lieu ET qu'un scan de démarrage
    /// a été lancé par ce processus.
    premier_tour: bool,
}

impl Ordonnanceur {
    fn new(scan_de_demarrage: bool) -> Self {
        Self {
            premier_tour: scan_de_demarrage,
        }
    }

    /// Un tour de boucle réduit à sa décision : ni horloge ambiante, ni base.
    /// Le drapeau est consommé à CHAQUE tour, y compris ceux qui ne décident
    /// rien — sinon il survivrait jusqu'à la première occurrence due, des
    /// heures après que le scan de démarrage soit terminé.
    fn tour(
        &mut self,
        active: bool,
        horaire: &str,
        now: time::OffsetDateTime,
        dernier: Option<&str>,
    ) -> Ordre {
        let couvert_par_le_demarrage = std::mem::take(&mut self.premier_tour);
        decider(active, horaire, now, dernier, couvert_par_le_demarrage)
    }
}

fn date_iso(jour: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        jour.year(),
        jour.month() as u8,
        jour.day()
    )
}

fn parse_date_iso(s: &str) -> Option<time::Date> {
    let mut parts = s.trim().split('-');
    let y: i32 = parts.next()?.trim().parse().ok()?;
    let m: u8 = parts.next()?.trim().parse().ok()?;
    let d: u8 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    time::Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

fn noter_occurrence(settings: &SettingsRepo, jour: time::Date) {
    if let Err(e) = settings.set(CLE_DERNIERE_OCCURRENCE, &date_iso(jour)) {
        // Une écriture de réglage qui échoue laisse l'occurrence due : la
        // boucle repassera. C'est le bon sens du risque — mieux vaut réessayer
        // sur une base cassée que perdre définitivement le rendez-vous.
        tracing::warn!(error = %e, "scheduled_scan_last_run_persist_failed");
    }
}

/// Boucle du scan programmé. Le point d'entrée `/scan/schedule` enregistre
/// `scan_schedule_enabled` / `scan_schedule_time` ("HH:MM") depuis longtemps ;
/// **plus rien ne les relisait** : cette fonction n'était appelée de nulle part
/// depuis la PR #1230, la bascule des clients était donc muette. Elle est
/// rebranchée dans `spawn_background_tasks`, et un test de non-régression
/// vérifie que l'appel y figure toujours.
///
/// Réveil toutes les 30 s. À chaque tour, l'occurrence la plus récente est-elle
/// honorée ? Sinon elle part — que le tour tombe à l'heure juste, après un
/// démarrage tardif ou après un réveil de veille : c'est le MÊME chemin, il n'y
/// a pas de « rattrapage » séparé à maintenir.
///
/// `scan_de_demarrage` dit si un scan de démarrage a été lancé par ce
/// processus ; il ne vaut que pour le premier tour de boucle.
pub(crate) fn spawn_scan_scheduler(state: AppState, scan_de_demarrage: bool) {
    tokio::spawn(async move {
        let mut ordonnanceur = Ordonnanceur::new(scan_de_demarrage);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let active = settings
                .get("scan_schedule_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);
            let horaire = settings
                .get("scan_schedule_time")
                .ok()
                .flatten()
                .unwrap_or_else(|| "03:00".into());
            let dernier = settings.get(CLE_DERNIERE_OCCURRENCE).ok().flatten();
            // Heure LOCALE : l'utilisateur règle SON 21 h, et les horodatages du
            // journal sont déjà locaux (run.rs). Repli UTC si l'offset local est
            // indisponible (certaines configurations Linux durcies).
            let now = time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            let due = match ordonnanceur.tour(active, &horaire, now, dernier.as_deref()) {
                Ordre::Rien => continue,
                Ordre::Noter(jour, motif) => {
                    noter_occurrence(&settings, jour);
                    tracing::info!(occurrence = %date_iso(jour), motif, "scheduled_scan_noted");
                    continue;
                }
                Ordre::Scanner(jour) => jour,
            };
            // Même condition que le balayage acoustique, par la MÊME fonction :
            // une passe de fond ne dispute pas le disque au lecteur (#1310,
            // #1515). Rien n'est inscrit ici — l'occurrence reste due et
            // repassera dans 30 s, jusqu'à ce que la musique s'arrête. C'est ce
            // que demandait Thierry : « dans les mêmes conditions que le scan
            // CLAP ».
            if let Some(zone) = tune_core::audio::replaygain::playing_zone_name(&state.backend) {
                tracing::info!(
                    zone = %zone,
                    occurrence = %date_iso(due),
                    "scheduled_scan_yield_to_playback"
                );
                continue;
            }
            // Inscrite AVANT le départ : une occurrence n'est due qu'une fois.
            // Sinon un scan qui se termine mal relancerait un scan complet
            // toutes les 30 secondes.
            noter_occurrence(&settings, due);
            if spawn_library_scan(state.clone(), false, None).await {
                tracing::info!(
                    time = %horaire,
                    occurrence = %date_iso(due),
                    "scheduled_scan_triggered"
                );
            } else {
                // La porte unique refuse : un scan tourne DÉJÀ (manuel, ajout de
                // dossier, ou scan de démarrage encore en cours). Il fait le
                // travail de l'occurrence, qui est donc honorée — jamais un
                // second scan, jamais une nouvelle tentative dans 30 s.
                tracing::info!(
                    occurrence = %date_iso(due),
                    "scheduled_scan_covered_by_running_scan"
                );
            }
        }
    });
}

fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u8 = h.trim().parse().ok()?;
    let m: u8 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

pub(super) async fn scan_schedule(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let time = settings
        .get("scan_schedule_time")
        .ok()
        .flatten()
        .unwrap_or_else(|| "03:00".into());
    let enabled = settings
        .get("scan_schedule_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    // La moitié visible du correctif #2469 : sans la date du dernier passage
    // réel, l'utilisateur ne peut pas savoir si son scan programmé a eu lieu.
    // `null` = jamais observé (option fraîchement activée, ou base neuve).
    let last_run = settings.get(CLE_DERNIERE_OCCURRENCE).ok().flatten();
    Json(json!({ "enabled": enabled, "time": time, "last_run": last_run }))
}

#[derive(Deserialize)]
pub(super) struct ScanScheduleReq {
    enabled: bool,
    time: Option<String>,
}

pub(super) async fn set_scan_schedule(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ScanScheduleReq>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "scan_schedule_enabled",
            if body.enabled { "true" } else { "false" },
        )
        .ok();
    if let Some(ref t) = body.time {
        settings.set("scan_schedule_time", t).ok();
    }
    Json(json!({ "enabled": body.enabled, "time": body.time }))
}

pub(super) async fn library_clear(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Json<Value> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    match repo.delete_all() {
        Ok(count) => {
            tracing::info!(tracks_deleted = count, "library_cleared");
            Json(json!({"ok": true, "deleted": count}))
        }
        Err(e) => {
            tracing::warn!(error = %e, "library_clear_failed");
            Json(json!({"ok": false, "error": e.to_string()}))
        }
    }
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

/// Build a JSON array string for the `genres` column from parsed metadata.
///
/// If the structured `genres` vec is non-empty, serialize it as JSON.
/// Otherwise, fall back to the primary `genre` string and wrap it as a
/// single-element array so the column is never NULL when genre data exists.
pub(super) async fn scan_report() -> impl IntoResponse {
    let report_path = std::env::var("TUNE_DB_PATH")
        .unwrap_or_else(|_| "tune.db".into())
        .replace(".db", "-scan-report.json");
    match std::fs::read_to_string(&report_path) {
        Ok(json) => match serde_json::from_str::<Value>(&json) {
            Ok(v) => Json(v).into_response(),
            Err(_) => Json(json!({"error": "invalid report file"})).into_response(),
        },
        Err(_) => Json(json!({"error": "no scan report available yet"})).into_response(),
    }
}

/// GET /system/artist-split-preview — READ-ONLY dry-run of multi-artist credit
/// splitting (Phase 0 telemetry). Reports how many `artists` rows would split,
/// broken down by separator, plus example splits — WITHOUT changing anything.
/// Used to size the change and tune the allowlist before touching scan/DB.
pub(super) async fn artist_split_preview(State(state): State<AppState>) -> Json<Value> {
    use tune_core::metadata::artist_split::analyze_artist_credit;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let extra: Vec<String> = settings
        .get("artist_split_allowlist")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artists = artist_repo.list_all_id_name_mbid().unwrap_or_default();

    let total = artists.len();
    let mut would_split = 0usize;
    let mut would_split_no_mbid = 0usize;
    let mut by_sep: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    let mut examples: Vec<Value> = Vec::new();

    for (_id, name, mbid) in &artists {
        let a = analyze_artist_credit(name, &extra, true);
        if a.would_split() {
            would_split += 1;
            if mbid.is_empty() {
                would_split_no_mbid += 1;
            }
            for s in &a.separators {
                *by_sep.entry(s.as_str()).or_insert(0) += 1;
            }
            if examples.len() < 60 {
                examples.push(json!({
                    "original": a.original,
                    "tokens": a.tokens,
                    "separators": a.separators.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "has_mbid": !mbid.is_empty(),
                }));
            }
        }
    }

    Json(json!({
        "total_artists": total,
        "would_split": would_split,
        "would_split_no_mbid": would_split_no_mbid,
        "by_separator": by_sep,
        "extra_allowlist_size": extra.len(),
        "examples": examples,
        "note": "dry-run, read-only — no data changed",
    }))
}

#[cfg(test)]
mod roots_gone_empty_tests {
    use super::{
        VerdictPurge, purge_refusee, purge_trop_massive, roots_gone_empty, sous_arbres_vides,
        sous_le_dossier, verdict_purge,
    };
    use std::collections::HashSet;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    // Le cas de Dominique COMET (#1652) : NAS OpenMediaVault en SMB, le
    // service démarre avant que le partage soit monté. Le point de montage
    // existe et se lit — il est simplement vide.
    const NAS: &str = "/mnt/nas/musique";

    #[test]
    fn un_partage_non_monte_protege_toute_la_bibliotheque() {
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/mnt/nas/musique/Bach/02.flac",
            "/mnt/nas/musique/Mahler/01.flac",
        ];
        // Le scan ne découvre RIEN sous cette racine.
        let decouverts = set(&[]);
        assert_eq!(
            roots_gone_empty(&[NAS.to_string()], &existants, &decouverts),
            vec![NAS.to_string()],
            "zero fichier la ou il y en avait des milliers doit proteger, pas supprimer"
        );
    }

    #[test]
    fn une_racine_qui_repond_normalement_n_est_pas_protegee() {
        // Sans ça, plus aucune piste réellement supprimée ne serait nettoyée.
        let existants = ["/mnt/nas/musique/Bach/01.flac"];
        let decouverts = set(&["/mnt/nas/musique/Bach/01.flac"]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    #[test]
    fn une_seule_piste_retrouvee_suffit_a_lever_la_protection() {
        // La racine répond : les autres absences sont de vraies suppressions.
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/mnt/nas/musique/Bach/02.flac",
        ];
        let decouverts = set(&["/mnt/nas/musique/Bach/01.flac"]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    // ── Purge de fin de scan : le sort d'une piste absente (#1943) ────────

    /// Le cas de Yacine, 17/08 : bibliothèque sur `/mnt/music2`, et 21 277
    /// lignes en base portant un ANCIEN point de montage qui n'est plus
    /// configuré. Elles ont été supprimées sans qu'aucune protection ne
    /// s'applique — elles n'étaient ni manquantes, ni en erreur, ni sous une
    /// racine vidée, puisque personne n'était allé voir.
    #[test]
    fn une_piste_hors_de_toute_racine_configuree_est_conservee() {
        let v = verdict_purge(
            "/mnt/music/Bach/01.flac",    // ancien montage
            &["/mnt/music2".to_string()], // seule racine configurée aujourd'hui
            &[],
            &[],
            &[],
            &[],
        );
        assert_eq!(
            v,
            VerdictPurge::HorsPerimetre,
            "un point de montage qui a changé n'est pas un fichier supprimé"
        );
    }

    #[test]
    fn une_piste_sous_une_racine_saine_et_absente_du_disque_est_supprimee() {
        // Sans ça, plus rien ne serait jamais nettoyé.
        let v = verdict_purge(
            "/mnt/music2/Bach/01.flac",
            &["/mnt/music2".to_string()],
            &[],
            &[],
            &[],
            &[],
        );
        assert_eq!(v, VerdictPurge::Supprimer);
    }

    #[test]
    fn une_racine_videe_protege_ce_qu_elle_contenait() {
        let v = verdict_purge(
            "/mnt/music2/Bach/01.flac",
            &["/mnt/music2".to_string()],
            &[],
            &[],
            &["/mnt/music2".to_string()],
            &[],
        );
        assert_eq!(v, VerdictPurge::ProtegeIllisible);
    }

    #[test]
    fn sans_aucune_racine_configuree_on_ne_supprime_rien() {
        // Une liste vide ne dit pas « tout est hors périmètre », elle dit
        // qu'on ne sait rien. Le pire moment pour purger.
        let v = verdict_purge("/mnt/music2/Bach/01.flac", &[], &[], &[], &[], &[]);
        assert_eq!(v, VerdictPurge::HorsPerimetre);
    }

    #[test]
    fn un_dossier_voisin_ne_beneficie_pas_du_prefixe() {
        // `/mnt/music2` est un préfixe de `/mnt/music22` : avec un simple
        // `starts_with`, une piste de `music22` passerait pour être sous
        // `music2` — protection appliquée au mauvais endroit, ou pas appliquée
        // là où on la croit.
        assert!(sous_le_dossier("/mnt/music2/a.flac", "/mnt/music2"));
        assert!(!sous_le_dossier("/mnt/music22/a.flac", "/mnt/music2"));
        assert!(sous_le_dossier("/mnt/music2", "/mnt/music2"));
        // Une barre finale sur la racine ne doit rien changer.
        assert!(sous_le_dossier("/mnt/music2/a.flac", "/mnt/music2/"));
    }

    /// La porte de sortie du plafond est un drapeau DÉDIÉ, jamais `force`.
    ///
    /// `force` est le bouton « Scan complet » : on le clique pour relire ses
    /// fichiers, et c'est exactement ce que clique quelqu'un dont le NAS était
    /// hors ligne, pour réparer sa bibliothèque. Y accrocher l'autorisation de
    /// supprimer en masse recréerait #1943 par la porte de service.
    ///
    /// Ce test fige la distinction : si quelqu'un fusionne un jour les deux
    /// drapeaux par souci de simplicité, il échoue.
    #[test]
    fn le_plafond_ne_cede_qu_a_une_confirmation_explicite() {
        // Les chiffres de Yacine : 21 277 sur 70 346, soit 30 %.
        let (candidats, examinees) = (21_277usize, 70_346usize);
        assert!(purge_trop_massive(candidats, examinees));
        // On interroge `purge_refusee` — la fonction de PRODUCTION. Rejouer la
        // règle dans une closure locale ne prouverait que la closure.
        assert!(
            purge_refusee(candidats, examinees, None),
            "sans confirmation, la purge doit être refusée"
        );
        assert!(
            !purge_refusee(candidats, examinees, Some(candidats as u64)),
            "avec confirmation explicite du nombre annoncé, elle doit passer"
        );
    }

    /// Sous Windows, `tracks.file_path` contient des ANTISLASHS. Avec un `/`
    /// codé en dur, TOUS ces cas échouaient — aucune piste n'était vue sous sa
    /// racine, et la purge cessait silencieusement de fonctionner.
    ///
    /// Ces tests tournent sur n'importe quel hôte : `sous_le_dossier` compare
    /// des chaînes, pas des chemins du système de fichiers.
    /// Les TROIS symptomes observes sur .42 (Windows, `D:\\data\\music`), qui
    /// n'avaient aucun rapport apparent entre eux :
    ///
    ///   - « scan_targeted_path_outside_music_dirs » sur un chemin pourtant dedans
    ///   - « scan_root_no_audio_files » sur une racine pleine
    ///   - tout import refuse comme « outside the configured music directories »
    ///
    /// Une seule cause : `format!("{root}/")`, avec `/` code en dur, alors que
    /// les reglages et la base portent des ANTISLASHS. Cinq sites au total
    /// portaient cette erreur (#2016 en a corrige deux).
    #[test]
    fn un_chemin_windows_est_reconnu_sous_sa_racine_configuree() {
        let racine = r"D:\data\music";
        // Scan cible : le chemin signale par .42.
        assert!(sous_le_dossier(
            r"D:\data\music\Jacobs, Lisa\2016 - L'Arte del Violino",
            racine
        ));
        // Racine elle-meme.
        assert!(sous_le_dossier(racine, racine));
        // Un volume voisin ne doit rien capter.
        assert!(!sous_le_dossier(r"E:\data\music\x.flac", racine));
        // Et le piege de prefixe tient aussi en antislash.
        assert!(!sous_le_dossier(r"D:\data\music2\x.flac", racine));
    }

    /// L'ancienne formule, pour prouver que le defaut etait REEL et pas suppose.
    /// Ce test echouerait si quelqu'un revenait a `format!("{root}/")`.
    #[test]
    fn l_ancienne_comparaison_echouait_bien_sous_windows() {
        let racine = r"D:\data\music";
        let chemin = r"D:\data\music\album\piste.flac";
        let ancienne = chemin == racine || chemin.starts_with(&format!("{racine}/"));
        assert!(
            !ancienne,
            "l'ancienne comparaison rendait faux — c'est tout le defaut"
        );
        assert!(
            sous_le_dossier(chemin, racine),
            "la nouvelle doit rendre vrai"
        );
    }

    #[test]
    fn les_chemins_windows_sont_reconnus_sous_leur_racine() {
        assert!(sous_le_dossier(r"G:\Blues 2\track.flac", r"G:\Blues 2"));
        assert!(sous_le_dossier(
            r"G:\Blues 2\sous\dossier\t.flac",
            r"G:\Blues 2"
        ));
        assert!(sous_le_dossier(r"G:\Blues 2", r"G:\Blues 2"));
        // Le piège du préfixe vaut aussi avec des antislashs.
        assert!(!sous_le_dossier(r"G:\Blues 22\track.flac", r"G:\Blues 2"));
        assert!(sous_le_dossier(r"G:\Blues 2\track.flac", r"G:\Blues 2\"));
        // Racine de lecteur, et le lecteur voisin qui ne doit rien capter.
        assert!(sous_le_dossier(r"C:\musique.flac", r"C:\"));
        assert!(!sous_le_dossier(r"D:\musique.flac", r"C:\"));
        // Les chemins POSIX ne régressent pas.
        assert!(sous_le_dossier("/mnt/music2/a.flac", "/mnt/music2"));
        assert!(!sous_le_dossier("/mnt/music22/a.flac", "/mnt/music2"));
    }

    /// Le verdict complet, pas seulement le prédicat : c'est lui qui décidait
    /// du sort d'une bibliothèque entière.
    #[test]
    fn une_piste_windows_sous_sa_racine_n_est_pas_hors_perimetre() {
        let racines = vec![r"G:\Blues 2".to_string()];
        assert_eq!(
            verdict_purge(r"G:\Blues 2\track.flac", &racines, &[], &[], &[], &[]),
            VerdictPurge::Supprimer
        );
        assert_eq!(
            verdict_purge(r"H:\Autre\track.flac", &racines, &[], &[], &[], &[]),
            VerdictPurge::HorsPerimetre
        );
        assert_eq!(
            verdict_purge(
                r"G:\Blues 2\track.flac",
                &racines,
                &[r"G:\Blues 2".to_string()],
                &[],
                &[],
                &[]
            ),
            VerdictPurge::ProtegeIllisible
        );
    }

    /// LE test de ce correctif : le garde-fou anti-effacement de #1652 doit se
    /// déclencher sous Windows comme ailleurs.
    ///
    /// Avec un `/` codé en dur, `had` était toujours faux sur des chemins en
    /// antislash — donc `roots_gone_empty` rendait TOUJOURS une liste vide, et
    /// un partage réseau non monté (point de montage lisible et vide) faisait
    /// effacer la bibliothèque. C'est le scénario exact de #1652.
    #[test]
    fn une_racine_windows_videe_est_bien_detectee() {
        use std::collections::HashSet;
        let racines = vec![r"G:\Musique".to_string()];
        let avait = vec![r"G:\Musique\a.flac", r"G:\Musique\b.flac"];
        // Le partage n'est pas monté : rien de découvert.
        let rien: HashSet<String> = HashSet::new();
        assert_eq!(
            roots_gone_empty(&racines, &avait, &rien),
            vec![r"G:\Musique".to_string()],
            "une racine Windows vidée doit être signalée"
        );
        // Le partage est monté : la racine n'est pas vidée.
        let trouve: HashSet<String> = [r"G:\Musique\a.flac".to_string()].into();
        assert!(roots_gone_empty(&racines, &avait, &trouve).is_empty());
        // Une racine qui n'avait rien n'a rien à perdre.
        assert!(roots_gone_empty(&racines, &[], &rien).is_empty());
    }

    // ── Cohabitation : garde-fous de chemin ET plafond volumétrique ───────
    //
    // Deux correctifs de #1943 sont arrivés par deux chemins différents :
    // le séparateur Windows qui neutralisait les gardes-fous par dossier
    // (#1652/#1943), et le plafond de 20 % qui refusait sans jamais offrir de
    // sortie. Les tester séparément ne dit RIEN de ce qui se passe quand ils
    // s'appliquent au même scan. Ce bloc rejoue la décision complète.

    #[derive(Debug, PartialEq, Eq)]
    struct Purge {
        supprimees: usize,
        protegees: usize,
        hors_perimetre: usize,
        /// Le plafond a refusé et rendu la main à l'utilisateur.
        refusee: bool,
    }

    /// Rejoue la décision de purge de bout en bout, dans l'ORDRE de
    /// production (`spawn_library_scan_confirmee`) : racines vidées →
    /// sous-arbres vidés → verdict par piste → plafond volumétrique.
    fn purge_simulee(
        racines: &[&str],
        en_base: &[String],
        decouvertes: &[String],
        confirmee: Option<u64>,
    ) -> Purge {
        use std::collections::HashSet;
        let racines: Vec<String> = racines.iter().map(|s| s.to_string()).collect();
        let refs: Vec<&str> = en_base.iter().map(String::as_str).collect();
        let trouvees: HashSet<String> = decouvertes.iter().cloned().collect();

        let racines_videes = roots_gone_empty(&racines, &refs, &trouvees);
        let sous_arbres = sous_arbres_vides(&refs, &trouvees);

        let (mut candidats, mut protegees, mut hors_perimetre) = (0usize, 0usize, 0usize);
        let examinees = en_base.len();
        for p in &refs {
            if trouvees.contains(*p) {
                continue;
            }
            match verdict_purge(p, &racines, &[], &[], &racines_videes, &sous_arbres) {
                VerdictPurge::ProtegeIllisible => protegees += 1,
                VerdictPurge::HorsPerimetre => hors_perimetre += 1,
                VerdictPurge::Supprimer => candidats += 1,
            }
        }
        if purge_refusee(candidats, examinees, confirmee) {
            return Purge {
                supprimees: 0,
                protegees: protegees + candidats,
                hors_perimetre,
                refusee: true,
            };
        }
        Purge {
            supprimees: candidats,
            protegees,
            hors_perimetre,
            refusee: false,
        }
    }

    fn pistes(prefixe: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefixe}{i}.flac")).collect()
    }

    /// LE test de cohabitation, côté garde-fou de chemin.
    ///
    /// NAS Windows hors ligne : la racine répond, vide. `roots_gone_empty`
    /// doit la voir (il ne la voyait pas avant `1ecdeb5a` : le `/` codé en
    /// dur), et cette protection doit tenir **même quand l'utilisateur
    /// confirme une purge de masse**. La confirmation ne lève QUE le plafond
    /// volumétrique ; elle n'a jamais le droit de passer par-dessus un
    /// montage absent — c'est précisément ce qui a coûté 21 277 pistes.
    #[test]
    fn windows_nas_hors_ligne_la_confirmation_ne_perce_pas_le_garde_fou() {
        let en_base = pistes(r"G:\Musique\album\t", 500);
        for confirmee in [None, Some(0), Some(500), Some(u64::MAX)] {
            let p = purge_simulee(&[r"G:\Musique"], &en_base, &[], confirmee);
            assert_eq!(
                p,
                Purge {
                    supprimees: 0,
                    protegees: 500,
                    hors_perimetre: 0,
                    refusee: false,
                },
                "racine Windows vidée, confirmee={confirmee:?} : les pistes doivent être \
                 CONSERVÉES par le garde-fou de racine, pas par le plafond"
            );
        }
    }

    /// Même exigence pour le montage IMBRIQUÉ qui tombe : la racine répond
    /// encore, seul le sous-arbre a disparu. `sous_arbres_vides` remontait
    /// les parents avec `rfind('/')` seul — donc ne trouvait aucun parent
    /// dans `G:\...` et rendait TOUJOURS une liste vide sous Windows.
    #[test]
    fn windows_montage_imbrique_tombe_la_confirmation_ne_perce_pas_non_plus() {
        let mut en_base = pistes(r"G:\Musique\Jazz\t", 150);
        en_base.extend(pistes(r"G:\Musique\Rock\t", 400));
        let decouvertes = pistes(r"G:\Musique\Rock\t", 400);

        for confirmee in [None, Some(u64::MAX)] {
            let p = purge_simulee(&[r"G:\Musique"], &en_base, &decouvertes, confirmee);
            assert_eq!(
                p,
                Purge {
                    supprimees: 0,
                    protegees: 150,
                    hors_perimetre: 0,
                    refusee: false,
                },
                "sous-arbre Windows vidé, confirmee={confirmee:?} : CONSERVÉES"
            );
        }
    }

    /// LE test de cohabitation, côté plafond : il ne doit plus être une
    /// IMPASSE.
    ///
    /// Ici les montages vont bien — la racine répond, aucun sous-arbre n'est
    /// tombé — et l'utilisateur a réellement supprimé 30 % de ses fichiers.
    /// Sans confirmation le plafond refuse (bon réflexe). Mais relancer sans
    /// rien changer donnait le MÊME refus, indéfiniment, alors que le message
    /// promettait que relancer suffirait. Avec la confirmation explicite du
    /// nombre annoncé, la purge aboutit enfin.
    #[test]
    fn le_plafond_n_est_plus_une_impasse() {
        let en_base = pistes(r"G:\Musique\album\t", 1000);
        let decouvertes = pistes(r"G:\Musique\album\t", 700);

        // Relancer, encore et encore, sans confirmer : toujours le même refus.
        for _ in 0..3 {
            let p = purge_simulee(&[r"G:\Musique"], &en_base, &decouvertes, None);
            assert!(p.refusee, "sans confirmation, le plafond doit refuser");
            assert_eq!(p.supprimees, 0);
            assert_eq!(p.protegees, 300, "les 300 pistes restent en base");
        }

        // La sortie : confirmer le nombre que le refus a annoncé.
        let p = purge_simulee(&[r"G:\Musique"], &en_base, &decouvertes, Some(300));
        assert!(
            !p.refusee,
            "confirmation explicite du nombre annoncé : la purge doit aboutir"
        );
        assert_eq!(p.supprimees, 300);
    }

    /// La confirmation est bornée par l'ampleur constatée : elle n'est pas un
    /// blanc-seing rejouable. Une URL qui confirmait 300 pistes ne doit pas
    /// autoriser, plus tard, l'effacement de toute la bibliothèque.
    #[test]
    fn une_confirmation_perimee_n_autorise_pas_une_purge_plus_grande() {
        let en_base = pistes(r"G:\Musique\album\t", 1000);
        // Le NAS retombe : cette fois 900 pistes manquent, pas 300.
        let decouvertes = pistes(r"G:\Musique\album\t", 100);
        let p = purge_simulee(&[r"G:\Musique"], &en_base, &decouvertes, Some(300));
        assert!(
            p.refusee,
            "une confirmation de 300 ne couvre pas une purge de 900"
        );
        assert_eq!(p.supprimees, 0);
    }

    /// …et pas non plus une NOUVELLE impasse : entre le refus et la
    /// confirmation, le compte peut avoir bougé à la baisse (une piste
    /// réapparaît). Exiger l'égalité stricte rejouerait le défaut corrigé.
    #[test]
    fn la_confirmation_tolere_une_derive_a_la_baisse() {
        let en_base = pistes(r"G:\Musique\album\t", 1000);
        let decouvertes = pistes(r"G:\Musique\album\t", 701); // 299 manquantes
        let p = purge_simulee(&[r"G:\Musique"], &en_base, &decouvertes, Some(300));
        assert!(!p.refusee, "299 ≤ 300 : la confirmation reste valable");
        assert_eq!(p.supprimees, 299);
    }

    /// La porte de sortie du plafond est un paramètre DÉDIÉ, jamais `force`.
    ///
    /// `force`/`full` est le bouton « Scan complet » : on le clique pour
    /// relire ses fichiers, et c'est exactement ce que clique quelqu'un dont
    /// le NAS était hors ligne, pour réparer sa bibliothèque. Y accrocher
    /// l'autorisation de supprimer en masse recréerait #1943 par la porte de
    /// service. Ce test fige la distinction : si quelqu'un fusionne un jour
    /// les deux drapeaux par souci de simplicité, il échoue.
    #[test]
    fn scan_complet_ne_vaut_pas_confirmation_de_purge() {
        let q: super::ScanQuery =
            serde_json::from_value(serde_json::json!({ "force": true, "full": true })).unwrap();
        assert_eq!(
            q.confirm_purge, None,
            "« Scan complet » ne doit JAMAIS confirmer une purge"
        );
        // Et sans confirmation, le plafond refuse — quel que soit `force`.
        assert!(purge_refusee(300, 1000, q.confirm_purge));
    }

    /// Sous le plafond, la confirmation ne change rien : elle ne doit pas
    /// devenir un passage obligé du scan ordinaire.
    #[test]
    fn sous_le_plafond_la_confirmation_ne_change_rien() {
        for confirmee in [None, Some(0), Some(50)] {
            assert!(
                !purge_refusee(50, 70_346, confirmee),
                "50 pistes sur 70 346 passent, confirmee={confirmee:?}"
            );
        }
    }

    #[test]
    fn une_purge_massive_est_refusee() {
        // Chez Yacine : 21 277 sur 70 346, soit 30 %. Au-dessus du plafond,
        // on refuse — une disparition de cette ampleur est bien plus souvent
        // un montage absent qu'une suppression réelle.
        assert!(purge_trop_massive(21_277, 70_346));
        // Une purge ordinaire passe.
        assert!(!purge_trop_massive(50, 70_346));
        // Le plafond exact ne déclenche pas ; au-delà, oui.
        assert!(!purge_trop_massive(200, 1000));
        assert!(purge_trop_massive(201, 1000));
    }

    #[test]
    fn une_petite_bibliotheque_n_est_pas_soumise_au_plafond() {
        // Retirer 10 pistes sur 20 est banal quand on range à la main : un
        // pourcentage n'a pas de sens à cette échelle.
        assert!(!purge_trop_massive(10, 20));
        assert!(!purge_trop_massive(49, 49));
    }

    // ── Sous-arbres vidés : le montage IMBRIQUÉ qui tombe (#1943) ─────────

    fn perdues(prefixe: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefixe}/{i:04}.flac")).collect()
    }

    #[test]
    fn un_montage_imbrique_qui_tombe_protege_son_sous_arbre() {
        // La RACINE répond encore — un fichier y est découvert — donc ni
        // `missing_dirs`, ni `error_dirs`, ni `emptied_roots` ne voient rien.
        // C'est exactement le cas que le garde par racine laissait passer.
        let mut chemins = perdues("/mnt/music/nas/Jazz", 150);
        chemins.push("/mnt/music/local/ok.flac".to_string());
        let refs: Vec<&str> = chemins.iter().map(|s| s.as_str()).collect();
        let decouverts = set(&["/mnt/music/local/ok.flac"]);

        // Le garde par racine ne bronche pas :
        assert!(roots_gone_empty(&["/mnt/music".to_string()], &refs, &decouverts).is_empty());
        // Celui par sous-arbre, si :
        let v = sous_arbres_vides(&refs, &decouverts);
        assert!(
            v.iter()
                .any(|d| d == "/mnt/music/nas/Jazz" || d == "/mnt/music/nas"),
            "le sous-arbre perdu doit etre protege, obtenu {v:?}"
        );
    }

    #[test]
    fn supprimer_un_album_reste_possible() {
        // 15 pistes disparues d'un dossier : c'est un geste normal, et ces
        // fantomes-la doivent bien etre nettoyes. Proteger ici rendrait toute
        // suppression definitive impossible a refleter.
        let mut chemins = perdues("/mnt/music/Bach/Cantates", 15);
        chemins.push("/mnt/music/autre/ok.flac".to_string());
        let refs: Vec<&str> = chemins.iter().map(|s| s.as_str()).collect();
        let decouverts = set(&["/mnt/music/autre/ok.flac"]);
        assert!(
            sous_arbres_vides(&refs, &decouverts).is_empty(),
            "sous le seuil, on laisse nettoyer"
        );
    }

    #[test]
    fn un_dossier_qui_repond_encore_n_est_jamais_protege() {
        // Meme au-dela du seuil : s'il reste un fichier decouvert dessous, le
        // montage est la, et les absences sont de vraies suppressions.
        let chemins = perdues("/mnt/music/nas/Jazz", 200);
        let refs: Vec<&str> = chemins.iter().map(|s| s.as_str()).collect();
        let decouverts = set(&["/mnt/music/nas/Jazz/0000.flac"]);
        let v = sous_arbres_vides(&refs, &decouverts);
        assert!(
            !v.iter().any(|d| d == "/mnt/music/nas/Jazz"),
            "un dossier vivant ne se protege pas, obtenu {v:?}"
        );
    }

    #[test]
    fn seul_l_ancetre_est_retenu_pas_ses_enfants() {
        // Inutile de lister /nas ET /nas/Jazz : `sous_le_dossier` couvre deja
        // les enfants, et une liste redondante brouille le journal.
        let mut chemins = perdues("/mnt/music/nas/Jazz", 120);
        chemins.extend(perdues("/mnt/music/nas/Rock", 120));
        chemins.push("/mnt/music/local/ok.flac".to_string());
        let refs: Vec<&str> = chemins.iter().map(|s| s.as_str()).collect();
        let decouverts = set(&["/mnt/music/local/ok.flac"]);
        let v = sous_arbres_vides(&refs, &decouverts);
        assert_eq!(v.len(), 1, "un seul ancetre attendu, obtenu {v:?}");
        assert_eq!(v[0], "/mnt/music/nas");
    }

    #[test]
    fn un_sous_arbre_protege_empeche_la_suppression() {
        // Le bout de la chaine : le verdict doit changer.
        let v = verdict_purge(
            "/mnt/music/nas/Jazz/01.flac",
            &["/mnt/music".to_string()],
            &[],
            &[],
            &[],
            &["/mnt/music/nas".to_string()],
        );
        assert_eq!(v, VerdictPurge::ProtegeIllisible);
    }

    #[test]
    fn un_dossier_neuf_sans_piste_n_est_pas_concerne() {
        // Cas normal d'une racine fraîchement configurée : rien à perdre.
        let existants: [&str; 0] = [];
        let decouverts = set(&[]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    #[test]
    fn seule_la_racine_disparue_est_protegee() {
        // Un disque local intact ne doit pas cesser d'être nettoyé parce que
        // le NAS a disparu : la protection est par racine, pas globale.
        let local = "/home/dom/musique";
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/home/dom/musique/pop/01.flac",
            "/home/dom/musique/pop/supprime.flac",
        ];
        let decouverts = set(&["/home/dom/musique/pop/01.flac"]);
        assert_eq!(
            roots_gone_empty(
                &[NAS.to_string(), local.to_string()],
                &existants,
                &decouverts
            ),
            vec![NAS.to_string()]
        );
    }

    #[test]
    fn une_racine_avec_barre_finale_est_traitee_pareil() {
        let existants = ["/mnt/nas/musique/Bach/01.flac"];
        let decouverts = set(&[]);
        assert_eq!(
            roots_gone_empty(&["/mnt/nas/musique/".to_string()], &existants, &decouverts),
            vec!["/mnt/nas/musique/".to_string()]
        );
    }

    #[test]
    fn une_racine_voisine_de_meme_prefixe_ne_deteint_pas() {
        // `/mnt/nas/musique2` ne doit pas être considérée comme couverte par
        // `/mnt/nas/musique` : c'est la barre finale du préfixe qui l'évite.
        let existants = ["/mnt/nas/musique2/Bach/01.flac"];
        let decouverts = set(&["/mnt/nas/musique2/Bach/01.flac"]);
        assert!(
            roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty(),
            "la racine voisine ne doit ni proteger ni etre protegee a tort"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::scan_import::{decide_compilation_albums, is_various_artists};

    fn decide<'a>(
        tracks: &'a [(&'a str, &'a str, Option<&'a str>, bool)],
    ) -> std::collections::HashMap<(String, String), bool> {
        decide_compilation_albums(
            tracks
                .iter()
                .map(|(dir, album, aa, flag)| (dir.to_string(), *album, *aa, *flag)),
        )
    }

    fn is_comp(
        m: &std::collections::HashMap<(String, String), bool>,
        dir: &str,
        album: &str,
    ) -> bool {
        *m.get(&(dir.to_string(), album.to_lowercase())).unwrap()
    }

    #[test]
    fn va_sentinels() {
        for s in [
            "Various Artists",
            "various",
            "VA",
            "Compilations",
            "  various artists  ",
        ] {
            assert!(is_various_artists(s), "{s} should be VA");
        }
        for s in ["The Beatles", "Various State", "AC/DC"] {
            assert!(!is_various_artists(s), "{s} should not be VA");
        }
    }

    #[test]
    fn single_artist_album_is_not_compilation() {
        // Consistent album_artist across the album -> not a compilation.
        let m = decide(&[
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
        ]);
        assert!(!is_comp(&m, "/m/beatles/abbey", "Abbey Road"));
    }

    #[test]
    fn per_track_album_artist_variance_is_compilation() {
        // The reported bug: a compilation whose tracks each carry their own
        // artist as the album_artist (no flag, no "Various Artists").
        let m = decide(&[
            ("/m/comp/jazz", "Best of Jazz", Some("Miles Davis"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("John Coltrane"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("Bill Evans"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/jazz", "Best of Jazz"));
    }

    #[test]
    fn explicit_va_album_artist_is_compilation() {
        let m = decide(&[
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/hits", "Now 100"));
    }

    #[test]
    fn compilation_flag_wins_even_with_consistent_artist() {
        let m = decide(&[
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), true),
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/ost", "OST"));
    }

    #[test]
    fn features_with_consistent_album_artist_not_compilation() {
        // Guests on some tracks, but album_artist stays the main artist -> the
        // album must not be flagged as a compilation.
        let m = decide(&[
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
        ]);
        assert!(!is_comp(&m, "/m/drake/album", "Scorpion"));
    }

    #[test]
    fn distinct_albums_same_folder_decided_independently() {
        // Two different single-artist albums sharing a folder must not be merged
        // into a compilation just because two album_artists appear in the dir.
        let m = decide(&[
            ("/m/singles", "Album A", Some("Artist A"), false),
            ("/m/singles", "Album B", Some("Artist B"), false),
        ]);
        assert!(!is_comp(&m, "/m/singles", "Album A"));
        assert!(!is_comp(&m, "/m/singles", "Album B"));
    }

    #[test]
    fn no_album_artist_is_not_flagged_compilation() {
        // Missing album_artist is left to the folder-first-artist heuristic in
        // the scan loop, not treated as a compilation here.
        let m = decide(&[
            ("/m/x/rec", "Recital", None, false),
            ("/m/x/rec", "Recital", None, false),
        ]);
        assert!(!is_comp(&m, "/m/x/rec", "Recital"));
    }

    #[test]
    fn same_album_title_different_folders_are_separate() {
        let m = decide(&[
            ("/m/a/greatest", "Greatest Hits", Some("Queen"), false),
            ("/m/b/greatest", "Greatest Hits", Some("ABBA"), false),
        ]);
        assert!(!is_comp(&m, "/m/a/greatest", "Greatest Hits"));
        assert!(!is_comp(&m, "/m/b/greatest", "Greatest Hits"));
    }

    #[test]
    fn parse_hhmm_accepts_valid_rejects_invalid() {
        assert_eq!(super::parse_hhmm("03:00"), Some((3, 0)));
        assert_eq!(super::parse_hhmm(" 23:59 "), Some((23, 59)));
        assert_eq!(super::parse_hhmm("3:5"), Some((3, 5)));
        assert_eq!(super::parse_hhmm("24:00"), None);
        assert_eq!(super::parse_hhmm("12:60"), None);
        assert_eq!(super::parse_hhmm("noon"), None);
        assert_eq!(super::parse_hhmm(""), None);
    }
}

/// Garde-fou : le rapport de fin de scan est construit QUATRE fois, et les
/// quatre doivent publier ce que la purge a retiré.
///
/// Le bandeau de fin de scan annonçait « 0 supprimés » quoi que la purge ait
/// fait : le client lit `d.removed`, et aucune des constructions du rapport
/// n'envoyait cette clé (#2146). Le compte existait pourtant — il mourait avec
/// le bloc qui le calculait.
///
/// Ces constructions ne sont reliées par rien de mécanique : ce sont quatre
/// `json!` recopiés à la main, et ils ont DÉJÀ divergé une fois
/// (`skipped_unsupported_by_ext`, qui n'existe que dans un seul — #2012).
/// Ajouter une clé à trois d'entre eux et pas au quatrième ne casse aucune
/// compilation et ne fait échouer aucun test fonctionnel : le champ manque
/// simplement chez un consommateur, en silence. D'où ce test, sur le modèle de
/// `tests/smb_dialect_seam.rs`.
///
/// Le quatrième exemplaire est celui du scan AUTOMATIQUE (`auto_scan.rs`) :
/// l'issue ne le comptait pas parmi les trois, mais il purge (`pruned`) et il
/// émet `library.scan.completed`, donc il alimente le même bandeau.
#[cfg(test)]
mod rapport_de_scan_publie_la_purge {
    use std::fs;
    use std::path::PathBuf;

    /// Une clé de rapport, sous sa forme littérale exacte. `"removed"` tout
    /// court apparaît aussi dans les noms d'événements de journal
    /// (`post_scan_stale_tracks_removed`) : chercher le mot nu ferait passer
    /// le test sans qu'aucune clé soit publiée.
    const CLE_PURGE: &str = "\"removed\":";

    /// Présent dans les quatre rapports, et nulle part ailleurs : sert à
    /// reconnaître un rapport de fin de scan sans compter d'accolades.
    const MARQUEUR_RAPPORT: &str = "\"artwork_extracted\":";

    fn source(chemin: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(chemin);
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("lecture de {} : {e}", p.display()))
    }

    /// Le corps du `json!` qui précède le marqueur, commentaires ÔTÉS.
    ///
    /// Les commentaires nomment le défaut corrigé — « le client lit
    /// `d.removed` » — et raconter l'histoire ne doit pas suffire à faire
    /// passer le test. Seul le code compte.
    fn corps_du_rapport(texte: &str, fin: usize) -> String {
        let debut = texte[..fin]
            .rfind("json!(")
            .expect("un rapport doit être construit par un json!(");
        texte[debut..fin]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn les_quatre_constructions_du_rapport_publient_la_cle_lue_par_le_client() {
        let fichiers = ["src/routes/system/scan.rs", "src/auto_scan.rs"];
        let mut examines = 0usize;
        for fichier in fichiers {
            let texte = source(fichier);
            let mut depuis = 0usize;
            while let Some(rel) = texte[depuis..].find(MARQUEUR_RAPPORT) {
                let fin = depuis + rel;
                examines += 1;
                let corps = corps_du_rapport(&texte, fin);
                assert!(
                    corps.contains(CLE_PURGE),
                    "{fichier} : la construction de rapport n° {examines} ne publie pas \
                     {CLE_PURGE}\nLe client lit `d.removed` pour le bandeau de fin de scan. \
                     Un rapport qui ne l'envoie pas fait annoncer « 0 supprimés » quoi que la \
                     purge ait fait (#2146). Les quatre constructions doivent porter la clé \
                     (#2012)."
                );
                depuis = fin + MARQUEUR_RAPPORT.len();
            }
        }
        // Contrôle positif : sans lui, un marqueur renommé ferait passer le
        // test en n'examinant RIEN. Quatre, c'est `scan_result`,
        // `library.scan.completed`, `/scan/report`, et le rapport du scan
        // automatique.
        assert_eq!(
            examines, 4,
            "quatre constructions de rapport attendues, {examines} trouvée(s) — le marqueur \
             {MARQUEUR_RAPPORT} a dû être renommé, ou un exemplaire ajouté/supprimé"
        );
    }
}

/// Ordonnanceur du scan programmé (#2469).
///
/// Ces tests jugent `decider`, qui porte TOUTE la décision : la boucle qui
/// l'entoure ne fait que lire trois réglages et obéir. L'horloge est injectée,
/// donc « la machine était éteinte de 20 h 55 à 21 h 05 » s'écrit littéralement.
#[cfg(test)]
mod scan_scheduler_tests {
    use super::{MOTIF_AMORCE, MOTIF_SCAN_DE_DEMARRAGE, Ordre, decider};
    use time::macros::{date, datetime};

    const HORAIRE: &str = "21:00";
    /// Aucun scan de démarrage : le cas d'un poste ordinaire, `TUNE_AUTO_SCAN`
    /// valant `false` par défaut (`tune-server/src/config.rs`).
    const SANS_SCAN_DE_DEMARRAGE: bool = false;

    /// L'occurrence tombe pendant que le serveur tourne : elle part.
    #[test]
    fn heure_atteinte_declenche_le_scan() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 21:00 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 27))
        );
    }

    /// Une minute avant, rien : l'occurrence de la veille est déjà honorée.
    /// C'est la contre-épreuve du test précédent — sans elle, un `decider` qui
    /// renverrait toujours `Scanner` passerait pour correct.
    #[test]
    fn avant_l_heure_ne_declenche_rien() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 20:59 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Rien
        );
    }

    /// La contre-épreuve exigée par #2469, mot pour mot : occurrence à 21 h 00,
    /// processus absent de 20 h 55 à 21 h 05, redémarrage — le scan doit partir.
    #[test]
    fn occurrence_manquee_machine_eteinte_est_rattrapee_au_demarrage() {
        // Dernier passage : la veille, à l'heure. Le serveur s'arrête à 20 h 55.
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 20:55 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Rien,
            "rien n'est dû avant l'heure : l'état persisté est bien celui de la veille"
        );
        // Redémarrage à 21 h 05, la même trace en base.
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 21:05 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 27)),
            "l'occurrence de 21 h 00 manquée doit partir au démarrage"
        );
    }

    /// Le cas de Thierry : machine éteinte toute la nuit, rallumée le lendemain
    /// matin. L'occurrence de la VEILLE est due — pas celle du jour, qui n'a pas
    /// encore d'heure.
    #[test]
    fn machine_rallumee_le_lendemain_matin_rattrape_l_occurrence_de_la_veille() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-28 09:30 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 27))
        );
    }

    /// Le point que l'issue signalait comme NON examiné : un Mac qui dort à
    /// 21 h 00 n'est pas un Mac éteint, et `tokio::time::sleep` peut se réveiller
    /// bien après la minute programmée. Ici la minute n'a pas besoin d'être
    /// touchée — seule compte l'occurrence.
    #[test]
    fn reveil_de_veille_tardif_declenche_quand_meme() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 23:47 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 27))
        );
    }

    /// Une semaine de vacances ne vaut PAS sept scans. Le retard ne s'accumule
    /// pas : une seule occurrence est due, la plus récente.
    #[test]
    fn six_jours_manques_ne_valent_qu_une_seule_occurrence() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 09:00 UTC),
                Some("2026-08-20"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 26)),
            "seule la dernière occurrence échue est due"
        );
        // Et une fois celle-là honorée, plus rien avant 21 h.
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 09:01 UTC),
                Some("2026-08-26"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Rien
        );
    }

    /// Deux démarrages rapprochés après un rendez-vous manqué : le premier
    /// rattrape, le second ne doit RIEN faire. Un scan coûte cher ; le rallumage
    /// en rafale (redémarrage, mise à jour, plantage) ne doit pas le multiplier.
    #[test]
    fn deux_demarrages_rapproches_ne_scannent_qu_une_fois() {
        let premier = decider(
            true,
            HORAIRE,
            datetime!(2026-08-28 08:00 UTC),
            Some("2026-08-26"),
            SANS_SCAN_DE_DEMARRAGE,
        );
        let Ordre::Scanner(jour) = premier else {
            panic!("le premier démarrage doit rattraper, obtenu {premier:?}");
        };
        // Exactement ce que la boucle persiste juste avant de lancer le scan.
        let persiste = super::date_iso(jour);
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-28 08:03 UTC),
                Some(&persiste),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Rien,
            "le second démarrage relit la trace du premier et ne relance rien"
        );
    }

    /// L'autre moitié du « jamais deux scans » : quand `TUNE_AUTO_SCAN` est
    /// actif, le scan de démarrage fait déjà le travail de l'occurrence manquée.
    /// Elle est inscrite, pas rejouée — sinon la machine rallumée après un
    /// rendez-vous manqué scannerait DEUX fois de suite.
    #[test]
    fn le_scan_de_demarrage_couvre_l_occurrence_manquee() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-28 09:30 UTC),
                Some("2026-08-26"),
                true
            ),
            Ordre::Noter(date!(2026 - 08 - 27), MOTIF_SCAN_DE_DEMARRAGE),
            "l'occurrence est honorée par le scan de démarrage, pas rejouée"
        );
    }

    /// Et la couverture ne vaut QUE pour le premier tour : la boucle consomme le
    /// drapeau avec `std::mem::take`. Trois jours plus tard, une occurrence due
    /// scanne bel et bien.
    #[test]
    fn la_couverture_de_demarrage_ne_vaut_pas_pour_les_jours_suivants() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-31 21:00 UTC),
                Some("2026-08-30"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 31))
        );
    }

    /// Contre-épreuve de la consommation du drapeau : `decider` seul ne peut
    /// pas la montrer, elle vit dans `Ordonnanceur::tour`. Sans ce test, un
    /// drapeau jamais consommé passe toutes les autres assertions — et mute
    /// définitivement le scan programmé sur une machine en `TUNE_AUTO_SCAN`.
    #[test]
    fn le_drapeau_de_demarrage_ne_couvre_que_le_premier_tour() {
        let mut ordonnanceur = super::Ordonnanceur::new(true);
        assert_eq!(
            ordonnanceur.tour(
                true,
                HORAIRE,
                datetime!(2026-08-28 09:30 UTC),
                Some("2026-08-26")
            ),
            Ordre::Noter(date!(2026 - 08 - 27), MOTIF_SCAN_DE_DEMARRAGE),
            "premier tour : le scan de démarrage couvre l'occurrence manquée"
        );
        assert_eq!(
            ordonnanceur.tour(
                true,
                HORAIRE,
                datetime!(2026-08-29 21:00 UTC),
                Some("2026-08-27")
            ),
            Ordre::Scanner(date!(2026 - 08 - 29)),
            "le lendemain, le scan de démarrage est loin derrière : l'occurrence part"
        );
    }

    /// Et le drapeau est consommé même par un tour qui ne décide rien : sinon il
    /// survivrait jusqu'à l'occurrence du soir, des heures après la fin du scan
    /// de démarrage, et la mangerait.
    #[test]
    fn le_drapeau_est_consomme_meme_par_un_tour_sans_occurrence() {
        let mut ordonnanceur = super::Ordonnanceur::new(true);
        assert_eq!(
            ordonnanceur.tour(
                true,
                HORAIRE,
                datetime!(2026-08-27 09:00 UTC),
                Some("2026-08-26")
            ),
            Ordre::Rien,
            "au démarrage du matin, rien n'est dû"
        );
        assert_eq!(
            ordonnanceur.tour(
                true,
                HORAIRE,
                datetime!(2026-08-27 21:00 UTC),
                Some("2026-08-26")
            ),
            Ordre::Scanner(date!(2026 - 08 - 27)),
            "le soir même, l'occurrence part normalement"
        );
    }

    /// Sans scan de démarrage — le cas par défaut — le premier tour scanne déjà.
    #[test]
    fn sans_scan_de_demarrage_le_premier_tour_rattrape() {
        let mut ordonnanceur = super::Ordonnanceur::new(SANS_SCAN_DE_DEMARRAGE);
        assert_eq!(
            ordonnanceur.tour(
                true,
                HORAIRE,
                datetime!(2026-08-28 09:30 UTC),
                Some("2026-08-26")
            ),
            Ordre::Scanner(date!(2026 - 08 - 27))
        );
    }

    /// Réglage désactivé : rien, même avec une occurrence manifestement manquée.
    #[test]
    fn reglage_desactive_ne_fait_rien() {
        assert_eq!(
            decider(
                false,
                HORAIRE,
                datetime!(2026-08-28 09:30 UTC),
                Some("2026-08-20"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Rien
        );
    }

    /// Aucune trace en base : on AMORCE, on ne scanne pas. Sans cela, la montée
    /// de version qui porte ce correctif déclencherait un scan complet chez tous
    /// les utilisateurs ayant la bascule active. Une absence n'est pas une
    /// occurrence manquée.
    #[test]
    fn premiere_observation_amorce_sans_scanner() {
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-27 09:30 UTC),
                None,
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Noter(date!(2026 - 08 - 26), MOTIF_AMORCE)
        );
    }

    /// Activer l'option APRÈS l'heure du jour ne doit pas scanner sur-le-champ,
    /// et doit scanner le lendemain à l'heure. Les deux moitiés comptent : la
    /// première seule serait satisfaite par un ordonnanceur mort.
    #[test]
    fn activer_le_soir_amorce_puis_scanne_le_lendemain() {
        let amorce = decider(
            true,
            HORAIRE,
            datetime!(2026-08-27 22:10 UTC),
            None,
            SANS_SCAN_DE_DEMARRAGE,
        );
        assert_eq!(
            amorce,
            Ordre::Noter(date!(2026 - 08 - 27), MOTIF_AMORCE),
            "activer à 22 h 10 pour 21 h 00 ne doit pas scanner le soir même"
        );
        let Ordre::Noter(jour, _) = amorce else {
            unreachable!()
        };
        let persiste = super::date_iso(jour);
        assert_eq!(
            decider(
                true,
                HORAIRE,
                datetime!(2026-08-28 21:00 UTC),
                Some(&persiste),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 28)),
            "et le lendemain à 21 h 00, il part"
        );
    }

    /// Une trace illisible est traitée comme une absence : on amorce. Le piège
    /// serait de la traiter comme « très ancienne » et de scanner.
    #[test]
    fn trace_illisible_amorce_au_lieu_de_scanner() {
        for corrompu in ["", "hier", "2026-13-01", "2026-08", "2026-08-27-01"] {
            assert_eq!(
                decider(
                    true,
                    HORAIRE,
                    datetime!(2026-08-27 22:00 UTC),
                    Some(corrompu),
                    SANS_SCAN_DE_DEMARRAGE
                ),
                Ordre::Noter(date!(2026 - 08 - 27), MOTIF_AMORCE),
                "trace « {corrompu} »"
            );
        }
    }

    /// Horaire illisible : rien du tout. Surtout pas un scan quotidien décidé
    /// par défaut sur une valeur que l'utilisateur n'a pas écrite.
    #[test]
    fn horaire_illisible_ne_fait_rien() {
        for mauvais in ["", "25:00", "21:60", "21h00", "abc"] {
            assert_eq!(
                decider(
                    true,
                    mauvais,
                    datetime!(2026-08-27 22:00 UTC),
                    Some("2026-08-20"),
                    SANS_SCAN_DE_DEMARRAGE
                ),
                Ordre::Rien,
                "horaire « {mauvais} »"
            );
        }
    }

    /// Minuit : l'occurrence d'aujourd'hui est atteinte dès 00 h 00, et la
    /// veille reste due tant qu'elle ne l'est pas. Cas limite de
    /// `previous_day()` sur un changement de mois.
    #[test]
    fn minuit_et_changement_de_mois() {
        assert_eq!(
            decider(
                true,
                "00:00",
                datetime!(2026-09-01 00:00 UTC),
                Some("2026-08-31"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 09 - 01))
        );
        assert_eq!(
            decider(
                true,
                "23:30",
                datetime!(2026-09-01 00:10 UTC),
                Some("2026-08-30"),
                SANS_SCAN_DE_DEMARRAGE
            ),
            Ordre::Scanner(date!(2026 - 08 - 31)),
            "à 00 h 10, l'occurrence de 23 h 30 due est celle du 31 août"
        );
    }
}

/// L'ordonnanceur a été du CODE MORT pendant des mois : `spawn_scan_scheduler`
/// n'avait qu'une seule occurrence dans tout le dépôt, sa définition, et la
/// bascule « scan planifié » des clients était muette depuis la PR #1230.
///
/// Aucun test de comportement ne pouvait le voir : ils passaient tous sans que
/// la boucle tourne jamais. Ce test lit le texte du seul endroit qui la lance et
/// exige que l'appel y soit — même procédé que le garde-fou de rendement du
/// balayage acoustique (`embedding.rs`), pour la même raison : ce qui a été
/// perdu, c'est un APPEL, pas une logique.
#[cfg(test)]
mod scan_scheduler_cablage_tests {
    #[test]
    fn l_ordonnanceur_est_bien_lance_au_demarrage() {
        let background = include_str!("../../background.rs");
        // Témoin : si `include_str!` pointait sur un fichier vide ou faux,
        // l'assertion suivante échouerait pour la mauvaise raison.
        assert!(
            background.contains("pub async fn spawn_background_tasks"),
            "témoin : le fichier lu doit être celui qui câble les passes de fond"
        );
        assert!(
            background.contains("scan::spawn_scan_scheduler(state.clone(), config.auto_scan)"),
            "spawn_scan_scheduler doit être appelé depuis background.rs, en lui \
             passant `config.auto_scan` — sans cet appel, la bascule « scan \
             planifié » est sans effet (#2469)"
        );
    }
}

/// Le sort de l'enrichissement automatique d'après scan (#2507).
///
/// Reivax66 installe TuneOS Fedora **sans licence**, scanne, et la grille
/// Artistes n'affiche que des initiales. Les deux conditions de la passe —
/// `enrich_on_scan` et `Feature::AutoEnrichment` (Premium) — sont appliquées
/// depuis toujours ; ce qui manquait, c'est de le DIRE ailleurs qu'au journal.
#[cfg(test)]
mod suite_du_scan_apres_scan {
    use super::SuiteDuScan;

    #[test]
    fn sans_licence_le_motif_est_premium_meme_reglage_allume() {
        let suite = SuiteDuScan::decider(true, false);
        assert!(!suite.demarree());
        assert_eq!(suite.motif(), Some("premium_required"));
    }

    /// Le cas exact du ticket : réglage à sa valeur par défaut (allumé), pas
    /// de licence. Le rapport doit porter le motif, pas un simple `false`.
    #[test]
    fn le_rapport_publie_started_et_le_motif() {
        let rapport = SuiteDuScan::decider(true, false).rapport();
        assert_eq!(rapport["started"], serde_json::json!(false));
        assert_eq!(
            rapport["skipped_reason"],
            serde_json::json!("premium_required")
        );
    }

    /// Premium et réglage éteint : c'est un choix de l'utilisateur, et il doit
    /// se distinguer du refus d'offre — sinon on envoie un abonné Premium
    /// acheter ce qu'il a déjà.
    #[test]
    fn premium_mais_reglage_eteint_est_un_motif_distinct() {
        let suite = SuiteDuScan::decider(false, true);
        assert!(!suite.demarree());
        assert_eq!(suite.motif(), Some("disabled_by_setting"));
    }

    /// Les deux refus à la fois : le manque de licence l'emporte, parce que
    /// c'est le seul que les Réglages ne peuvent pas lever.
    #[test]
    fn sans_licence_et_reglage_eteint_le_manque_de_licence_lemporte() {
        assert_eq!(
            SuiteDuScan::decider(false, false).motif(),
            Some("premium_required")
        );
    }

    /// Contrôle positif : sans lui, un `decider` qui rendrait TOUJOURS un
    /// refus passerait les quatre tests ci-dessus.
    #[test]
    fn premium_et_reglage_allume_la_passe_part_sans_motif() {
        let suite = SuiteDuScan::decider(true, true);
        assert!(suite.demarree());
        assert_eq!(suite.motif(), None);
        assert_eq!(suite.rapport()["started"], serde_json::json!(true));
        assert_eq!(suite.rapport()["skipped_reason"], serde_json::Value::Null);
    }
}

/// La clé doit être dans les TROIS rapports de fin de scan complet.
///
/// `scan_result` (lu par `/scan/status`), `library.scan.completed` (le bandeau
/// de fin de scan) et le fichier de `/scan/report` sont trois `json!` recopiés
/// à la main. Ils ont déjà divergé deux fois — `skipped_unsupported_by_ext`
/// (#2012) puis `removed` (#2146) — et une clé posée dans deux d'entre eux sur
/// trois ne casse aucune compilation : elle manque simplement chez un
/// consommateur, en silence. Même garde que
/// `rapport_de_scan_publie_la_purge`, sur la clé de #2507.
#[cfg(test)]
mod rapport_de_scan_publie_le_sort_de_lenrichissement {
    use std::fs;
    use std::path::PathBuf;

    /// La clé, sous sa forme littérale exacte. `auto_enrichment` tout court
    /// apparaît aussi dans le nom de l'événement de journal
    /// (`auto_enrichment_after_scan_skipped`) : chercher le mot nu ferait
    /// passer le test sans qu'aucun rapport ne publie quoi que ce soit.
    const CLE: &str = "\"auto_enrichment\": suite_du_scan.rapport(),";
    /// Dernière clé commune aux trois rapports, et postérieure à celle-ci :
    /// borne la portion de texte examinée.
    const MARQUEUR: &str = "\"failed_paths\": scan_stats.failed_paths,";

    #[test]
    fn les_trois_rapports_du_scan_complet_publient_la_cle() {
        let chemin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/routes/system/scan.rs");
        let texte = fs::read_to_string(&chemin)
            .unwrap_or_else(|e| panic!("lecture de {} : {e}", chemin.display()));
        let mut examines = 0usize;
        let mut depuis = 0usize;
        while let Some(rel) = texte[depuis..].find(MARQUEUR) {
            let fin = depuis + rel;
            examines += 1;
            let debut = texte[..fin]
                .rfind("json!(")
                .expect("un rapport doit être construit par un json!(");
            let corps = texte[debut..fin]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                corps.contains(CLE),
                "rapport n° {examines} : {CLE} manque.\nSans cette clé, une \
                 installation sans licence scanne, ne voit aucune vignette \
                 d'artiste, et rien ne lui dit que la passe n'a pas eu lieu \
                 (#2507)."
            );
            depuis = fin + MARQUEUR.len();
        }
        // Contrôle positif : sans lui, un marqueur renommé ferait passer le
        // test en n'examinant RIEN. Trois, c'est `scan_result`,
        // `library.scan.completed` et le fichier de `/scan/report`.
        assert_eq!(
            examines, 3,
            "trois rapports attendus, {examines} trouvé(s) — le marqueur \
             {MARQUEUR} a dû être renommé, ou un exemplaire ajouté/supprimé"
        );
    }
}
