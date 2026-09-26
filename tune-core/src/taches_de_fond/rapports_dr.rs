//! Le rattrapage des rapports `foo_dr.txt`, sans rien décoder (#5168).
//!
//! ## Le défaut
//!
//! La lecture du DR dans un rapport voisin (#4186, v0.9.152) n'est branchée
//! que dans `metadata::read_extended_metadata`, donc au scan d'un FICHIER.
//! Le scan incrémental ne relit pas un fichier qui n'a pas changé : une
//! bibliothèque scannée avant la 0.9.152 — ou un `foo_dr.txt` posé après
//! coup à côté de fichiers déjà connus — ne voit jamais ses rapports. Chez
//! Thierry (Tades, 537 910 pistes), la carte « Plage dynamique » disait
//! « 0 mesurées par Tune, 22 lues dans les tags » devant des centaines de
//! dossiers qui portaient un rapport.
//!
//! ## Ce que fait la passe
//!
//! Pour les pistes SANS `dr_track`, regroupées par dossier :
//!
//! 1. un dossier déjà vu, dont rien n'a bougé, coûte **un seul `stat`** —
//!    celui du rapport qu'on y a trouvé (sa date de modification), ou celui
//!    du dossier quand il n'y en avait pas (poser un fichier dans un dossier
//!    change la date du dossier) ;
//! 2. sinon, [`foo_dr::rapport_du_dossier`] relit le dossier — la même
//!    recherche que le scan, mêmes noms, mêmes bornes — et chaque piste est
//!    appariée par le code du scan ([`foo_dr::RapportDr::dr_pour_la_piste`] :
//!    numéro de piste, disque, titre, canaux) ;
//! 3. un DR apparié s'écrit avec `dr_source = "sidecar"`
//!    ([`crate::metadata::DR_SOURCE_SIDECAR`]), SEULEMENT dans le vide —
//!    `replaygain::peut_ecrire_le_dr`, relu juste avant d'écrire. La
//!    précédence ne change pas : un tag du fichier ou une mesure de Tune déjà
//!    en base ne sont jamais écrasés.
//!
//! ## La mémoire, sans migration
//!
//! Ce qui a été vu d'un dossier tient dans UNE clé de `track_metadata`,
//! [`CLE_TEMOIN`], posée sur la piste sans DR de plus petit identifiant du
//! dossier. Elle porte la date du rapport (ou du dossier) et le plus grand
//! identifiant de piste vu : une piste AJOUTÉE au dossier depuis (identifiant
//! plus grand) fait relire le dossier, même si le rapport n'a pas bougé.
//! Pas dans `settings` : `GET /system/config` rend la table entière au
//! client, et 40 000 dossiers y pèseraient des mégaoctets.
//!
//! ## Quand elle tourne
//!
//! Au démarrage (après 90 s), puis toutes les [`INTERVALLE`], et aussitôt
//! qu'un `.txt` apparaît ou change dans un dossier surveillé
//! ([`signaler_le_dossier`], appelé par `scanner::watcher`). Elle respecte la
//! pause de « Plage dynamique » et cède à la lecture
//! ([`super::priorite`]). Elle ne décode rien, et ne prend donc pas le
//! créneau d'analyse des passes lourdes.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::db::backend::DbBackend;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::metadata::foo_dr;

/// La clé de `track_metadata` qui mémorise ce qu'on a vu d'un dossier.
pub const CLE_TEMOIN: &str = "dr_rapport_vu";

/// Entre deux passes complètes, faute de signal du surveillant.
pub const INTERVALLE: Duration = Duration::from_secs(2 * 3600);

/// Attente au démarrage : laisser le serveur et le scan de démarrage
/// s'installer.
const ATTENTE_AU_DEMARRAGE: Duration = Duration::from_secs(90);

/// Regroupe les signaux du surveillant : un éditeur qui réécrit un rapport
/// en trois événements ne doit lancer qu'une passe.
const REGROUPEMENT: Duration = Duration::from_secs(10);

/// Pendant la lecture, céder tous les N dossiers RELUS…
const CEDER_TOUS_LES_DOSSIERS_LUS: usize = 50;
/// … et tous les N dossiers inchangés (un `stat` chacun).
const CEDER_TOUS_LES_DOSSIERS_INCHANGES: usize = 2000;

/// Ce qu'une passe a fait — servi par `GET /system/background-tasks`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Bilan {
    /// Pistes sans DR examinées.
    pub tracks_without_dr: usize,
    /// Dossiers qui les contiennent.
    pub folders: usize,
    /// Dossiers écartés sur un seul `stat` : rien n'a bougé.
    pub folders_unchanged: usize,
    /// Dossiers relus (`read_dir`, puis lecture du rapport s'il y en a un).
    pub folders_read: usize,
    /// Dossiers introuvables (partage démonté) : ni lus, ni mémorisés.
    pub folders_missing: usize,
    /// Rapports trouvés parmi les dossiers relus.
    pub reports_found: usize,
    /// `stat` émis par la passe.
    pub stats: usize,
    /// Pistes qui ont reçu leur DR d'un rapport.
    pub tracks_written: usize,
    /// La passe s'est arrêtée sur une pause de l'utilisateur.
    pub interrupted: bool,
    /// Durée de la passe.
    pub duration_ms: u64,
}

/// Le relevé de la passe : tourne-t-elle, et qu'a fait la dernière.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Releve {
    pub running: bool,
    pub last: Option<Bilan>,
    pub last_finished_epoch_s: Option<u64>,
}

static EN_COURS: AtomicBool = AtomicBool::new(false);
static DERNIER: Mutex<Option<(Bilan, u64)>> = Mutex::new(None);
static SIGNALES: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
static REVEIL: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// Le relevé, sans requête.
pub fn releve() -> Releve {
    let dernier = DERNIER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Releve {
        running: EN_COURS.load(Ordering::Relaxed),
        last_finished_epoch_s: dernier.as_ref().map(|(_, t)| *t),
        last: dernier.map(|(b, _)| b),
    }
}

/// La passe tourne-t-elle en ce moment ?
pub fn en_cours() -> bool {
    EN_COURS.load(Ordering::Relaxed)
}

/// Le surveillant de fichiers a vu un `.txt` apparaître ou changer dans ce
/// dossier : réveiller la passe, et relire CE dossier sans consulter la
/// mémoire.
pub fn signaler_le_dossier(dossier: PathBuf) {
    SIGNALES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(dossier);
    REVEIL.notify_one();
}

/// Les dossiers signalés et pas encore traités — pour les témoins.
pub fn dossiers_signales() -> Vec<PathBuf> {
    SIGNALES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

fn prendre_les_signales() -> HashSet<PathBuf> {
    SIGNALES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default()
}

/// Ce qu'on a vu d'un dossier, tel que [`CLE_TEMOIN`] le garde.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Temoin {
    /// Un rapport, `nom` dans le dossier, modifié à `mtime` (ns).
    Rapport {
        nom: String,
        mtime: u128,
        max_id: i64,
    },
    /// Pas de rapport ; le dossier était daté `mtime` (ns).
    SansRapport { mtime: u128, max_id: i64 },
}

impl Temoin {
    fn encoder(&self) -> String {
        match self {
            Temoin::Rapport { nom, mtime, max_id } => format!("v1;r;{mtime};{max_id};{nom}"),
            Temoin::SansRapport { mtime, max_id } => format!("v1;d;{mtime};{max_id}"),
        }
    }

    fn decoder(brut: &str) -> Option<Self> {
        let mut p = brut.splitn(5, ';');
        if p.next()? != "v1" {
            return None;
        }
        let genre = p.next()?;
        let mtime = p.next()?.parse().ok()?;
        let max_id = p.next()?.parse().ok()?;
        match genre {
            "r" => Some(Temoin::Rapport {
                nom: p.next().filter(|n| !n.is_empty())?.to_string(),
                mtime,
                max_id,
            }),
            "d" => Some(Temoin::SansRapport { mtime, max_id }),
            _ => None,
        }
    }

    fn max_id(&self) -> i64 {
        match self {
            Temoin::Rapport { max_id, .. } | Temoin::SansRapport { max_id, .. } => *max_id,
        }
    }
}

fn mtime_ns(m: &std::fs::Metadata) -> Option<u128> {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

/// Une piste sans DR, telle que la base la connaît.
#[derive(Debug, Clone)]
struct Piste {
    id: i64,
    chemin: String,
    numero: Option<u32>,
    disque: Option<u32>,
    titre: Option<String>,
    canaux: Option<u16>,
    temoin: Option<String>,
}

/// Les pistes sans DR, par identifiant croissant.
///
/// Les pistes CUE virtuelles ont `file_path` NUL et leur fichier dans
/// `cue_media_path` : le dossier est le même, le numéro de piste est celui de
/// la feuille.
const CANDIDATS_SQL: &str = "SELECT t.id, COALESCE(NULLIF(t.file_path, ''), t.cue_media_path), \
            t.track_number, t.disc_number, t.title, t.channels, v.value \
     FROM tracks t \
     LEFT JOIN track_metadata v ON v.track_id = t.id AND v.key = 'dr_rapport_vu' \
     WHERE COALESCE(NULLIF(t.file_path, ''), t.cue_media_path) IS NOT NULL \
       AND COALESCE(NULLIF(t.file_path, ''), t.cue_media_path) != '' \
       AND NOT EXISTS (SELECT 1 FROM track_metadata m \
             WHERE m.track_id = t.id AND m.key = 'dr_track' AND TRIM(m.value) != '') \
     ORDER BY t.id";

fn positif(v: Option<i64>) -> Option<u32> {
    v.filter(|n| *n > 0).and_then(|n| u32::try_from(n).ok())
}

fn lire_les_candidats(backend: &Arc<dyn DbBackend>) -> Result<Vec<Piste>, String> {
    let lignes = backend.query_many(CANDIDATS_SQL, &[])?;
    Ok(lignes
        .into_iter()
        .filter_map(|r| {
            let id = r.first().and_then(|v| v.as_i64())?;
            let chemin = r.get(1).and_then(|v| v.as_string())?;
            Some(Piste {
                id,
                chemin,
                numero: positif(r.get(2).and_then(|v| v.as_i64())),
                disque: positif(r.get(3).and_then(|v| v.as_i64())),
                titre: r.get(4).and_then(|v| v.as_string()),
                canaux: r
                    .get(5)
                    .and_then(|v| v.as_i64())
                    .and_then(|c| u16::try_from(c).ok()),
                temoin: r.get(6).and_then(|v| v.as_string()),
            })
        })
        .collect())
}

/// Poser un DR lu dans un rapport, seulement dans le vide.
///
/// Relu JUSTE avant d'écrire, comme la passe d'analyse : un scan ou une
/// mesure a pu en poser un depuis la sélection.
fn ecrire_le_dr_du_rapport(repo: &TrackMetadataRepo, track_id: i64, dr: u8) -> bool {
    let existant = repo
        .get_all(track_id)
        .ok()
        .and_then(|m| m.get("dr_track").cloned());
    if !crate::audio::replaygain::peut_ecrire_le_dr(existant.as_deref()) {
        return false;
    }
    repo.set(track_id, "dr_track", &dr.to_string()).is_ok()
        && repo
            .set(track_id, "dr_source", crate::metadata::DR_SOURCE_SIDECAR)
            .is_ok()
}

/// Le dossier tel que le disque le connaît : le chemin de la base d'abord,
/// puis la graphie que `resolve_local_path` trouve pour une de ses pistes
/// (base en NFC, disque en NFD sous macOS ou SMB).
fn dossier_sur_disque(
    dossier: &Path,
    pistes: &[Piste],
    bilan: &mut Bilan,
) -> Option<(PathBuf, std::fs::Metadata)> {
    bilan.stats += 1;
    if let Ok(m) = std::fs::metadata(dossier) {
        return Some((dossier.to_path_buf(), m));
    }
    let reel = crate::library::local_path::resolve_local_path(&pistes.first()?.chemin).found()?;
    let parent = Path::new(&reel).parent()?.to_path_buf();
    bilan.stats += 1;
    let m = std::fs::metadata(&parent).ok()?;
    Some((parent, m))
}

/// Traiter un dossier. Rend `true` s'il a été RELU (pour la cadence de
/// cession à la lecture).
fn traiter_le_dossier(
    repo: &TrackMetadataRepo,
    dossier: &Path,
    pistes: &[Piste],
    force: bool,
    bilan: &mut Bilan,
) -> bool {
    let max_id = pistes.iter().map(|p| p.id).max().unwrap_or(0);

    // 1. La mémoire : un seul `stat` quand rien n'a bougé.
    if !force
        && let Some(t) = pistes
            .first()
            .and_then(|p| p.temoin.as_deref())
            .and_then(Temoin::decoder)
        && t.max_id() >= max_id
    {
        let (cible, attendu) = match &t {
            Temoin::Rapport { nom, mtime, .. } => (dossier.join(nom), *mtime),
            Temoin::SansRapport { mtime, .. } => (dossier.to_path_buf(), *mtime),
        };
        bilan.stats += 1;
        if std::fs::metadata(&cible)
            .ok()
            .and_then(|m| mtime_ns(&m))
            .is_some_and(|m| m == attendu)
        {
            bilan.folders_unchanged += 1;
            return false;
        }
    }

    // 2. Relire le dossier. La date du dossier est prise AVANT la lecture :
    //    un rapport posé pendant la lecture changera la date, et le prochain
    //    passage relira.
    let Some((reel, meta_dossier)) = dossier_sur_disque(dossier, pistes, bilan) else {
        bilan.folders_missing += 1;
        return false;
    };
    bilan.folders_read += 1;
    let trouve = foo_dr::rapport_du_dossier(&reel);

    let mut restantes: Vec<i64> = Vec::new();
    let temoin = match trouve {
        Some((chemin_rapport, rapport)) => {
            bilan.reports_found += 1;
            for p in pistes {
                let numero = p.numero.or_else(|| {
                    Path::new(&p.chemin)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(foo_dr::numero_dans_le_nom)
                });
                match rapport.dr_pour_la_piste(numero, p.disque, p.titre.as_deref(), p.canaux) {
                    Some(dr) if ecrire_le_dr_du_rapport(repo, p.id, dr) => {
                        bilan.tracks_written += 1;
                    }
                    _ => restantes.push(p.id),
                }
            }
            bilan.stats += 1;
            let mtime = std::fs::metadata(&chemin_rapport)
                .ok()
                .and_then(|m| mtime_ns(&m));
            match (mtime, chemin_rapport.file_name().and_then(|n| n.to_str())) {
                (Some(mtime), Some(nom)) => Some(Temoin::Rapport {
                    nom: nom.to_string(),
                    mtime,
                    max_id,
                }),
                _ => None,
            }
        }
        None => {
            restantes.extend(pistes.iter().map(|p| p.id));
            mtime_ns(&meta_dossier).map(|mtime| Temoin::SansRapport { mtime, max_id })
        }
    };

    // 3. Mémoriser, sur la plus petite piste qui reste sans DR — c'est elle
    //    que la prochaine passe lira en tête du dossier.
    if let (Some(premiere), Some(t)) = (restantes.iter().min(), temoin) {
        let _ = repo.set(*premiere, CLE_TEMOIN, &t.encoder());
    }
    true
}

/// Une passe de rattrapage complète. Synchrone : disque et base, à lancer
/// hors des fils de l'exécuteur.
///
/// `forces` : dossiers à relire sans consulter la mémoire (signalés par le
/// surveillant de fichiers).
pub fn rattraper(backend: &Arc<dyn DbBackend>, forces: &HashSet<PathBuf>) -> Bilan {
    use super::{Tache, est_en_pause, priorite};

    let debut = Instant::now();
    let mut bilan = Bilan::default();
    let pistes = match lire_les_candidats(backend) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "rapports_dr_selection_echouee");
            return bilan;
        }
    };
    bilan.tracks_without_dr = pistes.len();

    let mut par_dossier: BTreeMap<PathBuf, Vec<Piste>> = BTreeMap::new();
    for p in pistes {
        if let Some(d) = Path::new(&p.chemin).parent() {
            par_dossier.entry(d.to_path_buf()).or_default().push(p);
        }
    }
    bilan.folders = par_dossier.len();

    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let (mut lus, mut inchanges) = (0usize, 0usize);
    for (dossier, pistes) in &par_dossier {
        // La pause de l'utilisateur, à la frontière du dossier.
        if est_en_pause(Tache::PlageDynamique) {
            bilan.interrupted = true;
            break;
        }
        let force = forces.contains(dossier);
        if traiter_le_dossier(&repo, dossier, pistes, force, &mut bilan) {
            lus += 1;
            if lus % CEDER_TOUS_LES_DOSSIERS_LUS == 0 {
                priorite::ceder_a_la_lecture_bloquant(Tache::PlageDynamique.id());
            }
        } else {
            inchanges += 1;
            if inchanges % CEDER_TOUS_LES_DOSSIERS_INCHANGES == 0 {
                priorite::ceder_a_la_lecture_bloquant(Tache::PlageDynamique.id());
            }
        }
    }
    bilan.duration_ms = debut.elapsed().as_millis() as u64;
    bilan
}

/// Lancer la passe et publier son bilan au relevé.
fn passe_publiee(backend: &Arc<dyn DbBackend>, forces: &HashSet<PathBuf>) -> Bilan {
    EN_COURS.store(true, Ordering::Relaxed);
    let bilan = rattraper(backend, forces);
    EN_COURS.store(false, Ordering::Relaxed);
    let fin = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    *DERNIER.lock().unwrap_or_else(|e| e.into_inner()) = Some((bilan.clone(), fin));
    tracing::info!(
        pistes_sans_dr = bilan.tracks_without_dr,
        dossiers = bilan.folders,
        inchanges = bilan.folders_unchanged,
        relus = bilan.folders_read,
        rapports = bilan.reports_found,
        stats = bilan.stats,
        ecrites = bilan.tracks_written,
        interrompue = bilan.interrupted,
        duree_ms = bilan.duration_ms,
        "rapports_dr_rattrapage"
    );
    bilan
}

/// La boucle de fond. Appelée une fois, au démarrage du serveur.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    use super::{CADENCE_RELECTURE_PAUSE, Tache, est_en_pause};

    tokio::spawn(async move {
        tokio::time::sleep(ATTENTE_AU_DEMARRAGE).await;
        loop {
            if est_en_pause(Tache::PlageDynamique) {
                tokio::time::sleep(CADENCE_RELECTURE_PAUSE).await;
                continue;
            }
            // Pas pendant un scan : il lit déjà les rapports des fichiers
            // qu'il touche, et la base est à lui.
            if crate::scanner::activite::scan_bibliotheque_en_cours() {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            let forces = prendre_les_signales();
            let b = backend.clone();
            let bilan = super::priorite::hors_du_fil_async(Tache::PlageDynamique.id(), move || {
                passe_publiee(&b, &forces)
            })
            .await;
            if bilan.is_some_and(|b| b.interrupted) {
                continue;
            }
            tokio::select! {
                _ = REVEIL.notified() => {
                    tokio::time::sleep(REGROUPEMENT).await;
                }
                _ = tokio::time::sleep(INTERVALLE) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_temoin_fait_l_aller_retour() {
        for t in [
            Temoin::Rapport {
                nom: "foo_dr.txt".into(),
                mtime: 1_758_000_000_123_456_789,
                max_id: 42,
            },
            Temoin::Rapport {
                nom: "journal; avec point-virgule.txt".into(),
                mtime: 1,
                max_id: 7,
            },
            Temoin::SansRapport {
                mtime: 99,
                max_id: 3,
            },
        ] {
            assert_eq!(Temoin::decoder(&t.encoder()), Some(t));
        }
        assert_eq!(Temoin::decoder(""), None);
        assert_eq!(Temoin::decoder("v0;d;1;2"), None);
        assert_eq!(Temoin::decoder("v1;r;1;2;"), None);
    }
}
