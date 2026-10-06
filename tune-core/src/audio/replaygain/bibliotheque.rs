//! Où en est la BIBLIOTHÈQUE pour le ReplayGain — #5597.
//!
//! La jauge de l'écran État du serveur lisait seulement la campagne en cours
//! ([`super::progression`]) : `traitees = 0`, `total = restants` à chaque
//! ouverture. Après un redémarrage, elle affichait donc « 0 piste analysée
//! sur ce qui reste », alors que les mesures déjà faites sont en base
//! (`rg_analyzed`, `rg_track_gain` dans `track_metadata`). Un testeur l'a lu
//! comme « 4 heures et 13 485 analyses perdues » (Tades, fil 2058 et 2083).
//!
//! Ce module donne le couple qui parle de la bibliothèque :
//! - `eligibles` : pistes qui ont un fichier (`file_path` non vide), la même
//!   population que [`super::compter_les_candidats_replaygain`] filtre ;
//! - `analysees` : parmi elles, celles qui portent `rg_analyzed` ou
//!   `rg_track_gain` — exactement les deux témoins qui font sortir une piste
//!   des candidats.
//!
//! Donc `eligibles - analysees` = candidats + pistes reportées pas encore
//! mesurées. Le couple ne dépend d'aucun état de processus : il survit à un
//! redémarrage.
//!
//! ## Pourquoi un cache
//!
//! Le comptage parcourt toute la table `tracks` (un testeur en a 528 000), avec
//! une recherche par clé primaire de `track_metadata` par piste. L'écran sonde
//! la route en boucle : recompter à chaque sondage ferait payer ce parcours
//! toutes les quelques secondes pour un chiffre qui avance d'une trentaine de
//! pistes par minute. [`CacheBibliothequeReplayGain`] le garde
//! [`DUREE_DU_CACHE`] secondes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::backend::DbBackend;

/// Durée pendant laquelle un comptage de la bibliothèque est resservi tel quel.
///
/// Une minute : à environ 1 300 à 1 900 pistes par heure (#5519), la jauge
/// prend au plus une trentaine de pistes de retard, invisible à l'échelle d'une
/// bibliothèque de plusieurs centaines de milliers de titres.
pub const DUREE_DU_CACHE: Duration = Duration::from_secs(60);

/// Le couple « analysées / éligibles » de la bibliothèque entière.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BibliothequeReplayGain {
    /// Pistes avec un fichier qui portent `rg_analyzed` ou `rg_track_gain`.
    pub analysees: i64,
    /// Pistes avec un fichier : celles que la passe peut analyser.
    pub eligibles: i64,
}

/// Compte la bibliothèque, SANS cache. `None` sur erreur de requête : une
/// jauge sans chiffre vaut mieux qu'un « 0 sur 0 » qui se lirait comme une
/// bibliothèque vide.
///
/// Une seule passe sur `tracks`. `COUNT(CASE … END)` plutôt que `SUM` : il
/// rend 0 (et pas `NULL`) sur une table vide, et un entier sur les deux
/// moteurs.
pub fn compter_la_bibliotheque_replaygain(
    backend: &Arc<dyn DbBackend>,
) -> Option<BibliothequeReplayGain> {
    // #5593 — dans le PÉRIMÈTRE réglé : une racine exclue sort des éligibles
    // comme des candidats, sinon `eligibles - analysees` annoncerait pour
    // toujours un reste que la passe ne fera jamais.
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    let ligne = backend
        .query_one(
            &format!(
                "SELECT COUNT(*), \
                   COUNT(CASE WHEN EXISTS (SELECT 1 FROM track_metadata m \
                         WHERE m.track_id = t.id AND m.key IN ('rg_analyzed', 'rg_track_gain')) \
                       THEN 1 END) \
                 FROM tracks t WHERE t.file_path IS NOT NULL AND t.file_path != ''{perimetre}"
            ),
            &[],
        )
        .ok()
        .flatten()?;
    let eligibles = ligne.first().and_then(|v| v.as_i64())?;
    let analysees = ligne.get(1).and_then(|v| v.as_i64())?;
    Some(BibliothequeReplayGain {
        analysees: analysees.clamp(0, eligibles.max(0)),
        eligibles: eligibles.max(0),
    })
}

/// Le dernier comptage, resservi pendant [`DUREE_DU_CACHE`].
///
/// Tenu dans l'état du serveur et non en `static` : un test ne pollue pas le
/// suivant (même montage que `PasseDr`).
#[derive(Debug, Default)]
pub struct CacheBibliothequeReplayGain {
    dernier: Mutex<Option<(Instant, BibliothequeReplayGain)>>,
}

impl CacheBibliothequeReplayGain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Le couple de la bibliothèque, recompté au plus une fois par
    /// [`DUREE_DU_CACHE`].
    pub fn lire(&self, backend: &Arc<dyn DbBackend>) -> Option<BibliothequeReplayGain> {
        self.lire_a(backend, Instant::now(), DUREE_DU_CACHE)
    }

    fn lire_a(
        &self,
        backend: &Arc<dyn DbBackend>,
        maintenant: Instant,
        duree: Duration,
    ) -> Option<BibliothequeReplayGain> {
        if let Ok(dernier) = self.dernier.lock()
            && let Some((quand, valeur)) = *dernier
            && maintenant.saturating_duration_since(quand) < duree
        {
            return Some(valeur);
        }
        // Compté HORS du verrou : deux sondages simultanés peuvent compter
        // deux fois, mais aucun ne reste bloqué derrière le `COUNT` de l'autre.
        let valeur = compter_la_bibliotheque_replaygain(backend)?;
        if let Ok(mut dernier) = self.dernier.lock() {
            *dernier = Some((maintenant, valeur));
        }
        Some(valeur)
    }
}

#[cfg(test)]
mod tests_5597 {
    use super::*;
    use crate::db::backend::ToSqlValue;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Bjork')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Homogenic', 1)",
            &[],
        )
        .unwrap();
        for id in 1..=5 {
            db.execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                     sample_rate, channels) VALUES ({id}, 'Piste {id}', 1, 1, \
                     '/media/music/{id}.flac', 300000, 44100, 2)"
                ),
                &[],
            )
            .unwrap();
        }
        // Une piste CUE (sans fichier propre) : hors de la population, comme
        // pour le balayage.
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms) \
             VALUES (6, 'Piste CUE', 1, 1, NULL, 300000)",
            &[],
        )
        .unwrap();
        Arc::new(db)
    }

    fn poser(backend: &Arc<dyn DbBackend>, track_id: i64, cle: &str) {
        backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, '1')",
                &[&track_id as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
            )
            .unwrap();
    }

    /// 🔴 Le cœur de #5597 : le couple compte ce qui est EN BASE, pas ce que la
    /// campagne en cours a fait. Une piste analysée avant le redémarrage reste
    /// comptée, et la somme analysées + candidats retombe sur les éligibles.
    #[test]
    fn compte_les_pistes_deja_analysees_en_base() {
        let backend = base();
        assert_eq!(
            compter_la_bibliotheque_replaygain(&backend),
            Some(BibliothequeReplayGain {
                analysees: 0,
                eligibles: 5
            })
        );
        poser(&backend, 1, "rg_analyzed");
        poser(&backend, 2, "rg_track_gain");
        // Les deux témoins sur la même piste : comptée une fois.
        poser(&backend, 3, "rg_analyzed");
        poser(&backend, 3, "rg_track_gain");
        // Un autre témoin n'est pas une analyse ReplayGain.
        poser(&backend, 4, "dr_track");
        // La piste CUE porte un gain lu dans les tags, mais n'est pas éligible.
        poser(&backend, 6, "rg_track_gain");

        let b = compter_la_bibliotheque_replaygain(&backend).unwrap();
        assert_eq!(
            b,
            BibliothequeReplayGain {
                analysees: 3,
                eligibles: 5
            }
        );
        let candidats = super::super::compter_les_candidats_replaygain(&backend);
        assert_eq!(
            b.analysees + candidats,
            b.eligibles,
            "analysées + candidats doit retomber sur les éligibles (aucun report ici)"
        );
    }

    /// #5593 — une racine exclue des analyses sort du couple, des analysées
    /// comme des éligibles ; la somme analysées + candidats retombe toujours
    /// sur les éligibles.
    #[test]
    fn le_couple_suit_le_perimetre_regle() {
        let backend = base();
        poser(&backend, 1, "rg_analyzed");
        backend
            .execute(
                "UPDATE tracks SET file_path = '/mnt/nas/' || id || '.flac' WHERE id IN (1, 2)",
                &[],
            )
            .unwrap();
        crate::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set(
                crate::taches_de_fond::perimetre::CLE_RACINES_EXCLUES,
                r#"["/mnt/nas"]"#,
            )
            .unwrap();
        let b = compter_la_bibliotheque_replaygain(&backend).unwrap();
        assert_eq!(
            b,
            BibliothequeReplayGain {
                analysees: 0,
                eligibles: 3
            },
            "les pistes 1 et 2 sont sous la racine exclue"
        );
        let candidats = super::super::compter_les_candidats_replaygain(&backend);
        assert_eq!(b.analysees + candidats, b.eligibles);
    }

    #[test]
    fn le_cache_resert_puis_recompte_apres_sa_duree() {
        let backend = base();
        let cache = CacheBibliothequeReplayGain::new();
        let t0 = Instant::now();
        let duree = Duration::from_secs(60);
        assert_eq!(cache.lire_a(&backend, t0, duree).unwrap().analysees, 0);

        poser(&backend, 1, "rg_analyzed");
        assert_eq!(
            cache
                .lire_a(&backend, t0 + Duration::from_secs(30), duree)
                .unwrap()
                .analysees,
            0,
            "dans la minute, le comptage est resservi sans requête"
        );
        assert_eq!(
            cache
                .lire_a(&backend, t0 + Duration::from_secs(61), duree)
                .unwrap()
                .analysees,
            1,
            "passé la minute, on recompte"
        );
    }
}
