//! #6288 — la route, sans réseau : la discographie est GARDÉE d'avance
//! (`albums_manquants::garder`), si bien qu'aucune requête ne part vers
//! MusicBrainz. La lecture page par page et ses pannes sont gardées dans
//! `tune_core::metadata::albums_manquants`.

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::albums_manquants::{self as am, Discographie, Groupe};

use crate::state::AppState;

const ARTISTE: &str = "83b9cbe7-9857-49e2-ab8e-b57b01038103";

fn g(n: u32, titre: &str, secondaires: &[&str], date: &str) -> Groupe {
    Groupe {
        mbid: format!("00000000-0000-0000-0000-{n:012}"),
        titre: titre.into(),
        type_primaire: Some("Album".into()),
        types_secondaires: secondaires.iter().map(|s| s.to_string()).collect(),
        premiere_sortie: Some(date.into()),
    }
}

async fn get(state: &AppState, chemin: &str) -> (u16, Value) {
    let r = super::super::router()
        .with_state(state.clone())
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = r.status().as_u16();
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    )
}

fn artiste(state: &AppState, mbid: Option<&str>) -> i64 {
    let id = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create("Pink Floyd", None, None)
        .unwrap()
        .id
        .unwrap();
    if let Some(m) = mbid {
        state
            .backend
            .execute(
                "UPDATE artists SET musicbrainz_id = ? WHERE id = ?",
                &[&m.to_string() as &dyn ToSqlValue, &id],
            )
            .unwrap();
    }
    id
}

#[tokio::test]
async fn un_artiste_sans_identifiant_ne_part_pas_en_requete_6288() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, None);
    let (statut, v) = get(&state, &format!("/artists/{id}/missing-albums")).await;
    assert_eq!(statut, 200);
    assert_eq!(v["statut"], "non_identifie");
    let (statut, _) = get(&state, "/artists/999999/missing-albums").await;
    assert_eq!(statut, 404);
}

#[tokio::test]
async fn la_route_soustrait_la_bibliotheque_de_la_discographie_gardee_6288() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, Some(ARTISTE));
    am::garder(
        &state.backend,
        ARTISTE,
        &Discographie {
            groupes: vec![
                g(1, "The Piper at the Gates of Dawn", &[], "1967-08-05"),
                g(2, "Meddle", &[], "1971-10-30"),
                g(3, "Pulse", &["Live"], "1995-05-29"),
            ],
            tronquee: false,
            lu_le: chrono::Utc::now().timestamp(),
        },
    );
    let album = AlbumRepo::with_backend(state.backend.clone())
        .get_or_create("Meddle", id, None)
        .unwrap()
        .id
        .unwrap();
    state
        .backend
        .execute(
            "UPDATE albums SET musicbrainz_release_group_id = ? WHERE id = ?",
            &[
                &"00000000-0000-0000-0000-000000000002".to_string() as &dyn ToSqlValue,
                &album,
            ],
        )
        .unwrap();

    let (statut, v) = get(&state, &format!("/artists/{id}/missing-albums")).await;
    assert_eq!(statut, 200, "{v}");
    assert_eq!(v["statut"], "ok");
    let titres: Vec<&str> = v["manquants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["titre"].as_str().unwrap())
        .collect();
    assert_eq!(titres, vec!["The Piper at the Gates of Dawn"]);
    assert_eq!(
        (v["possedes"].clone(), v["retenus"].clone()),
        (json!(1), json!(2))
    );

    let (_, v) = get(
        &state,
        &format!("/artists/{id}/missing-albums?secondaires=1"),
    )
    .await;
    assert_eq!(v["manquants"].as_array().unwrap().len(), 2, "{v}");
}

/// Relecture de #6636 — une bibliothèque illisible remonte comme une ERREUR
/// (500, `code: bibliotheque_illisible`), sans aucune liste de manquants.
#[tokio::test]
async fn une_bibliotheque_illisible_rend_une_erreur_et_aucun_manque_6288() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, Some(ARTISTE));
    am::garder(
        &state.backend,
        ARTISTE,
        &Discographie {
            groupes: vec![g(1, "Meddle", &[], "1971-10-30")],
            tronquee: false,
            lu_le: chrono::Utc::now().timestamp(),
        },
    );
    // La lecture des albums échoue : la table a disparu.
    state.backend.execute("DROP TABLE albums", &[]).unwrap();
    let (statut, v) = get(&state, &format!("/artists/{id}/missing-albums")).await;
    assert_eq!(
        statut, 500,
        "une bibliothèque illisible doit rendre une erreur, pas une réponse : {v}"
    );
    assert_eq!(v["code"], "bibliotheque_illisible");
    assert!(
        v.get("manquants").is_none(),
        "aucune liste de manquants ne doit accompagner l'erreur : {v}"
    );
}

/// Un artiste sans aucun album dans la bibliothèque : rien n'est comparé, et
/// sa discographie n'est pas annoncée manquante en bloc.
#[tokio::test]
async fn un_artiste_sans_album_n_est_pas_compare_6288() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, Some(ARTISTE));
    am::garder(
        &state.backend,
        ARTISTE,
        &Discographie {
            groupes: vec![g(1, "Meddle", &[], "1971-10-30")],
            tronquee: false,
            lu_le: chrono::Utc::now().timestamp(),
        },
    );
    let (statut, v) = get(&state, &format!("/artists/{id}/missing-albums")).await;
    assert_eq!(statut, 200);
    assert_eq!(v["statut"], "sans_album", "{v}");
    assert_eq!(v["manquants"], json!([]));
}
