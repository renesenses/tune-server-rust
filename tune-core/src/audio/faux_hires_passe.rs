//! La PASSE de fond de la détection des faux hi-res (#6285).
//!
//! Elle décode un EXTRAIT de chaque piste locale sans perte (30 s, à 30 s du
//! début quand la piste est assez longue), le mesure
//! ([`super::faux_hires::mesurer`]) et range le verdict dans
//! `hires_verifications` (migration SQLite 129 / PG 093).
//!
//! # Désactivée par défaut
//!
//! Le réglage [`REGLAGE_ACTIF`] vaut « non » tant que personne ne l'a armé :
//! c'est une charge de fond nouvelle, sur toute la bibliothèque, et l'issue
//! demande de mesurer le taux de faux positifs sur une vraie bibliothèque
//! AVANT de montrer un badge « douteux ».
//!
//! # Les mêmes gardes que les autres passes qui décodent
//!
//! Relues à chaque tour, dans l'ordre de la plage dynamique
//! (`replaygain/plage_dynamique.rs`) :
//!
//! 1. la PAUSE de l'utilisateur ([`Tache::FauxHiRes`]) ;
//! 2. la GARDE THERMIQUE (`ThermalGate::should_hold`) ;
//! 3. la PRIORITÉ DE LA LECTURE (#4681) : une zone qui joue, et la passe cède ;
//! 4. le CRÉNEAU unique des analyses (`ANALYSIS_SLOT`, #1576) ;
//! 5. la LARGEUR des lots : `vitesse` × cœurs, ramenée à UN fichier à la fois
//!    quand la température est illisible ([`largeur_de_la_passe`], #5410 ;
//!    décision de Bertrand du 20/10/2026 sur #6450 : le bridage reste) ;
//! 6. le PÉRIMÈTRE des analyses (#5593) : les racines exclues ne sont ni
//!    choisies ni comptées.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::faux_hires::{Mesure, Verdict, juger, mesurer};
use crate::audio::thermal::{LectureThermique, ThermalGate, largeur_selon_la_chaleur};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::db::settings_repo::SettingsRepo;
use crate::taches_de_fond::{Tache, est_en_pause, priorite, vitesse};

/// Le réglage qui arme la passe (`settings`). Absent ou autre que `true` :
/// la passe dort.
pub const REGLAGE_ACTIF: &str = "analyse_faux_hires_active";

/// Durée de l'extrait décodé, en secondes.
pub const EXTRAIT_S: f64 = 30.0;

/// Les formats sans perte que la passe examine. Le DSD n'en fait pas partie :
/// ses deux signatures (bits nuls, coupure spectrale) n'y ont pas de sens.
pub const FORMATS_SANS_PERTE: [&str; 7] = ["flac", "wav", "aiff", "aif", "alac", "wv", "ape"];

fn ph(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    }
}

/// La passe est-elle armée ?
pub fn est_active(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(REGLAGE_ACTIF)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}

/// Arme ou désarme la passe.
pub fn activer(db: &Arc<dyn DbBackend>, actif: bool) -> Result<(), String> {
    SettingsRepo::with_backend(db.clone()).set(REGLAGE_ACTIF, if actif { "true" } else { "false" })
}

/// Le prédicat des candidats : pistes LOCALES sans perte, avec un chemin, pas
/// encore mesurées, dans le périmètre des analyses. Partagé par la sélection
/// et le compteur, comme celui des autres passes.
fn where_candidats(db: &Arc<dyn DbBackend>) -> String {
    let formats: Vec<String> = FORMATS_SANS_PERTE
        .iter()
        .map(|f| format!("'{f}'"))
        .collect();
    format!(
        "{local} AND t.file_path IS NOT NULL AND t.file_path <> '' \
         AND LOWER(COALESCE(t.format, '')) IN ({formats}) \
         AND NOT EXISTS (SELECT 1 FROM hires_verifications h WHERE h.track_id = t.id){perimetre}",
        local = crate::db::track_repo::sql::PISTE_LOCALE,
        formats = formats.join(", "),
        perimetre = crate::taches_de_fond::perimetre::clause_decodage(db),
    )
}

/// Combien de pistes restent à mesurer.
pub fn compter_les_candidats(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one(
        &format!(
            "SELECT COUNT(*) FROM tracks t WHERE {}",
            where_candidats(db)
        ),
        &[],
    )
    .ok()
    .flatten()
    .and_then(|r| r.first().and_then(|v| v.as_i64()))
    .unwrap_or(0)
}

/// Combien de fichiers un lot décode à la fois. La vitesse réglée des
/// analyses, et UN SEUL quand la température est illisible : c'est le
/// bridage de #5410, maintenu par la décision de Bertrand du 20/10/2026
/// (#6450).
///
/// #6713 — sauf si l'utilisateur a levé ce bridage
/// (`vitesse::bridage_leve`) : la même règle que les autres passes.
pub fn largeur_de_la_passe(db: &Arc<dyn DbBackend>, lecture: &LectureThermique) -> usize {
    largeur_selon_la_chaleur(
        vitesse::vitesse(db).largeur(vitesse::coeurs()),
        lecture,
        vitesse::bridage_leve(db),
    )
}

/// Une piste à mesurer.
#[derive(Debug, Clone)]
pub struct Candidat {
    pub track_id: i64,
    pub chemin: String,
    pub duree_ms: i64,
}

/// Les `n` prochains candidats.
pub fn prochains_candidats(db: &Arc<dyn DbBackend>, n: usize) -> Vec<Candidat> {
    let n = n as i64;
    db.query_many(
        &format!(
            "SELECT t.id, t.file_path, COALESCE(t.duration_ms, 0) FROM tracks t \
             WHERE {} ORDER BY t.id LIMIT {}",
            where_candidats(db),
            ph(db.engine(), 1)
        ),
        &[&n as &dyn ToSqlValue],
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|r| {
        Some(Candidat {
            track_id: r.first()?.as_i64()?,
            chemin: r.get(1)?.as_string()?,
            duree_ms: r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
        })
    })
    .collect()
}

/// Décode l'extrait d'un fichier, à sa fréquence et sa profondeur d'origine,
/// et le mesure. Bloquant : à appeler hors de l'exécuteur asynchrone.
pub fn mesurer_un_fichier(chemin: &str, duree_ms: i64) -> Result<Mesure, String> {
    // À 30 s du début quand la piste le permet : une introduction silencieuse
    // ou un fondu ne dit rien de la bande.
    let depart = if duree_ms >= ((EXTRAIT_S * 2.0 + 30.0) * 1000.0) as i64 {
        30.0
    } else {
        0.0
    };
    let pcm = crate::audio::decode::decode_to_pcm(chemin, None, None, depart, EXTRAIT_S)?;
    Ok(mesurer(
        &pcm.samples_i32,
        pcm.channels,
        pcm.sample_rate,
        pcm.bit_depth as u32,
    ))
}

/// Range le verdict d'une piste. Une piste illisible est rangée aussi
/// (`non_concluant`, raison `decodage_impossible`) : sinon elle serait
/// reprise à chaque lot, indéfiniment.
pub fn enregistrer(
    db: &Arc<dyn DbBackend>,
    track_id: i64,
    resultat: &Result<Mesure, String>,
) -> Result<(), String> {
    let e = db.engine();
    let (verdict, raison, coupure, bits, rate, bits_declares) = match resultat {
        Ok(m) => {
            let v = juger(m);
            (
                v.code().to_string(),
                v.code_raison().map(str::to_string),
                m.coupure_hz,
                m.bits_effectifs.map(|b| b as i64),
                Some(m.sample_rate as i64),
                Some(m.bits_declares as i64),
            )
        }
        Err(_) => (
            Verdict::NonConcluant.code().to_string(),
            Some("decodage_impossible".to_string()),
            None,
            None,
            None,
            None,
        ),
    };
    let sql = format!(
        "INSERT INTO hires_verifications \
         (track_id, verdict, raison, coupure_hz, bits_effectifs, sample_rate, bits_declares) \
         VALUES ({}, {}, {}, {}, {}, {}, {}) \
         ON CONFLICT (track_id) DO UPDATE SET verdict = excluded.verdict, \
         raison = excluded.raison, coupure_hz = excluded.coupure_hz, \
         bits_effectifs = excluded.bits_effectifs, sample_rate = excluded.sample_rate, \
         bits_declares = excluded.bits_declares",
        ph(e, 1),
        ph(e, 2),
        ph(e, 3),
        ph(e, 4),
        ph(e, 5),
        ph(e, 6),
        ph(e, 7)
    );
    db.execute(
        &sql,
        &[
            &track_id as &dyn ToSqlValue,
            &verdict,
            &raison,
            &coupure,
            &bits,
            &rate,
            &bits_declares,
        ],
    )
    .map(|_| ())
}

/// Vrai pendant qu'un lot décode : c'est ce que lit le relevé des tâches de
/// fond (`/system/background-tasks`).
static LOT_EN_COURS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Un lot décode-t-il en ce moment ?
pub fn lot_en_cours() -> bool {
    LOT_EN_COURS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Un lot : au plus `largeur` fichiers, décodés en parallèle hors de
/// l'exécuteur. Rend le nombre de pistes rangées.
pub async fn un_lot(db: &Arc<dyn DbBackend>, largeur: usize) -> usize {
    LOT_EN_COURS.store(true, std::sync::atomic::Ordering::Relaxed);
    let rangees = un_lot_sans_drapeau(db, largeur).await;
    LOT_EN_COURS.store(false, std::sync::atomic::Ordering::Relaxed);
    rangees
}

async fn un_lot_sans_drapeau(db: &Arc<dyn DbBackend>, largeur: usize) -> usize {
    let candidats = prochains_candidats(db, largeur.max(1));
    let mut taches = Vec::with_capacity(candidats.len());
    for c in candidats {
        taches.push(tokio::task::spawn_blocking(move || {
            let r = mesurer_un_fichier(&c.chemin, c.duree_ms);
            (c, r)
        }));
    }
    let mut rangees = 0;
    for t in taches {
        let Ok((c, r)) = t.await else { continue };
        if let Err(e) = &r {
            debug!(track_id = c.track_id, error = %e, "faux_hires_decodage_impossible");
        }
        match enregistrer(db, c.track_id, &r) {
            Ok(()) => rangees += 1,
            Err(e) => warn!(track_id = c.track_id, error = %e, "faux_hires_enregistrement_echoue"),
        }
    }
    rangees
}

/// La boucle de fond. Dort tant que la passe n'est pas armée ; sinon, un lot
/// par tour, sous les six gardes du module.
pub async fn courir(db: Arc<dyn DbBackend>) {
    let mut thermique = ThermalGate::new();
    loop {
        if !est_active(&db) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        if est_en_pause(Tache::FauxHiRes) {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        if thermique.should_hold("faux_hires") {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        if crate::audio::replaygain::any_zone_playing(&db) {
            priorite::noter_cedee(Tache::FauxHiRes.id());
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        let rangees = {
            let _creneau = crate::audio::replaygain::ANALYSIS_SLOT.lock().await;
            let largeur = largeur_de_la_passe(&db, &crate::audio::thermal::lecture_thermique());
            un_lot(&db, largeur).await
        };
        if rangees == 0 {
            // Plus rien à mesurer (ou rien de lisible) : on revient voir plus
            // tard, le prochain scan aura peut-être ajouté des pistes.
            tokio::time::sleep(Duration::from_secs(600)).await;
        } else {
            info!(rangees, "faux_hires_lot_range");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// Le verdict d'un ALBUM à partir de ceux de ses pistes : « douteux » dès
/// qu'une piste l'est (avec la raison la plus fréquente), « vérifié » si
/// toutes ses pistes mesurées le sont, « non analysé » sans aucune mesure,
/// « conforme » sinon (qualité CD, rien de suspect).
pub fn verdict_d_album(pistes: &[(String, Option<String>)]) -> (String, Option<String>) {
    if pistes.is_empty() {
        return ("non_analyse".into(), None);
    }
    let douteuses: Vec<&Option<String>> = pistes
        .iter()
        .filter(|(v, _)| v == "douteux")
        .map(|(_, r)| r)
        .collect();
    if !douteuses.is_empty() {
        let mut comptes: std::collections::BTreeMap<String, usize> = Default::default();
        for r in douteuses.into_iter().flatten() {
            *comptes.entry(r.clone()).or_default() += 1;
        }
        let raison = comptes.into_iter().max_by_key(|(_, n)| *n).map(|(r, _)| r);
        return ("douteux".into(), raison);
    }
    let mesurees: Vec<&String> = pistes
        .iter()
        .map(|(v, _)| v)
        .filter(|v| *v != "non_concluant")
        .collect();
    if mesurees.is_empty() {
        return ("non_analyse".into(), None);
    }
    if mesurees.iter().all(|v| *v == "verifie") {
        return ("verifie".into(), None);
    }
    ("conforme".into(), None)
}

#[cfg(test)]
#[path = "faux_hires_passe_tests_6285.rs"]
mod tests;
