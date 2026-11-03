//! `GET /api/v1/library/revision` — #7619, volet L5.
//!
//! Le client web garde les pages de la Bibliothèque dans IndexedDB. Il les
//! affiche aussitôt, les redemande, et jette tout son cache quand la révision
//! rendue ici change.
//!
//! Le compteur est celui du volet L4, `tune_core::db::library_revision` : tenu
//! par des déclencheurs de la base sur les tables que la Bibliothèque lit. UN
//! seul compteur pour tous les volets ; cette route ne fait que l'exposer.
//!
//! Forme : `{"revision": "<instance>-<revision>"}`. L'instance (tirée au hasard
//! à la création de la ligne) sépare deux bases : une base recréée ne peut pas
//! rendre une révision déjà vue par le client.
//!
//! Compteur absent ou illisible (`lire` rend `None`) : `{"revision": null}`.
//! Le client éteint alors son cache — plus lent, jamais faux.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use tune_core::db::backend::DbBackend;

use crate::state::AppState;

/// La révision sous sa forme d'API, ou `None`.
pub(crate) fn revision_de(db: &dyn DbBackend) -> Option<String> {
    tune_core::db::library_revision::lire(db).map(|(instance, n)| format!("{instance}-{n}"))
}

pub(super) async fn revision_courante(State(state): State<AppState>) -> Response {
    let backend = state.backend.clone();
    let revision = tokio::task::spawn_blocking(move || revision_de(&*backend))
        .await
        .ok()
        .flatten();
    let mut reponse = Json(serde_json::json!({ "revision": revision })).into_response();
    // Jamais en cache : c'est précisément la question « ai-je encore raison ? ».
    reponse
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reponse
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::sqlite::SqliteDb;

    fn base() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        db
    }

    /// Témoin : une écriture dans la bibliothèque change la révision rendue
    /// par l'API ; la forme porte l'instance.
    #[test]
    fn une_ecriture_change_la_revision_rendue() {
        let db = base();
        let avant = revision_de(&db).expect("compteur posé au démarrage");
        let (instance, _) = avant.split_once('-').expect("<instance>-<revision>");
        db.execute_batch("INSERT INTO artists (id, name) VALUES (1, 'A');")
            .unwrap();
        let apres = revision_de(&db).unwrap();
        assert_ne!(
            avant, apres,
            "une écriture dans la bibliothèque doit changer la révision rendue au client"
        );
        assert!(apres.starts_with(&format!("{instance}-")));
    }

    #[test]
    fn la_musique_qui_joue_ne_change_rien() {
        let db = base();
        let avant = revision_de(&db).unwrap();
        db.execute_batch("INSERT INTO listen_history (title) VALUES ('x');")
            .unwrap();
        assert_eq!(revision_de(&db).unwrap(), avant);
    }

    #[test]
    fn sans_compteur_pas_de_revision() {
        let db = SqliteDb::open_in_memory().unwrap();
        assert_eq!(revision_de(&db), None);
    }
}
