//! #6001 — les trois routes, sans réseau : chaque cas testé ici s'arrête
//! avant tout appel à MusicBrainz ou à Wikidata (identifiant absent, réponse
//! gardée). Le transport et le filtrage des liens sont gardés dans
//! `tune_core::metadata::liens_externes` et `arbre_genres`.

use std::collections::{BTreeSet, HashMap};

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::arbre_genres::{self, GenreMb, Voisins};

use super::super::api_cache_set;
use crate::state::AppState;

const ARTISTE_MBID: &str = "83b9cbe7-9857-49e2-ab8e-b57b01038103";
const GROUPE_MBID: &str = "f5093c06-23e3-404f-aeaa-40f72885ee3a";
const SKA: &str = "65877068-2116-4af6-89a2-838179a7e103";
const ROCK: &str = "0e3fc579-2d24-4f20-9dae-736e1ec78798";
const DEUX_TONE: &str = "18797864-41b0-4602-8241-adbee761774c";

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

fn artiste(state: &AppState, nom: &str, mbid: Option<&str>) -> i64 {
    let a = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create(nom, None, None)
        .unwrap();
    let id = a.id.unwrap();
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
async fn sans_identifiant_musicbrainz_rien_n_est_demande_6001() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, "Inconnu", None);
    let (statut, v) = get(&state, &format!("/artists/{id}/external-links")).await;
    assert_eq!(statut, 200);
    assert_eq!(
        v,
        json!({"statut": "non_identifie", "mbid": null, "liens": []})
    );
    let (statut, _) = get(&state, "/artists/999999/external-links").await;
    assert_eq!(statut, 404);
}

#[tokio::test]
async fn les_liens_gardes_d_un_artiste_et_d_un_album_sont_servis_6001() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = artiste(&state, "Pink Floyd", Some(ARTISTE_MBID));
    let lien = json!([{"site": "progarchives", "nom": "ProgArchives",
                       "url": "https://www.progarchives.com/artist.asp?id=78"}]);
    api_cache_set(
        &state.backend,
        &format!("cache:liens-externes:artist:{ARTISTE_MBID}"),
        &lien,
    );
    let (statut, v) = get(&state, &format!("/artists/{id}/external-links")).await;
    assert_eq!(statut, 200);
    assert_eq!(
        v,
        json!({"statut": "ok", "mbid": ARTISTE_MBID, "liens": lien})
    );

    let album = AlbumRepo::with_backend(state.backend.clone())
        .get_or_create("The Dark Side of the Moon", id, None)
        .unwrap()
        .id
        .unwrap();
    state
        .backend
        .execute(
            "UPDATE albums SET musicbrainz_release_group_id = ? WHERE id = ?",
            &[&GROUPE_MBID.to_string() as &dyn ToSqlValue, &album],
        )
        .unwrap();
    let lien_album = json!([{"site": "allmusic", "nom": "AllMusic",
                             "url": "https://www.allmusic.com/album/mw0000191308"}]);
    api_cache_set(
        &state.backend,
        &format!("cache:liens-externes:release-group:{GROUPE_MBID}"),
        &lien_album,
    );
    let (_, v) = get(&state, &format!("/albums/{album}/external-links")).await;
    assert_eq!(
        v["liens"], lien_album,
        "l'album doit lire son groupe de parutions : {v}"
    );
}

#[tokio::test]
async fn l_arbre_musicbrainz_rapproche_les_genres_de_la_bibliotheque_6001() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for (titre, genre) in [("a", "Ska"), ("b", "Chanson"), ("c", "ska")] {
        state
            .backend
            .execute(
                "INSERT INTO tracks (title, genre) VALUES (?, ?)",
                &[&titre.to_string() as &dyn ToSqlValue, &genre.to_string()],
            )
            .unwrap();
    }
    let t = chrono::Utc::now().timestamp();
    let index = vec![
        GenreMb {
            nom: "2 tone".into(),
            mbid: DEUX_TONE.into(),
        },
        GenreMb {
            nom: "rock".into(),
            mbid: ROCK.into(),
        },
        GenreMb {
            nom: "ska".into(),
            mbid: SKA.into(),
        },
    ];
    arbre_genres::garder_index(&state.backend, &index, t);
    arbre_genres::garder_voisins(
        &state.backend,
        HashMap::from([(
            SKA.to_string(),
            Voisins {
                parents: BTreeSet::from([ROCK.to_string()]),
                enfants: BTreeSet::from([DEUX_TONE.to_string()]),
                lu_le: t,
            },
        )]),
    );
    let (statut, v) = get(&state, "/genre-tree/musicbrainz").await;
    assert_eq!(statut, 200, "{v}");
    assert_eq!(v["statut"], "ok");
    assert_eq!(v["licence"], "CC0");
    assert_eq!(v["non_reconnus"], json!(["Chanson"]));
    assert_eq!(
        v["noeuds"],
        json!([{"genre": "Ska", "mbid": SKA, "nom_musicbrainz": "ska",
                "parents": ["rock"], "sous_genres": ["2 tone"]}])
    );
}

/// Une tâche de fond qui panique laisse l'indicateur à faux : la lecture
/// suivante peut partir. Avant, l'indicateur n'était baissé qu'à la dernière
/// ligne de la tâche, que la panique n'atteint jamais.
#[tokio::test]
async fn une_tache_qui_panique_laisse_l_indicateur_a_faux_6001() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INDICATEUR: AtomicBool = AtomicBool::new(false);

    let tache = super::lancer_seule(&INDICATEUR, async {
        panic!("panique simulée de la lecture des genres");
    })
    .expect("indicateur baissé : la tâche part");
    assert!(
        tache.await.unwrap_err().is_panic(),
        "la tâche a bien paniqué"
    );
    assert!(
        !INDICATEUR.load(Ordering::SeqCst),
        "après une panique, l'indicateur doit être retombé à faux, sinon plus \
         aucune lecture ne part jusqu'au redémarrage"
    );

    // La lecture suivante part, et l'indicateur tient pendant qu'elle vit.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let suivante = super::lancer_seule(&INDICATEUR, async move {
        let _ = rx.await;
    })
    .expect("indicateur retombé : la lecture suivante part");
    assert!(
        super::lancer_seule(&INDICATEUR, async {}).is_none(),
        "une seule lecture à la fois"
    );
    tx.send(()).unwrap();
    suivante.await.unwrap();
    assert!(!INDICATEUR.load(Ordering::SeqCst), "fin normale : baissé");
}
