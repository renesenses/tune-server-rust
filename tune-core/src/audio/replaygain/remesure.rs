//! Refaire, sur demande, les mesures ReplayGain prises avant le correctif du
//! vrai pic (#5882).
//!
//! L'analyse découpe la piste en segments de 30 s, chacun décodé par un seek.
//! Avant #5882, `decode_symphonia` ne rognait pas le résidu entre le début du
//! paquet atteint et l'échantillon demandé : chaque jonction rejouait la fin
//! du segment précédent. Le suréchantillonnage 4× lisait ce saut de phase
//! comme un over (0,507 au lieu de 0,456 pour le même signal en FLAC et en
//! WAV, mesuré sur Shrek). Le correctif ne touche que les mesures FUTURES :
//! celles déjà en base restent fausses, et la passe ne les reprend jamais,
//! puisqu'une piste qui porte `rg_track_gain` ou `rg_analyzed` n'est plus
//! candidate.
//!
//! Ce module les rend à la passe, sur demande de l'utilisateur
//! (`POST /system/replaygain/reanalyze`). Il ne mesure rien et n'écrit dans
//! aucun fichier audio : il efface en base les valeurs périmées, et la passe
//! nominale les recalcule à son rythme, avec ses gardes (lecture, chaleur,
//! pause, vitesse, périmètre).
//!
//! ## Quelles mesures
//!
//! Celles que Tune a produites (`rg_track_source = analysis`) et qui ne
//! portent pas la version courante [`super::RG_ALGO`]. Une mesure d'avant
//! #5594 n'a pas de version ; une mesure `bs1770-tp4x-v1` a pu être écrite par
//! une construction sans #5882, puisque la branche de #5594 ne le contenait
//! pas. La version est montée à `v2` avec ce module : seule `v2` est sûre.
//!
//! Un gain lu dans les tags du fichier n'est jamais touché. Une piste dont le
//! fichier ne répond pas n'est pas remise à mesurer : la passe la reporterait
//! sans gain jusqu'au retour du disque, et l'utilisateur perdrait une valeur
//! approximative pour aucune. Une piste d'une racine exclue des analyses
//! (#5593) non plus, pour la même raison.
//!
//! ## Par lots, en suivant la passe
//!
//! Effacer d'un coup les mesures de toute une bibliothèque laisserait des
//! dizaines de milliers de pistes sans gain pendant des jours. La campagne
//! efface donc par lots de [`LOT`] pistes, et seulement quand la passe a
//! presque rattrapé le lot précédent ([`SEUIL_DE_RELANCE`] candidates au
//! plus). Au pire, quelques centaines de pistes jouent sans gain à un instant
//! donné. Tout le travail en base part sur le pool bloquant.
//!
//! La demande est notée dans les réglages ([`DEMANDE_KEY`]) : un redémarrage
//! en pleine campagne la reprend ([`reprendre_si_demandee`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::library::local_path::{LocalPath, resolve_local_path};
use crate::taches_de_fond::Tache;

use super::{RG_ALGO, RG_ALGO_KEY, SOURCE_ANALYSIS, TRACK_SOURCE_KEY};

/// La demande en cours, notée dans `settings` pour survivre à un redémarrage.
/// Effacée quand la campagne a fait le tour de la bibliothèque.
pub const DEMANDE_KEY: &str = "replaygain_remesure_demandee";

/// Pistes rendues à la passe d'un coup.
pub const LOT: usize = 200;

/// La campagne attend que la passe n'ait plus que ce nombre de candidates
/// avant d'en effacer d'autres.
pub const SEUIL_DE_RELANCE: i64 = 50;

/// Délai entre deux regards de la campagne sur la passe. Le comptage des
/// candidates parcourt `tracks` : une fois par minute, pas plus.
const ATTENTE: Duration = Duration::from_secs(60);

/// Les clés de piste effacées : la mesure, sa provenance, ses versions (celle
/// de la crête vraie et sa marque d'échec, #2713, comprises) et le témoin qui
/// ferait sauter la piste à la passe.
const CLES_DE_PISTE: &str = "'rg_track_gain', 'rg_track_peak', 'rg_track_true_peak', \
     'rg_track_source', 'rg_algo', 'rg_true_peak_algo', 'rg_true_peak_echec', 'rg_analyzed'";

/// Les clés d'album effacées, sur les seules pistes où l'album est de Tune
/// (`rg_album_source = analysis`). Le gain d'album se calcule sur les gains de
/// piste : sans cela, il garderait la valeur faite des anciennes mesures. La
/// passe d'albums le refait quand toutes les pistes de l'album ont de nouveau
/// leur gain.
const CLES_D_ALBUM: &str = "'rg_album_gain', 'rg_album_peak', 'rg_album_true_peak', \
     'rg_album_true_peak_algo', 'rg_album_source'";

/// Une campagne tourne dans ce processus.
static EN_COURS: AtomicBool = AtomicBool::new(false);

/// Le prédicat des mesures périmées, sur `tracks t`, périmètre compris. Un
/// seul texte pour le comptage et pour la sélection.
fn perimees_where(backend: &Arc<dyn DbBackend>) -> String {
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    format!(
        "t.file_path IS NOT NULL AND t.file_path != '' \
         AND EXISTS (SELECT 1 FROM track_metadata s WHERE s.track_id = t.id \
               AND s.key = '{TRACK_SOURCE_KEY}' AND s.value = '{SOURCE_ANALYSIS}') \
         AND NOT EXISTS (SELECT 1 FROM track_metadata v WHERE v.track_id = t.id \
               AND v.key = '{RG_ALGO_KEY}' AND v.value = '{RG_ALGO}'){perimetre}"
    )
}

/// Combien de pistes portent une mesure de Tune d'avant la version courante.
/// Synchrone : à appeler hors des fils async. `None` sur erreur de requête.
pub fn compter_les_mesures_perimees(backend: &Arc<dyn DbBackend>) -> Option<i64> {
    backend
        .query_one(
            &format!(
                "SELECT COUNT(*) FROM tracks t WHERE {}",
                perimees_where(backend)
            ),
            &[],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

/// Ce qu'un lot a fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lot {
    /// Pistes examinées (identifiant au-delà du curseur).
    pub examinees: usize,
    /// Pistes rendues à la passe.
    pub remises: usize,
    /// Le plus grand identifiant examiné : le curseur du lot suivant.
    pub curseur: i64,
}

/// Rendre à la passe au plus `taille` pistes périmées d'identifiant supérieur
/// à `apres`. Synchrone : à appeler hors des fils async.
///
/// Les pistes dont le fichier ne répond pas sont examinées mais gardent leur
/// mesure. C'est pourquoi la campagne avance par curseur : sans lui, elles
/// ressortiraient en tête de chaque lot.
pub fn remettre_un_lot(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
    taille: usize,
) -> Result<Lot, String> {
    let rows = backend.query_many(
        &format!(
            "SELECT t.id, t.file_path FROM tracks t WHERE t.id > ? AND {} \
             ORDER BY t.id LIMIT ?",
            perimees_where(backend)
        ),
        &[
            &apres as &dyn ToSqlValue,
            &(taille as i64) as &dyn ToSqlValue,
        ],
    )?;
    let mut curseur = apres;
    let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        curseur = curseur.max(id);
        let chemin = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        if matches!(resolve_local_path(&chemin), LocalPath::Found(_)) {
            ids.push(id);
        }
    }
    if !ids.is_empty() {
        // Des entiers lus en base : la liste en clair ne porte aucun texte.
        let liste = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        // L'album d'abord : sa sélection lit les pistes de l'album, pas les
        // clés de piste effacées ensuite.
        backend.execute(
            &format!(
                "DELETE FROM track_metadata WHERE key IN ({CLES_D_ALBUM}) \
                 AND track_id IN (SELECT a.track_id FROM track_metadata a \
                       WHERE a.key = 'rg_album_source' AND a.value = '{SOURCE_ANALYSIS}') \
                 AND track_id IN (SELECT t2.id FROM tracks t2 WHERE t2.album_id IN \
                       (SELECT t.album_id FROM tracks t WHERE t.id IN ({liste}) \
                        AND t.album_id IS NOT NULL))"
            ),
            &[],
        )?;
        backend.execute(
            &format!(
                "DELETE FROM track_metadata WHERE track_id IN ({liste}) \
                 AND key IN ({CLES_DE_PISTE})"
            ),
            &[],
        )?;
    }
    Ok(Lot {
        examinees: rows.len(),
        remises: ids.len(),
        curseur,
    })
}

/// Une campagne tourne-t-elle dans ce processus ?
pub fn en_cours() -> bool {
    EN_COURS.load(Ordering::SeqCst)
}

/// Ce que rend [`demander`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demande {
    /// La campagne part.
    Lancee,
    /// Une campagne tourne déjà.
    DejaEnCours,
}

/// Noter la demande et lancer la campagne en tâche de fond. Le comptage et le
/// refus « rien à faire » sont à la charge de l'appelant.
pub fn demander(backend: &Arc<dyn DbBackend>) -> Result<Demande, String> {
    if en_cours() {
        return Ok(Demande::DejaEnCours);
    }
    SettingsRepo::with_backend(backend.clone()).set(DEMANDE_KEY, "1")?;
    Ok(if lancer(backend.clone()) {
        Demande::Lancee
    } else {
        Demande::DejaEnCours
    })
}

/// Au démarrage : reprendre une campagne interrompue par un arrêt du serveur.
pub fn reprendre_si_demandee(backend: Arc<dyn DbBackend>) {
    let demandee = SettingsRepo::with_backend(backend.clone())
        .get(DEMANDE_KEY)
        .ok()
        .flatten()
        .is_some();
    if demandee {
        info!("replaygain_remesure_reprise — campagne interrompue par un arret, reprise");
        lancer(backend);
    }
}

/// Lancer la boucle si aucune ne tourne. `false` si une tourne déjà.
fn lancer(backend: Arc<dyn DbBackend>) -> bool {
    if EN_COURS.swap(true, Ordering::SeqCst) {
        return false;
    }
    tokio::spawn(async move {
        campagne(&backend).await;
        EN_COURS.store(false, Ordering::SeqCst);
    });
    true
}

/// Faut-il attendre avant d'effacer le lot suivant ? Fonction à part pour
/// que la règle se teste sans boucle ni horloge.
pub(crate) fn doit_attendre(analyse_armee: bool, en_pause: bool, candidates: Option<i64>) -> bool {
    // Analyse coupée ou suspendue : rien ne remesurerait, on n'efface rien.
    // Comptage en échec : on ne sait pas où en est la passe, on attend.
    !analyse_armee || en_pause || candidates.is_none_or(|n| n > SEUIL_DE_RELANCE)
}

async fn campagne(backend: &Arc<dyn DbBackend>) {
    let id = Tache::ReplayGain.id();
    let mut curseur = 0i64;
    let mut remises = 0usize;
    loop {
        let b = backend.clone();
        let candidates = crate::taches_de_fond::priorite::hors_du_fil_async(id, move || {
            super::compter_les_candidats_replaygain(&b)
        })
        .await;
        if doit_attendre(
            super::analysis_enabled(backend),
            crate::taches_de_fond::est_en_pause(Tache::ReplayGain),
            candidates,
        ) {
            tokio::time::sleep(ATTENTE).await;
            continue;
        }
        let b = backend.clone();
        let lot = crate::taches_de_fond::priorite::hors_du_fil_async(id, move || {
            remettre_un_lot(&b, curseur, LOT)
        })
        .await;
        match lot {
            Some(Ok(l)) if l.examinees == 0 => break,
            Some(Ok(l)) => {
                curseur = l.curseur;
                remises += l.remises;
                info!(
                    remises = l.remises,
                    examinees = l.examinees,
                    curseur,
                    "replaygain_remesure_lot — mesures d'avant {RG_ALGO} rendues a la passe"
                );
            }
            Some(Err(e)) => {
                warn!(error = %e, "replaygain_remesure_lot_echec");
                tokio::time::sleep(ATTENTE).await;
            }
            None => tokio::time::sleep(ATTENTE).await,
        }
    }
    let b = backend.clone();
    let _ = crate::taches_de_fond::priorite::hors_du_fil_async(id, move || {
        SettingsRepo::with_backend(b).delete(DEMANDE_KEY)
    })
    .await;
    info!(
        remises,
        "replaygain_remesure_terminee — toute la bibliotheque a ete parcourue"
    );
}

#[cfg(test)]
#[path = "remesure_tests.rs"]
mod tests;
