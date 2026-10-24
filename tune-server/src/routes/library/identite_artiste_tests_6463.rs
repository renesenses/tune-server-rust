//! #6463 — la fiche artiste : la saisie du MBID (`PUT`) le marque `manuel`,
//! « Détacher l'identité » le retire et le marque `manuel`, `GET` dit d'où
//! il vient. Aucun appel réseau : l'image de ces fiches n'est pas rattachée
//! au MBID, la recherche d'image n'est donc pas relancée.

use super::*;
use axum::body::to_bytes;
use tune_core::db::artist_repo::ProvenanceMbid;
use tune_core::db::backend::ToSqlValue;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn artiste(state: &AppState, id: i64, nom: &str, mbid: Option<&str>, source: Option<&str>) {
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name, musicbrainz_id, musicbrainz_id_source, image_path, image_source) \
             VALUES (?, ?, ?, ?, 'h-locale', 'local')",
            &[
                &id as &dyn ToSqlValue,
                &nom as &dyn ToSqlValue,
                &mbid as &dyn ToSqlValue,
                &source as &dyn ToSqlValue,
            ],
        )
        .unwrap();
}

async fn corps(r: Response) -> Value {
    let octets = to_bytes(r.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn provenance(state: &AppState, id: i64) -> Option<ProvenanceMbid> {
    ArtistRepo::with_backend(state.backend.clone())
        .provenance_du_mbid(id)
        .unwrap()
}

fn maj(mbid: Option<&str>, nom: Option<&str>) -> ArtistUpdate {
    ArtistUpdate {
        name: nom.map(str::to_string),
        sort_name: None,
        bio: None,
        musicbrainz_id: mbid.map(str::to_string),
    }
}

#[tokio::test]
async fn un_mbid_saisi_dans_la_fiche_est_manuel() {
    let s = etat();
    artiste(&s, 1, "Beatles", Some("mb-trouve"), Some("nom"));
    let r = update_artist(State(s.clone()), Path(1), Json(maj(Some("mb-saisi"), None)))
        .await
        .into_response();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        provenance(&s, 1),
        Some(ProvenanceMbid::Manuel),
        "un MBID saisi dans la fiche doit porter la provenance `manuel`"
    );
    let a = ArtistRepo::with_backend(s.backend.clone())
        .get(1)
        .unwrap()
        .unwrap();
    assert_eq!(a.musicbrainz_id.as_deref(), Some("mb-saisi"));
}

#[tokio::test]
async fn renvoyer_le_meme_mbid_ne_fige_pas_la_provenance() {
    let s = etat();
    artiste(&s, 1, "Magma", Some("mb-magma"), Some("nom"));
    let r = update_artist(
        State(s.clone()),
        Path(1),
        Json(maj(Some("mb-magma"), Some("Magma"))),
    )
    .await
    .into_response();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(provenance(&s, 1), Some(ProvenanceMbid::Nom));
}

#[tokio::test]
async fn detacher_l_identite_retire_le_mbid_et_le_marque_manuel() {
    let s = etat();
    artiste(&s, 1, "Ange", Some("mb-chanteuse"), Some("nom"));
    let r = detacher_identite(State(s.clone()), Path(1))
        .await
        .into_response();
    assert_eq!(r.status(), StatusCode::OK);
    let j = corps(r).await;
    assert_eq!(j["musicbrainz_id"], Value::Null);
    assert_eq!(j["musicbrainz_id_source"], "manuel");
    assert_eq!(j["mbid_retire"], "mb-chanteuse");
    assert_eq!(j["image_effacee"], false, "une image locale reste");
    assert_eq!(j["enrichissement_image"], "inutile");
    assert_eq!(provenance(&s, 1), Some(ProvenanceMbid::Manuel));
    assert!(
        ArtistRepo::with_backend(s.backend.clone())
            .identite_detachee(1)
            .unwrap()
    );

    // La fiche le dit.
    let g = get_artist(State(s.clone()), Path(1)).await.into_response();
    let j = corps(g).await;
    assert_eq!(j["musicbrainz_id_source"], "manuel");
    assert_eq!(j["musicbrainz_id"], Value::Null);

    // Et la fiche du cloud ne se retrouve plus par le nom.
    let a = ArtistRepo::with_backend(s.backend.clone())
        .get(1)
        .unwrap()
        .unwrap();
    let mut client = client_cloud(&s);
    assert_eq!(mbid_cloud(&s, &mut client, &a).await, None);
}

#[tokio::test]
async fn detacher_un_artiste_inconnu_rend_404() {
    let s = etat();
    let r = detacher_identite(State(s), Path(99)).await.into_response();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn la_fiche_dit_une_provenance_inconnue_par_null() {
    let s = etat();
    artiste(&s, 1, "Christophe", Some("mb-ancien"), None);
    let j = corps(get_artist(State(s), Path(1)).await.into_response()).await;
    assert!(j.as_object().unwrap().contains_key("musicbrainz_id_source"));
    assert_eq!(j["musicbrainz_id_source"], Value::Null);
}
