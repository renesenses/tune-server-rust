//! #8668 — `GET /library/album-countries?source=artist|release` : le pays de
//! chaque album, en une réponse, pour que la carte des pays de la
//! Bibliothèque compte les albums QUI PASSENT LES AUTRES FILTRES de la grille.
//!
//! La grille web filtre sur place (source, qualité, fréquence, format,
//! favoris, recherche…) : la plupart de ces filtres n'existent pas côté
//! facettes (la source « Qobuz » d'un favori de service, par exemple). Au
//! .18, la carte annonçait « États-Unis 20 albums » avec Source · QOBUZ
//! posée, et la grille en montrait 0. Le client croise désormais cette liste
//! avec les albums que sa grille garde, et compte lui-même : la carte et la
//! grille disent le même nombre.
//!
//! - `source=artist` (défaut) : pays de l'ARTISTE principal (`albums.artist_id`),
//!   lu sur MusicBrainz, code ISO en majuscules — même règle de validité que
//!   la facette `artist_country` ([`PAYS_ARTISTE_VALIDE`]).
//! - `source=release` : pays d'ÉDITION, la balise RELEASECOUNTRY telle
//!   qu'écrite, sinon le pays de la release MusicBrainz — même règle que la
//!   facette `country_albums` et le filtre `country=`. Un album balisé de
//!   deux valeurs (« fr » et « FR ») rend deux paires.
//!
//! Réponse : `{"source": "artist", "albums": [[id, "FR"], …]}`, triée par id.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::facet_filter::PAYS_ARTISTE_VALIDE;
use tune_http_types::panne_sql::OuDefautJournalise;

use super::facets::{hors_executeur, sql_pays_edition_par_piste};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub(super) struct PaysQuery {
    #[serde(default)]
    source: Option<String>,
}

pub(super) async fn pays_des_albums(
    Query(q): Query<PaysQuery>,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let edition = match q.source.as_deref().map(str::trim) {
        None | Some("") | Some("artist") => false,
        Some("release") => true,
        Some(autre) => {
            return Err(AppError::bad_request(format!(
                "source inconnue « {autre} » : artist ou release"
            )));
        }
    };
    hors_executeur("album_countries", move || lire(&state, edition))
        .await
        .map(Json)
}

/// Le corps de la route, hors de l'exécuteur.
pub(super) fn lire(state: &AppState, edition: bool) -> Value {
    let sql = if edition {
        format!(
            "SELECT DISTINCT x.album_id, x.pays FROM ({}) x \
             WHERE x.pays IS NOT NULL ORDER BY x.album_id, x.pays",
            sql_pays_edition_par_piste("")
        )
    } else {
        format!(
            "SELECT pa_al.id, UPPER(pa_ar.country) FROM albums pa_al \
             JOIN artists pa_ar ON pa_ar.id = pa_al.artist_id \
             WHERE {PAYS_ARTISTE_VALIDE} ORDER BY pa_al.id"
        )
    };
    let albums: Vec<Value> = state
        .backend
        .query_many(&sql, &[])
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|l| {
            let id = l.first()?.as_i64()?;
            let pays = l.get(1)?.as_string()?;
            Some(json!([id, pays]))
        })
        .collect();
    json!({
        "source": if edition { "release" } else { "artist" },
        "albums": albums,
    })
}
