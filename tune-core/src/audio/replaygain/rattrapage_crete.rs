//! Rattrapage des crêtes vraies mesurées par l'ancien algorithme (#2713).
//!
//! Jusqu'à #2713, `rg_track_true_peak` venait d'une interpolation Catmull-Rom
//! 4×, qui sous-estime la crête jusqu'à 1,1 dB à 12 kHz et 1,25 dB à 16 kHz
//! dans la pire phase. La mesure suit désormais l'annexe 2 de BS.1770
//! ([`crate::audio::crete_vraie`]). Les GAINS déjà calculés restent valides :
//! la sonie n'a pas changé d'un bit. Seul le pic est à refaire.
//!
//! # Version stockée
//!
//! Chaque crête vraie écrite par Tune porte sa version,
//! [`TRUE_PEAK_ALGO_KEY`] = [`TRUE_PEAK_ALGO`], et chaque crête d'album la
//! sienne, [`ALBUM_TRUE_PEAK_ALGO_KEY`]. La migration 121 (PG 085) étiquette
//! les valeurs existantes [`ANCIEN_TRUE_PEAK_ALGO`] sans rien effacer : une
//! crête Catmull-Rom vaut mieux que pas de crête du tout pour
//! `prevent_clipping` (elle majore toujours le pic d'échantillon), et elle
//! reste en service jusqu'à son remplacement.
//!
//! # Rattrapage progressif
//!
//! Le dernier rang de la cascade de fond ([`super::un_tour_de_cascade`]),
//! quand ReplayGain, empreintes et plage dynamique n'ont plus rien à faire :
//! [`TRACK_BATCH`](super::TRACK_BATCH) pistes par tour, avec les gardes de
//! la passe nominale — pause du ReplayGain, priorité à la lecture (#1310,
//! #2495, #4681), garde thermique, vitesse réglée. Chaque piste est décodée
//! une fois, pour sa seule crête ([`crate::audio::analyzer::mesurer_la_crete_vraie`]) ;
//! seules `rg_track_true_peak` et sa version sont écrites.
//!
//! Puis la passe d'albums refait la crête d'album des albums dont toutes les
//! pistes ont leur crête à jour ([`rafraichir_une_crete_d_album`]) : un
//! maximum, sans décodage.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::library::local_path::{
    LocalPath, deferral_stamp, deferral_threshold, resolve_local_path,
};

use super::{
    ALBUM_SOURCE_KEY, Issue, PATH_UNRESOLVED_KEY, SOURCE_ANALYSIS, SuitePiste, TRACK_BATCH,
    TRACK_SOURCE_KEY, any_zone_playing, delai_d_analyse, en_parallele_borne, format_peak,
    mesurer_en_cedant_a_la_lecture, now_epoch_secs,
};

/// La clé de `track_metadata` qui dit quel algorithme a produit
/// `rg_track_true_peak`.
pub const TRUE_PEAK_ALGO_KEY: &str = "rg_true_peak_algo";

/// La clé jumelle pour `rg_album_true_peak`.
pub const ALBUM_TRUE_PEAK_ALGO_KEY: &str = "rg_album_true_peak_algo";

/// La version courante de la crête vraie : ITU-R BS.1770 annexe 2, filtre de
/// Tune (sinus cardinal fenêtré, 16 prises par phase), 8× sous 88,2 kHz, 4×
/// au-delà. À changer dès qu'une crête rendue pour le même signal change.
pub const TRUE_PEAK_ALGO: &str = "bs1770-a2-fir-v1";

/// L'étiquette que la migration 121 pose sur les crêtes d'avant #2713.
pub const ANCIEN_TRUE_PEAK_ALGO: &str = "catmull-rom-4x";

/// Posée quand le rattrapage a essayé et n'a rien obtenu (fichier illisible,
/// muet, délai dépassé), avec la version qu'il visait : la piste n'est plus
/// reprise pour cette version, et garde son ancienne crête. Introuvable n'en
/// fait PAS partie : un partage démonté se reporte (#1865).
pub const TRUE_PEAK_ECHEC_KEY: &str = "rg_true_peak_echec";

/// Le prédicat des pistes dont la crête est à refaire : mesurées par Tune
/// (`rg_track_source = analysis`), sans crête de la version courante, pas déjà
/// essayées en vain pour cette version, pas reportées. Un paramètre : le
/// seuil de report.
///
/// Les gains lus dans les tags du fichier sont hors du prédicat : Tune ne les
/// a jamais décodés et n'a jamais écrit leur crête. Les pistes CUE aussi :
/// elles n'ont jamais de mesure de Tune (voir `analyze_track_batch`).
fn candidats_where(backend: &Arc<dyn DbBackend>) -> String {
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    format!(
        "t.file_path IS NOT NULL AND t.file_path != '' \
         AND EXISTS (SELECT 1 FROM track_metadata s WHERE s.track_id = t.id \
               AND s.key = '{TRACK_SOURCE_KEY}' AND s.value = '{SOURCE_ANALYSIS}') \
         AND NOT EXISTS (SELECT 1 FROM track_metadata v WHERE v.track_id = t.id \
               AND v.key = '{TRUE_PEAK_ALGO_KEY}' AND v.value = '{TRUE_PEAK_ALGO}') \
         AND NOT EXISTS (SELECT 1 FROM track_metadata e WHERE e.track_id = t.id \
               AND e.key = '{TRUE_PEAK_ECHEC_KEY}' AND e.value = '{TRUE_PEAK_ALGO}') \
         AND NOT EXISTS (SELECT 1 FROM track_metadata r WHERE r.track_id = t.id \
               AND r.key = '{PATH_UNRESOLVED_KEY}' AND r.value > ?){perimetre}"
    )
}

/// Combien de pistes ont une crête à refaire. Synchrone : hors des fils async.
/// `None` sur erreur de requête.
pub fn compter_les_cretes_a_refaire(backend: &Arc<dyn DbBackend>) -> Option<i64> {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    backend
        .query_one(
            &format!(
                "SELECT COUNT(*) FROM tracks t WHERE {}",
                candidats_where(backend)
            ),
            &[&seuil_report as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

/// Le curseur de la sélection, PAR BASE — même montage que celui de la passe
/// nominale (#5519) : un tour ne relit que ce qui suit la dernière piste
/// traitée ; quand plus rien ne le suit, on repart de 0 avant de conclure.
static CURSEURS: std::sync::Mutex<Vec<(std::sync::Weak<dyn DbBackend>, i64)>> =
    std::sync::Mutex::new(Vec::new());

fn curseur(backend: &Arc<dyn DbBackend>) -> i64 {
    let cible = Arc::downgrade(backend);
    let curseurs = CURSEURS.lock().unwrap_or_else(|e| e.into_inner());
    curseurs
        .iter()
        .find(|(base, _)| std::sync::Weak::ptr_eq(base, &cible))
        .map(|(_, c)| *c)
        .unwrap_or(0)
}

fn poser_curseur(backend: &Arc<dyn DbBackend>, valeur: i64) {
    let cible = Arc::downgrade(backend);
    let mut curseurs = CURSEURS.lock().unwrap_or_else(|e| e.into_inner());
    curseurs.retain(|(base, _)| base.strong_count() > 0);
    match curseurs
        .iter_mut()
        .find(|(base, _)| std::sync::Weak::ptr_eq(base, &cible))
    {
        Some((_, c)) => *c = valeur,
        None => curseurs.push((cible, valeur)),
    }
}

fn selectionner(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    backend.query_many(
        &format!(
            "SELECT t.id, t.file_path, t.duration_ms FROM tracks t \
             WHERE {} AND t.id > ? ORDER BY t.id LIMIT ?",
            candidats_where(backend)
        ),
        &[
            &seuil_report as &dyn ToSqlValue,
            &apres as &dyn ToSqlValue,
            &(TRACK_BATCH as i64) as &dyn ToSqlValue,
        ],
    )
}

/// Refaire la crête vraie d'un lot de pistes. Rend combien ont AVANCÉ
/// (0 ⇒ plus rien à faire, ou la passe a cédé avant le premier fichier).
///
/// Synchrone pour la base (sélection et écritures sur le pool bloquant), le
/// décodage part sur le pool bloquant segment par segment.
pub async fn rattraper_un_lot(backend: &Arc<dyn DbBackend>) -> usize {
    use crate::taches_de_fond::{Tache, est_en_pause, priorite};

    let apres = curseur(backend);
    let b = backend.clone();
    let rows = priorite::hors_du_fil_async(Tache::ReplayGain.id(), move || {
        let rows = selectionner(&b, apres)?;
        // Plus rien après le curseur : repartir du début une fois, avant de
        // conclure au repos (une piste a pu redevenir candidate derrière).
        if rows.is_empty() && apres > 0 {
            return selectionner(&b, 0).map(|r| (r, true));
        }
        Ok((rows, false))
    })
    .await;
    let rows = match rows {
        Some(Ok((rows, repart))) => {
            if repart {
                poser_curseur(backend, 0);
            }
            rows
        }
        Some(Err(e)) => {
            warn!(error = %e, "true_peak_rattrapage_requete_echec");
            return 0;
        }
        None => return 0,
    };
    if rows.is_empty() {
        debug!("true_peak_rattrapage_rien_a_faire");
        return 0;
    }
    if let Some(dernier) = rows
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .max()
    {
        poser_curseur(backend, dernier);
    }

    let mut faites = 0usize;
    let mut reportees = 0usize;
    let largeur = crate::taches_de_fond::vitesse::largeur_courante(backend);
    en_parallele_borne(
        largeur,
        rows.iter(),
        || {
            // Gardes relues AVANT CHAQUE fichier, comme la passe nominale.
            if est_en_pause(Tache::ReplayGain) {
                info!("true_peak_rattrapage_pause_utilisateur — arret a la frontiere de piste");
                return false;
            }
            if priorite::lecture_en_cours() || any_zone_playing(backend) {
                debug!("true_peak_rattrapage_cede_a_la_lecture");
                return false;
            }
            true
        },
        |r| rattraper_une_piste(backend, r),
        |suite| match suite {
            SuitePiste::Ignoree => true,
            SuitePiste::Avancee { reportee } => {
                faites += 1;
                if reportee {
                    reportees += 1;
                }
                true
            }
            SuitePiste::Cedee => false,
        },
    )
    .await;
    if faites > 0 {
        info!(
            refaites = faites - reportees,
            reportees,
            algo = TRUE_PEAK_ALGO,
            "true_peak_rattrapage_lot"
        );
    }
    faites
}

/// Ce que le rattrapage écrit pour UNE piste.
#[derive(Debug, Clone, PartialEq)]
enum Ecriture {
    /// La nouvelle crête, et sa version.
    Crete(f64),
    /// Essayé en vain pour cette version.
    Echec,
    /// Fichier introuvable : report daté (#1865).
    Report,
}

async fn rattraper_une_piste(
    backend: &Arc<dyn DbBackend>,
    r: &[crate::db::backend::SqlValue],
) -> SuitePiste {
    let Some(track_id) = r.first().and_then(|v| v.as_i64()) else {
        return SuitePiste::Ignoree;
    };
    let Some(path) = r
        .get(1)
        .and_then(|v| v.as_string())
        .filter(|p| !p.is_empty())
    else {
        return SuitePiste::Ignoree;
    };
    let ecriture = match resolve_local_path(&path) {
        LocalPath::Missing => {
            warn!(track_id, path = %path, "true_peak_path_unresolved — REPORTEE (#1865)");
            Ecriture::Report
        }
        LocalPath::Found(sur_disque) => {
            let delai = delai_d_analyse(r.get(2).and_then(|v| v.as_i64()));
            match mesurer_en_cedant_a_la_lecture(
                backend,
                delai,
                crate::audio::analyzer::mesurer_la_crete_vraie(&sur_disque),
            )
            .await
            {
                Issue::CedeeALaLecture => {
                    info!(
                        track_id,
                        "true_peak_rattrapage_cede_en_cours — repris plus tard"
                    );
                    return SuitePiste::Cedee;
                }
                Issue::Terminee(Ok(Some(crete))) => Ecriture::Crete(crete),
                Issue::Terminee(Ok(None)) if resolve_local_path(&path).is_missing() => {
                    Ecriture::Report
                }
                Issue::Terminee(Ok(None)) => {
                    debug!(track_id, path = %path, "true_peak_rattrapage_sans_mesure");
                    Ecriture::Echec
                }
                Issue::Terminee(Err(_)) => {
                    warn!(
                        track_id,
                        path = %path,
                        timeout_s = delai.as_secs(),
                        "true_peak_rattrapage_timeout"
                    );
                    Ecriture::Echec
                }
            }
        }
    };
    let reportee = ecriture == Ecriture::Report;
    let b = backend.clone();
    crate::taches_de_fond::priorite::hors_du_fil_async(
        crate::taches_de_fond::Tache::ReplayGain.id(),
        move || ecrire(&b, track_id, &ecriture),
    )
    .await;
    SuitePiste::Avancee { reportee }
}

/// Écrire l'issue d'une piste, en une transaction.
///
/// La crête ne s'écrit que si la mesure en place est TOUJOURS de Tune : une
/// remesure (#5882) a pu l'effacer pendant le décodage, et la passe nominale
/// la refera entière ; poser une crête seule sur une piste sans gain
/// mentirait sur sa provenance.
fn ecrire(backend: &Arc<dyn DbBackend>, track_id: i64, ecriture: &Ecriture) {
    const UPSERT: &str = "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?) \
                          ON CONFLICT (track_id, key) DO UPDATE SET value = excluded.value";
    let resultat = backend.write_tx(&mut |tx| {
        let poser = |cle: &str, valeur: &str| -> Result<(), String> {
            tx.execute(
                UPSERT,
                &[
                    &track_id as &dyn ToSqlValue,
                    &cle as &dyn ToSqlValue,
                    &valeur as &dyn ToSqlValue,
                ],
            )
            .map(|_| ())
        };
        match ecriture {
            Ecriture::Report => {
                poser(
                    PATH_UNRESOLVED_KEY,
                    &deferral_stamp(now_epoch_secs() as i64),
                )?;
            }
            Ecriture::Echec => {
                tx.execute(
                    "DELETE FROM track_metadata WHERE track_id = ? AND key = ?",
                    &[
                        &track_id as &dyn ToSqlValue,
                        &PATH_UNRESOLVED_KEY as &dyn ToSqlValue,
                    ],
                )?;
                poser(TRUE_PEAK_ECHEC_KEY, TRUE_PEAK_ALGO)?;
            }
            Ecriture::Crete(crete) => {
                tx.execute(
                    "DELETE FROM track_metadata WHERE track_id = ? AND key = ?",
                    &[
                        &track_id as &dyn ToSqlValue,
                        &PATH_UNRESOLVED_KEY as &dyn ToSqlValue,
                    ],
                )?;
                let source = tx
                    .query_one(
                        "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
                        &[
                            &track_id as &dyn ToSqlValue,
                            &TRACK_SOURCE_KEY as &dyn ToSqlValue,
                        ],
                    )?
                    .and_then(|r| r.first().and_then(|v| v.as_string()));
                if source.as_deref() == Some(SOURCE_ANALYSIS) {
                    poser("rg_track_true_peak", &format_peak(*crete))?;
                    poser(TRUE_PEAK_ALGO_KEY, TRUE_PEAK_ALGO)?;
                }
            }
        }
        Ok(())
    });
    if let Err(e) = resultat {
        warn!(track_id, error = %e, "true_peak_rattrapage_ecriture_echec");
    }
}

/// Refaire la crête vraie d'UN album dont toutes les pistes ont leur crête à
/// la version courante, mais dont la crête d'album ne l'est pas. Rend 1 si un
/// album a été traité, 0 sinon. Pure arithmétique : le maximum des crêtes de
/// piste. Synchrone, à appeler hors des fils async.
///
/// Écrite sur les seules pistes dont le gain d'album est de Tune
/// (`rg_album_source = analysis`), comme `analyze_album_batch` : un gain
/// d'album venu des tags n'a jamais reçu de crête d'album de Tune.
pub fn rafraichir_une_crete_d_album(backend: &Arc<dyn DbBackend>) -> usize {
    let album = backend
        .query_one(
            &format!(
                "SELECT t.album_id FROM tracks t \
                 JOIN track_metadata s ON s.track_id = t.id \
                      AND s.key = '{ALBUM_SOURCE_KEY}' AND s.value = '{SOURCE_ANALYSIS}' \
                 WHERE t.album_id IS NOT NULL \
                   AND NOT EXISTS (SELECT 1 FROM track_metadata v WHERE v.track_id = t.id \
                         AND v.key = '{ALBUM_TRUE_PEAK_ALGO_KEY}' AND v.value = '{TRUE_PEAK_ALGO}') \
                   AND NOT EXISTS (SELECT 1 FROM tracks t2 WHERE t2.album_id = t.album_id \
                         AND NOT EXISTS (SELECT 1 FROM track_metadata v2 \
                               WHERE v2.track_id = t2.id AND v2.key = '{TRUE_PEAK_ALGO_KEY}' \
                                 AND v2.value = '{TRUE_PEAK_ALGO}')) \
                 LIMIT 1"
            ),
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()));
    let Some(album_id) = album else {
        return 0;
    };
    let rows = match backend.query_many(
        &format!(
            "SELECT t.id, \
                    (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_true_peak'), \
                    (SELECT value FROM track_metadata WHERE track_id = t.id AND key = '{ALBUM_SOURCE_KEY}') \
             FROM tracks t WHERE t.album_id = ?"
        ),
        &[&album_id as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, album_id, "true_peak_album_requete_echec");
            return 0;
        }
    };
    let mut crete = 0.0f64;
    let mut ecrivables = Vec::new();
    for r in &rows {
        let Some(tid) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        if let Some(tp) = r
            .get(1)
            .and_then(|v| v.as_string())
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            crete = crete.max(tp);
        }
        if r.get(2).and_then(|v| v.as_string()).as_deref() == Some(SOURCE_ANALYSIS) {
            ecrivables.push(tid);
        }
    }
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let valeur = format_peak(crete);
    for tid in &ecrivables {
        if crete > 0.0 {
            let _ = repo.set(*tid, "rg_album_true_peak", &valeur);
        }
        // Posée même sans crête : sinon l'album serait rechoisi à chaque
        // tour et affamerait les autres.
        let _ = repo.set(*tid, ALBUM_TRUE_PEAK_ALGO_KEY, TRUE_PEAK_ALGO);
    }
    debug!(album_id, crete = %valeur, pistes = ecrivables.len(), "true_peak_album_refait");
    1
}

#[cfg(test)]
#[path = "rattrapage_crete_tests.rs"]
mod tests;
