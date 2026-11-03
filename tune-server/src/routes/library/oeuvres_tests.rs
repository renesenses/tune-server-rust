//! Le regroupement des versions d'un album en œuvres (#7617) : la fonction
//! pure sur les trois clés, puis les routes sur le vrai routeur.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use tune_core::db::backend::ToSqlValue;

use super::*;
use crate::state::AppState;

fn c(id: i64, titre: &str, artiste: Option<i64>, groupe: Option<&str>) -> Candidat {
    Candidat {
        id,
        titre: titre.to_string(),
        artist_id: artiste,
        groupe_mb: groupe.map(str::to_string),
        source: "local".to_string(),
        ..Default::default()
    }
}

fn grouper(albums: &[Candidat]) -> Vec<Oeuvre> {
    regrouper(albums, true, &DistinctPairSet::default(), &HashMap::new())
}

/// Les identifiants d'albums de chaque œuvre, triés, œuvres triées.
fn partition(oeuvres: &[Oeuvre]) -> Vec<Vec<i64>> {
    let mut p: Vec<Vec<i64>> = oeuvres
        .iter()
        .map(|o| {
            let mut ids: Vec<i64> = o.versions.iter().map(|v| v.id).collect();
            ids.sort();
            ids
        })
        .collect();
    p.sort();
    p
}

fn oeuvre_de(oeuvres: &[Oeuvre], id: i64) -> &Oeuvre {
    oeuvres
        .iter()
        .find(|o| o.versions.iter().any(|v| v.id == id))
        .expect("album absent des œuvres")
}

// ── Clé 1 : le groupe de sortie MusicBrainz ────────────────────────────────

#[test]
fn cle_1_meme_groupe_musicbrainz_groupe_des_titres_differents() {
    let albums = [
        c(1, "The Dark Side of the Moon", Some(10), Some("rg-dsotm")),
        c(
            2,
            "Dark Side Of The Moon [SACD]",
            Some(10),
            Some("rg-dsotm"),
        ),
        c(3, "Wish You Were Here", Some(10), Some("rg-wywh")),
    ];
    let o = grouper(&albums);
    assert_eq!(
        partition(&o),
        vec![vec![1, 2], vec![3]],
        "deux albums du même groupe MusicBrainz doivent faire une seule œuvre"
    );
    assert_eq!(oeuvre_de(&o, 1).methode, Methode::MusicBrainz);
    assert_eq!(oeuvre_de(&o, 1).work_id, "mb:rg-dsotm");
}

#[test]
fn cle_1_deux_groupes_musicbrainz_restent_deux_oeuvres_meme_au_meme_titre() {
    let albums = [
        c(1, "Live", Some(10), Some("rg-a")),
        c(2, "Live", Some(10), Some("rg-b")),
        // Sans identifiant : ambigu entre les deux groupes, il ne rejoint
        // aucun des deux.
        c(3, "Live", Some(10), None),
    ];
    assert_eq!(
        partition(&grouper(&albums)),
        vec![vec![1], vec![2], vec![3]]
    );
}

#[test]
fn cle_1_un_album_sans_identifiant_rejoint_le_seul_groupe_de_meme_artiste_et_titre() {
    let albums = [
        c(1, "Abbey Road", Some(7), Some("rg-abbey")),
        c(2, "Abbey Road (2019 Remaster)", Some(7), None),
    ];
    let o = grouper(&albums);
    assert_eq!(partition(&o), vec![vec![1, 2]]);
    assert_eq!(o[0].methode, Methode::MusicBrainz);
}

// ── Clé 2 : artiste + titre normalisé ───────────────────────────────────────

#[test]
fn cle_2_meme_artiste_titre_sans_mention_d_edition_groupe() {
    let albums = [
        c(1, "Abbey Road", Some(7), None),
        c(2, "Abbey Road (2019 Remaster)", Some(7), None),
        c(3, "Abbey Road [Super Deluxe Edition]", Some(7), None),
        c(4, "Abbey Road - Remastered 2009", Some(7), None),
    ];
    let o = grouper(&albums);
    assert_eq!(
        partition(&o),
        vec![vec![1, 2, 3, 4]],
        "« Abbey Road » et ses éditions remasterisées / deluxe du même artiste doivent faire une seule œuvre"
    );
    assert_eq!(o[0].methode, Methode::ArtisteTitre);
}

#[test]
fn cle_2_le_meme_titre_chez_un_autre_artiste_n_est_pas_groupe() {
    let albums = [
        c(1, "Greatest Hits", Some(1), None),
        c(2, "Greatest Hits (Remastered)", Some(2), None),
        c(3, "Greatest Hits", None, None),
    ];
    assert_eq!(
        partition(&grouper(&albums)),
        vec![vec![1], vec![2], vec![3]],
        "le titre seul ne suffit jamais à regrouper"
    );
}

#[test]
fn cle_2_une_parenthese_qui_n_est_pas_une_edition_ne_groupe_pas() {
    let albums = [
        c(1, "Wish You Were Here", Some(10), None),
        c(2, "Wish You Were Here (Live)", Some(10), None),
        c(3, "Physical Graffiti (Part 2)", Some(11), None),
        c(4, "Physical Graffiti", Some(11), None),
    ];
    assert_eq!(
        partition(&grouper(&albums)),
        vec![vec![1], vec![2], vec![3], vec![4]]
    );
}

#[test]
fn titre_normalise_retire_les_mentions_d_edition_empilees() {
    assert_eq!(
        sans_mentions_d_edition("Abbey Road (Deluxe Edition) [2019 Remaster]"),
        (
            "Abbey Road".to_string(),
            vec!["Deluxe Edition".to_string(), "2019 Remaster".to_string()]
        )
    );
    assert_eq!(
        titre_normalise("Smooth Operator - 2011 Remastered"),
        titre_normalise("smooth operator")
    );
    assert_eq!(sans_mentions_d_edition("Heroes").1, Vec::<String>::new());
}

// ── Clé 3 : le nom de chaque version ────────────────────────────────────────

#[test]
fn cle_3_version_nomme_la_version_sinon_la_mention_du_titre() {
    let mut hires = c(2, "Abbey Road (2019 Remaster)", Some(7), None);
    hires.version = Some("Hi-Res 24/96".to_string());
    let albums = [
        c(1, "Abbey Road", Some(7), None),
        hires,
        c(3, "Abbey Road (Super Deluxe Edition)", Some(7), None),
    ];
    let o = grouper(&albums);
    let noms: HashMap<i64, Option<String>> = o[0]
        .versions
        .iter()
        .map(|v| (v.id, v.nom.clone()))
        .collect();
    assert_eq!(noms[&1], None);
    assert_eq!(
        noms[&2].as_deref(),
        Some("Hi-Res 24/96"),
        "l'étiquette VERSION prime sur la mention du titre"
    );
    assert_eq!(noms[&3].as_deref(), Some("Super Deluxe Edition"));
}

#[test]
fn cle_3_version_ne_regroupe_rien() {
    let mut a = c(1, "Kind of Blue", Some(1), None);
    a.version = Some("Remaster".into());
    let mut b = c(2, "Blue Train", Some(1), None);
    b.version = Some("Remaster".into());
    assert_eq!(partition(&grouper(&[a, b])), vec![vec![1], vec![2]]);
}

// ── Album sans autre version ────────────────────────────────────────────────

#[test]
fn un_album_sans_version_reste_une_oeuvre_seule() {
    let o = grouper(&[c(5, "Blue Train", Some(1), None)]);
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].versions.len(), 1);
    assert_eq!(o[0].methode, Methode::Seule);
    assert_eq!(o[0].preferee, 5);
    assert_eq!(o[0].versions[0].nom, None);
}

// ── Réglage, séparation à la main, version préférée ─────────────────────────

#[test]
fn reglage_desactive_rien_n_est_groupe() {
    let albums = [
        c(1, "Abbey Road", Some(7), Some("rg")),
        c(2, "Abbey Road (2019 Remaster)", Some(7), Some("rg")),
    ];
    let o = regrouper(&albums, false, &DistinctPairSet::default(), &HashMap::new());
    assert_eq!(partition(&o), vec![vec![1], vec![2]]);
}

#[test]
fn version_preferee_par_defaut_locale_puis_meilleure_resolution() {
    let mut cd = c(1, "Abbey Road", Some(7), None);
    cd.sample_rate = Some(44_100);
    cd.bit_depth = Some(16);
    let mut hires = c(2, "Abbey Road (2019 Remaster)", Some(7), None);
    hires.sample_rate = Some(96_000);
    hires.bit_depth = Some(24);
    let mut service = c(3, "Abbey Road (Deluxe)", Some(7), None);
    service.source = "qobuz".into();
    service.sample_rate = Some(192_000);
    service.bit_depth = Some(24);
    let o = grouper(&[cd, hires, service]);
    assert_eq!(
        o[0].preferee, 2,
        "la Hi-Res locale avant la CD et avant le service"
    );
    let ordre: Vec<i64> = o[0].versions.iter().map(|v| v.id).collect();
    assert_eq!(ordre, vec![2, 1, 3]);
}

#[test]
fn version_preferee_choisie_par_l_utilisateur() {
    let albums = [
        c(1, "Abbey Road", Some(7), Some("rg")),
        c(2, "Abbey Road (Mono)", Some(7), Some("rg")),
    ];
    let mut choix = HashMap::new();
    choix.insert("mb:rg".to_string(), 2);
    let o = regrouper(&albums, true, &DistinctPairSet::default(), &choix);
    assert_eq!(o[0].preferee, 2);
    // Un choix qui ne désigne plus un membre est ignoré.
    choix.insert("mb:rg".to_string(), 99);
    let o = regrouper(&albums, true, &DistinctPairSet::default(), &choix);
    assert_eq!(o[0].preferee, 1);
}

// ── Les routes ──────────────────────────────────────────────────────────────

fn artiste(state: &AppState, nom: &str) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO artists (name) VALUES (?1)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

fn album(state: &AppState, titre: &str, artiste: i64, groupe: Option<&str>, sr: i64) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id, musicbrainz_release_group_id, source, \
             sample_rate, bit_depth) VALUES (?1, ?2, ?3, 'local', ?4, 16)",
            &[&titre as &dyn ToSqlValue, &artiste, &groupe, &sr],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

async fn appeler(
    state: &AppState,
    methode: &str,
    url: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(url);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

struct Banc {
    state: AppState,
    abbey: i64,
    abbey_remaster: i64,
    dsotm: i64,
    dsotm_sacd: i64,
    hits_a: i64,
    hits_b: i64,
}

fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let beatles = artiste(&state, "The Beatles");
    let floyd = artiste(&state, "Pink Floyd");
    let queen = artiste(&state, "Queen");
    let abbey = album(&state, "Abbey Road", beatles, None, 44_100);
    let abbey_remaster = album(&state, "Abbey Road (2019 Remaster)", beatles, None, 96_000);
    let dsotm = album(
        &state,
        "The Dark Side of the Moon",
        floyd,
        Some("rg-dsotm"),
        44_100,
    );
    let dsotm_sacd = album(
        &state,
        "Dark Side of the Moon (SACD)",
        floyd,
        Some("rg-dsotm"),
        88_200,
    );
    let hits_a = album(&state, "Greatest Hits", queen, None, 44_100);
    let hits_b = album(&state, "Greatest Hits", floyd, None, 44_100);
    Banc {
        state,
        abbey,
        abbey_remaster,
        dsotm,
        dsotm_sacd,
        hits_a,
        hits_b,
    }
}

fn ids(oeuvre: &Value) -> Vec<i64> {
    let mut v: Vec<i64> = oeuvre["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_i64().unwrap())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn route_works_rend_une_fiche_par_oeuvre() {
    let b = banc();
    let (statut, corps) = appeler(&b.state, "GET", "/api/v1/library/albums/works", None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["enabled"], json!(true));
    assert_eq!(corps["albums_total"], json!(6));
    assert_eq!(corps["total"], json!(4), "{corps}");
    let items = corps["items"].as_array().unwrap();
    let partition: Vec<Vec<i64>> = {
        let mut p: Vec<Vec<i64>> = items.iter().map(ids).collect();
        p.sort();
        p
    };
    let mut attendu = vec![
        vec![b.abbey, b.abbey_remaster],
        vec![b.dsotm, b.dsotm_sacd],
        vec![b.hits_a],
        vec![b.hits_b],
    ];
    attendu.sort();
    assert_eq!(partition, attendu);
    let abbey = items.iter().find(|o| ids(o).contains(&b.abbey)).unwrap();
    assert_eq!(abbey["method"], json!("artist_title"));
    assert_eq!(
        abbey["preferred_id"],
        json!(b.abbey_remaster),
        "la meilleure résolution par défaut"
    );
    assert_eq!(abbey["preferred"]["id"], json!(b.abbey_remaster));
    let remaster = abbey["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(b.abbey_remaster))
        .unwrap();
    assert_eq!(remaster["version_name"], json!("2019 Remaster"));
}

#[tokio::test]
async fn route_versions_d_un_album_et_version_tag() {
    let b = banc();
    // VERSION portée par toutes les pistes de l'édition SACD.
    b.state
        .backend
        .execute(
            "INSERT INTO tracks (title, album_id, file_path, source) VALUES ('Time', ?1, '/m/t.flac', 'local')",
            &[&b.dsotm_sacd as &dyn ToSqlValue],
        )
        .unwrap();
    let piste = b.state.backend.last_insert_rowid();
    b.state
        .backend
        .execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?1, 'version', '2003 SACD')",
            &[&piste as &dyn ToSqlValue],
        )
        .unwrap();
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.dsotm),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["method"], json!("musicbrainz"));
    assert_eq!(ids(&corps), vec![b.dsotm, b.dsotm_sacd]);
    let sacd = corps["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(b.dsotm_sacd))
        .unwrap();
    assert_eq!(sacd["version_name"], json!("2003 SACD"));

    // Un album sans autre version : une œuvre d'une seule version.
    let (_, seul) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.hits_a),
        None,
    )
    .await;
    assert_eq!(seul["count"], json!(1));
    assert_eq!(seul["method"], json!("single"));
    assert_eq!(seul["versions"][0]["version_name"], Value::Null);
}

#[tokio::test]
async fn route_reglage_desactive_puis_reactive() {
    let b = banc();
    let (statut, corps) = appeler(
        &b.state,
        "PUT",
        "/api/v1/library/albums/works/settings",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let (_, corps) = appeler(&b.state, "GET", "/api/v1/library/albums/works", None).await;
    assert_eq!(corps["enabled"], json!(false));
    assert_eq!(corps["total"], json!(6), "désactivé : un album, une fiche");
    let (_, lu) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works/settings",
        None,
    )
    .await;
    assert_eq!(lu, json!({"enabled": false}));
    appeler(
        &b.state,
        "PUT",
        "/api/v1/library/albums/works/settings",
        Some(json!({"enabled": true})),
    )
    .await;
    let (_, corps) = appeler(&b.state, "GET", "/api/v1/library/albums/works", None).await;
    assert_eq!(corps["total"], json!(4));
}

#[tokio::test]
async fn route_paire_distincte_separe_et_preferee_memorisee() {
    let b = banc();
    AlbumDistinctRepo::with_backend(b.state.backend.clone())
        .declarer_distincts(b.dsotm, b.dsotm_sacd)
        .unwrap();
    let (_, corps) = appeler(&b.state, "GET", "/api/v1/library/albums/works", None).await;
    assert_eq!(
        corps["total"],
        json!(5),
        "la paire « pas des doublons » est séparée"
    );

    let (statut, _) = appeler(
        &b.state,
        "PUT",
        "/api/v1/library/albums/works/preferred",
        Some(json!({"work_id": format!("at:{}:abbey road", artiste_de(&b, b.abbey)), "album_id": b.abbey})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let (_, oeuvre) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.abbey_remaster),
        None,
    )
    .await;
    assert_eq!(oeuvre["preferred_id"], json!(b.abbey), "{oeuvre}");
}

fn artiste_de(b: &Banc, album: i64) -> i64 {
    b.state
        .backend
        .query_one(
            "SELECT artist_id FROM albums WHERE id = ?1",
            &[&album as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn la_liste_des_albums_reste_inchangee() {
    let b = banc();
    let (statut, corps) = appeler(&b.state, "GET", "/api/v1/library/albums?limit=50", None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let n = corps["items"]
        .as_array()
        .or_else(|| corps.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    assert_eq!(
        n, 6,
        "GET /library/albums garde un album par ligne : {corps}"
    );
}

#[tokio::test]
async fn route_members_rend_les_seules_oeuvres_de_plusieurs_versions() {
    let b = banc();
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works/members",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let works = corps["works"].as_array().unwrap();
    assert_eq!(works.len(), 2, "{corps}");
    let abbey = works
        .iter()
        .find(|w| w["ids"].as_array().unwrap().contains(&json!(b.abbey)))
        .unwrap();
    assert_eq!(abbey["preferred_id"], json!(b.abbey_remaster));
    appeler(
        &b.state,
        "PUT",
        "/api/v1/library/albums/works/settings",
        Some(json!({"enabled": false})),
    )
    .await;
    let (_, corps) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works/members",
        None,
    )
    .await;
    assert_eq!(corps, json!({"enabled": false, "works": []}));
}
