//! LES SOURCES D'UN ARTISTE, calculées par le serveur — tune-web-client#1716.
//!
//! ## Ce qu'on corrige
//!
//! L'onglet Bibliothèque › Artistes du client web chargeait TOUTES les pistes
//! (`GET /library/tracks`, par pages de 5 000) et TOUS les albums, pour une
//! seule chose : savoir de quelles sources vient chaque artiste (le filtre et
//! les comptes du menu Source). Sur une base de 95 925 pistes, vingt pages
//! d'environ 4 Mo de JSON à chaque génération de la bibliothèque.
//!
//! ## Ce que rend ce module
//!
//! Pour une page d'artistes, la clé de provenance de chacun de leurs albums
//! et de chacune de leurs pistes — `local`, `upnp:<UDN>`, `upnp`, `qobuz`… —,
//! avec les effectifs. La clé est celle de `provenanceDe` côté client
//! ([`expression_provenance_de`]). Les albums et les pistes comptés sont ceux
//! des LISTES que le client lisait : albums visibles (masqués et distants
//! doublés par un local exclus, comme `GET /library/albums`), pistes du socle
//! de la vue (comme `GET /library/tracks`). L'appartenance est celle du
//! client : un artiste est d'une source s'il y a un album à son nom OU une
//! piste à son nom (compilations).

use std::collections::{BTreeMap, HashMap};

use super::ArtistRepo;
use crate::TuneError;
use crate::db::engine::Engine;
use crate::db::facet_filter::{
    album_distant_double_exclu, hidden_albums_excluded, hidden_tracks_excluded,
    sql_pistes_ecartees_par_le_socle,
};
use crate::db::track_repo::expression_provenance_de;

/// Ce qu'un artiste a dans UNE source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PresenceDansUneSource {
    pub albums: i64,
    pub pistes: i64,
}

/// Identifiant d'artiste → clé de provenance → présence. Un artiste sans
/// album ni piste visible est absent de la table.
pub type SourcesDesArtistes = HashMap<i64, BTreeMap<String, PresenceDansUneSource>>;

/// Les albums visibles de la liste `ids`, par artiste et par provenance.
pub(super) fn sql_albums(engine: Engine, ids: &str) -> String {
    format!(
        "SELECT a.artist_id, {prov}, COUNT(*) FROM albums a \
         WHERE a.artist_id IN ({ids}) AND {masques} AND {doubles} \
         GROUP BY 1, 2",
        prov = expression_provenance_de(engine, "a"),
        masques = hidden_albums_excluded(),
        doubles = album_distant_double_exclu(engine, "a"),
    )
}

/// Les pistes du socle de la vue, par artiste de PISTE et par provenance.
///
/// Le socle est celui de `GET /library/tracks`, mais RÉSOLU une fois
/// ([`sql_pistes_ecartees_par_le_socle`], #5977) : posés tels quels, ses deux
/// replis corrélés se rejouaient sur chaque piste des artistes de la page.
/// Les deux formes écartent exactement les mêmes pistes (épreuve dans
/// `lenteur_pistes_5138_tests`).
pub(super) fn sql_pistes(engine: Engine, ids: &str) -> String {
    format!(
        "SELECT t.artist_id, {prov}, COUNT(*) FROM tracks t \
         WHERE t.artist_id IN ({ids}) AND {masques} \
         AND t.id NOT IN ({ecartees}) \
         GROUP BY 1, 2",
        prov = expression_provenance_de(engine, "t"),
        masques = hidden_tracks_excluded(),
        ecartees = sql_pistes_ecartees_par_le_socle(engine),
    )
}

impl ArtistRepo {
    /// Les sources des artistes `ids` — voir le module.
    pub fn sources_des_artistes(&self, ids: &[i64]) -> Result<SourcesDesArtistes, TuneError> {
        let mut table = SourcesDesArtistes::new();
        let engine = self.db.engine();
        for liste in crate::db::ordre_alphabetique::listes_d_ids(ids) {
            for (sql, pistes) in [
                (sql_albums(engine, &liste), false),
                (sql_pistes(engine, &liste), true),
            ] {
                for ligne in self.db.query_many(&sql, &[])? {
                    let Some(id) = ligne.first().and_then(|v| v.as_i64()) else {
                        continue;
                    };
                    let Some(cle) = ligne.get(1).and_then(|v| v.as_string()) else {
                        continue;
                    };
                    let n = ligne.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
                    let p = table.entry(id).or_default().entry(cle).or_default();
                    if pistes {
                        p.pistes += n;
                    } else {
                        p.albums += n;
                    }
                }
            }
        }
        Ok(table)
    }
}

#[cfg(test)]
#[path = "sources_des_artistes_tests.rs"]
mod tests;
