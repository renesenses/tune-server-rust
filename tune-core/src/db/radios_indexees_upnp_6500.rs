//! #6500 — retirer de `tracks` les stations de radio qu'une indexation UPnP y
//! avait rangées comme des pistes.
//!
//! # Le défaut
//!
//! La synchronisation d'une source UPnP (`routes/indexation_upnp.rs`,
//! `recolter`) descendait dans tous les rayons du serveur parcouru, y compris
//! le rayon Radio d'un autre Tune, et retenait chaque `<item>` doté d'un titre
//! et d'une URL. Relevé sur le `.18` le 19/10/2026 : **31 stations** en tête de
//! Bibliothèque > Pistes, toutes `source = 'upnp'`, sans artiste, sans album,
//! durée nulle, `upnp_object_id = radio/<n>` et
//! `upnp_res_url = http://<autre Tune>/api/v1/radios/<n>/audio.wav`.
//!
//! La récolte les écarte désormais. Mais celles qui sont déjà en base n'en
//! sortent pas seules : la synchronisation ne retire que ce qu'une passe
//! COMPLÈTE ne revoit plus, et la source du `.18` n'est plus joignable.
//!
//! # Ce que la passe retire, et rien d'autre
//!
//! Une ligne n'est retirée que si les QUATRE conditions tiennent :
//!
//! 1. `source = 'upnp'` ;
//! 2. `file_path IS NULL` (jamais une piste locale adoptée) ;
//! 3. son instantané porte `upnp_object_id = 'radio/…'` ;
//! 4. et son URL de lecture passe par la route des radios d'un Tune,
//!    `…/api/v1/radios/<n>/audio…`.
//!
//! (3) et (4) sont la signature d'une station du serveur média de Tune ; une
//! vraie piste UPnP porte `track/<n>` et `…/library/tracks/<n>/audio`. Sept
//! vraies pistes du `.18` ont, elles aussi, une durée nulle : la durée n'est
//! donc PAS un critère.
//!
//! Une ligne qu'un favori (`item_type = 'track'`) ou une playlist référence est
//! **laissée** et comptée : la passe ne supprime jamais ce que l'utilisateur a
//! rangé lui-même (doctrine de `synchronisation_upnp::remove_missing`).
//!
//! Idempotente : sur une base déjà passée, une lecture et rien d'écrit.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};

/// Les candidates : les quatre conditions de l'en-tête.
const CANDIDATES: &str = "SELECT t.id FROM tracks t \
     WHERE t.source = 'upnp' AND t.file_path IS NULL \
       AND EXISTS (SELECT 1 FROM track_metadata m \
                    WHERE m.track_id = t.id AND m.key = 'upnp_object_id' \
                      AND m.value LIKE 'radio/%') \
       AND EXISTS (SELECT 1 FROM track_metadata m \
                    WHERE m.track_id = t.id AND m.key = 'upnp_res_url' \
                      AND m.value LIKE '%/api/v1/radios/%/audio%')";

/// Une candidate qu'un favori de piste ou une playlist référence.
const LIEE: &str = "SELECT 1 FROM tracks t WHERE t.id = ? AND ( \
       EXISTS (SELECT 1 FROM playlist_tracks pt WHERE pt.track_id = t.id) \
       OR EXISTS (SELECT 1 FROM favorites f \
                   WHERE f.item_type = 'track' AND f.item_id = CAST(t.id AS TEXT)))";

/// Ce que la passe a fait.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Stations retirées de `tracks`.
    pub retirees: usize,
    /// Stations laissées parce qu'un favori ou une playlist les référence.
    pub gardees_liees: Vec<i64>,
}

/// Retire les stations indexées comme des pistes. Voir l'en-tête du module.
pub fn retirer_les_radios_indexees(db: &Arc<dyn DbBackend>) -> Result<Bilan, String> {
    let ids: Vec<i64> = db
        .query_many(CANDIDATES, &[])?
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let mut bilan = Bilan::default();
    if ids.is_empty() {
        return Ok(bilan);
    }
    db.write_tx(&mut |tx| {
        // Relu dans la transaction : un favori a pu être posé depuis la lecture.
        let (mut retirees, mut gardees) = (0usize, Vec::new());
        for id in &ids {
            let p: [&dyn ToSqlValue; 1] = [id];
            if tx.query_one(LIEE, &p)?.is_some() {
                gardees.push(*id);
                continue;
            }
            tx.execute("DELETE FROM upnp_library_members WHERE track_id = ?", &p)?;
            retirees += tx.execute(
                "DELETE FROM tracks WHERE id = ? AND source = 'upnp' AND file_path IS NULL",
                &p,
            )?;
        }
        bilan = Bilan {
            retirees,
            gardees_liees: gardees,
        };
        Ok(())
    })?;
    Ok(bilan)
}

/// La passe du démarrage : journalise, n'interrompt jamais le démarrage.
pub fn passe_journalisee(db: &Arc<dyn DbBackend>) {
    match retirer_les_radios_indexees(db) {
        Ok(b) if b.retirees == 0 && b.gardees_liees.is_empty() => {}
        Ok(b) => tracing::info!(
            retirees = b.retirees,
            gardees_liees = ?b.gardees_liees,
            "radios_indexees_upnp_6500_retirees_des_pistes"
        ),
        Err(e) => tracing::warn!(error = %e, "radios_indexees_upnp_6500_passe_echouee"),
    }
}

#[cfg(test)]
#[path = "radios_indexees_upnp_6500_tests.rs"]
mod tests;
