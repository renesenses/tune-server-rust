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

#[tokio::test]
async fn route_versions_n_offre_pas_un_album_masque_du_meme_groupe() {
    let b = banc();
    let floyd = artiste_de(&b, b.dsotm);
    // Une Hi-Res du même groupe MusicBrainz, masquée : sans le filtre, elle
    // serait la version par défaut de la fiche.
    let masque = album(
        &b.state,
        "The Dark Side of the Moon (24-bit)",
        floyd,
        Some("rg-dsotm"),
        192_000,
    );
    tune_core::db::hidden_repo::HiddenRepo::with_backend(b.state.backend.clone())
        .hide_album(masque)
        .unwrap();
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.dsotm),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        ids(&corps),
        vec![b.dsotm, b.dsotm_sacd],
        "un album masqué n'est pas une version proposée : {corps}"
    );
    assert_eq!(corps["preferred_id"], json!(b.dsotm_sacd), "{corps}");

    // La fiche et la grille annoncent la même version préférée.
    let (_, membres) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works/members",
        None,
    )
    .await;
    let dsotm = membres["works"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["ids"].as_array().unwrap().contains(&json!(b.dsotm)))
        .unwrap();
    assert_eq!(dsotm["preferred_id"], corps["preferred_id"]);

    // L'album masqué lui-même s'ouvre encore.
    let (statut, seul) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{masque}/versions"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{seul}");
    assert!(ids(&seul).contains(&masque), "{seul}");
}

#[tokio::test]
async fn paire_distincte_meme_oeuvre_dans_la_grille_et_dans_la_fiche() {
    let b = banc();
    let artiste_x = artiste(&b.state, "The Zombies");
    // Insérés dans cet ordre, les identifiants (mono < nu < remaster)
    // diffèrent de l'ordre des titres (nu < remaster < mono).
    let mono = album(&b.state, "Odessey (Mono)", artiste_x, None, 44_100);
    let nu = album(&b.state, "Odessey", artiste_x, None, 44_100);
    let remaster = album(&b.state, "Odessey (2018 Remaster)", artiste_x, None, 96_000);
    AlbumDistinctRepo::with_backend(b.state.backend.clone())
        .declarer_distincts(mono, nu)
        .unwrap();

    // La grille : toutes les œuvres, albums triés par titre.
    let (_, grille) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works?limit=1000",
        None,
    )
    .await;
    let dans_la_grille = |album: i64| -> (String, Vec<i64>) {
        let o = grille["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| ids(o).contains(&album))
            .unwrap();
        (o["work_id"].as_str().unwrap().to_string(), ids(o))
    };
    // La fiche : les candidats triés par identifiant.
    for a in [mono, nu, remaster] {
        let (statut, fiche) = appeler(
            &b.state,
            "GET",
            &format!("/api/v1/library/albums/{a}/versions"),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{fiche}");
        assert_eq!(
            (fiche["work_id"].as_str().unwrap().to_string(), ids(&fiche)),
            dans_la_grille(a),
            "l'album {a} doit être dans la même œuvre, sous le même work_id, \
             dans la grille et dans la fiche"
        );
    }
    let (_, avec_mono) = dans_la_grille(mono);
    assert_eq!(avec_mono, vec![mono, remaster]);
    let (_, avec_nu) = dans_la_grille(nu);
    assert_eq!(avec_nu, vec![nu]);
}

// ── tune-web-client#2731 — les canaux de CHAQUE version ─────────────────────

fn piste_a_canaux(state: &AppState, album: i64, chemin: &str, canaux: i64) {
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, album_id, file_path, source, channels) \
             VALUES ('t', ?1, ?2, 'local', ?3)",
            &[
                &album as &dyn ToSqlValue,
                &chemin.to_string() as &dyn ToSqlValue,
                &canaux as &dyn ToSqlValue,
            ],
        )
        .unwrap();
}

fn version(oeuvre: &Value, id: i64) -> &Value {
    oeuvre["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == json!(id))
        .unwrap_or_else(|| panic!("version {id} absente de {oeuvre}"))
}

#[tokio::test]
async fn chaque_version_porte_ses_canaux_2731() {
    let b = banc();
    // L'édition stéréo et l'édition SACD multicanale du même disque : la
    // fiche doit dire laquelle est en 2.0 et laquelle en 5.1.
    piste_a_canaux(&b.state, b.dsotm, "/m/stereo/01.dsf", 2);
    piste_a_canaux(&b.state, b.dsotm_sacd, "/m/multi/01.dsf", 2);
    piste_a_canaux(&b.state, b.dsotm_sacd, "/m/multi/02.dsf", 6);

    let (statut, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.dsotm),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        version(&corps, b.dsotm)["max_channels"],
        json!(2),
        "{corps}"
    );
    assert_eq!(
        version(&corps, b.dsotm_sacd)["max_channels"],
        json!(6),
        "le canal MAXIMUM des pistes de la version : {corps}"
    );

    // Même chose dans la bibliothèque en œuvres.
    let (_, oeuvres) = appeler(&b.state, "GET", "/api/v1/library/albums/works", None).await;
    let dsotm = oeuvres["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| ids(o).contains(&b.dsotm))
        .unwrap();
    assert_eq!(version(dsotm, b.dsotm)["max_channels"], json!(2));
    assert_eq!(version(dsotm, b.dsotm_sacd)["max_channels"], json!(6));

    // Canaux inconnus : la clé est ABSENTE, le client ne pose aucun badge.
    let (_, seul) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.hits_a),
        None,
    )
    .await;
    assert!(
        seul["versions"][0].get("max_channels").is_none(),
        "pas de canaux connus, pas de clé : {seul}"
    );
}

/// web#3064 : chaque version porte son numéro de catalogue quand la
/// bibliothèque le connaît ; sinon (inconnu ou vide) la clé est absente.
#[tokio::test]
async fn chaque_version_porte_son_numero_de_catalogue_quand_il_est_connu() {
    let b = banc();
    b.state
        .backend
        .execute(
            "UPDATE albums SET catalog_number = ?1 WHERE id = ?2",
            &[&"CDP 7 46001 2" as &dyn ToSqlValue, &b.dsotm],
        )
        .unwrap();
    b.state
        .backend
        .execute(
            "UPDATE albums SET catalog_number = '  ' WHERE id = ?1",
            &[&b.hits_a as &dyn ToSqlValue],
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
    assert_eq!(
        version(&corps, b.dsotm)["catalog_number"],
        json!("CDP 7 46001 2")
    );
    assert!(
        version(&corps, b.dsotm_sacd)
            .get("catalog_number")
            .is_none(),
        "pas de numéro connu, pas de clé : {corps}"
    );

    // Un numéro vide (que des blancs) vaut « inconnu ».
    let (_, seul) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.hits_a),
        None,
    )
    .await;
    assert!(
        seul["versions"][0].get("catalog_number").is_none(),
        "numéro vide, pas de clé : {seul}"
    );
}

// ── #8953 (fil 2884) : un coffret n'est la version de rien ─────────────────

/// Un album DSD de Didier : ses dix pistes, disque 1, en local.
fn disque_dsd(b: &Banc, titre: &str, artiste: i64, dossier: &str) -> i64 {
    let id = album(&b.state, titre, artiste, None, 2_822_400);
    for n in 1..=10i64 {
        let chemin = format!("{dossier}/{n:02}.dsf");
        b.state
            .backend
            .execute(
                "INSERT INTO tracks (title, album_id, file_path, source, track_number, \
                 disc_number, format) VALUES (?1, ?2, ?3, 'local', ?4, 1, 'dsf')",
                &[&format!("piste {n}") as &dyn ToSqlValue, &id, &chemin, &n],
            )
            .unwrap();
    }
    id
}

/// Les identifiants que `/works/members` replie dans une œuvre de plusieurs
/// versions, et ceux qu'il y garde pour la carte.
async fn membres(b: &Banc) -> (Vec<i64>, Vec<i64>) {
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        "/api/v1/library/albums/works/members",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let mut replies = Vec::new();
    let mut gardes = Vec::new();
    for w in corps["works"].as_array().unwrap() {
        let pref = w["preferred_id"].as_i64().unwrap();
        gardes.push(pref);
        for id in w["ids"].as_array().unwrap() {
            let id = id.as_i64().unwrap();
            if id != pref {
                replies.push(id);
            }
        }
    }
    (replies, gardes)
}

/// Le témoin : les deux disques de Didier réunis à la main, sous le nom que
/// Tune propose. Le coffret ne doit être replié sous AUCUNE autre carte —
/// ni par `/works/members` (la grille), ni par `/albums/{id}/versions` (la
/// fiche) — et `GET /library/albums` le rend, recherche comprise.
#[tokio::test]
async fn un_coffret_compose_n_est_replie_sous_aucune_autre_carte_8953() {
    let b = banc();
    let floyd = artiste_de(&b, b.dsotm);
    let stereo = disque_dsd(
        &b,
        "The Dark Side Of The Moon (2023)(50th Anniversary)(Stereo) [DSD64 - SACD Sony Records Int'l Japan 2023]",
        floyd,
        "/m/dsotm50/Stereo",
    );
    let multi = disque_dsd(
        &b,
        "The Dark Side Of The Moon (2023)(50th Anniversary)(Multichannel) [DSD64 - SACD Sony Records Int'l Japan 2023]",
        floyd,
        "/m/dsotm50/Multichannel",
    );
    // Avant la réunion, aucun des deux disques n'est replié.
    let (replies, _) = membres(&b).await;
    assert!(
        !replies.contains(&stereo) && !replies.contains(&multi),
        "{replies:?}"
    );

    let (statut, corps) = appeler(
        &b.state,
        "POST",
        "/api/v1/library/albums/coffret",
        Some(json!({ "album_ids": [stereo, multi] })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["cible"], json!(stereo));
    // Le nom proposé garde sa parenthèse fermante.
    assert_eq!(
        corps["titre"],
        json!("The Dark Side Of The Moon (2023)(50th Anniversary)")
    );

    let (replies, _) = membres(&b).await;
    assert!(
        !replies.contains(&stereo),
        "le coffret est replié sous une autre carte : {replies:?}"
    );
    let (statut, fiche) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{stereo}/versions"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    assert_eq!(ids(&fiche), vec![stereo], "{fiche}");
    assert_eq!(fiche["method"], json!("single"), "{fiche}");
    // L'œuvre DSOTM garde ses éditions : le coffret n'y entre pas, et il
    // n'en retire rien.
    let (statut, dsotm) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{}/versions", b.dsotm),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{dsotm}");
    let mut attendu = vec![b.dsotm, b.dsotm_sacd];
    attendu.sort();
    assert_eq!(ids(&dsotm), attendu, "{dsotm}");

    for url in [
        "/api/v1/library/albums?limit=500&sort=title".to_string(),
        "/api/v1/library/albums?limit=500&sort=title&q=The%20Dark%20Side%20Of%20The%20Moon%20(2"
            .to_string(),
    ] {
        let (statut, liste) = appeler(&b.state, "GET", &url, None).await;
        assert_eq!(statut, StatusCode::OK, "{liste}");
        let rendus: Vec<i64> = liste["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["id"].as_i64())
            .collect();
        assert!(
            rendus.contains(&stereo),
            "{url} : coffret absent de {rendus:?}"
        );
        assert!(
            !rendus.contains(&multi),
            "{url} : disque absorbé encore rendu"
        );
    }
}

/// La contre-épreuve : le MÊME album, sans marqueur de coffret, sous le même
/// titre, rejoint l'œuvre « The Dark Side of the Moon » — c'est exactement ce
/// qui retirait le coffret de la grille avant #8953. Sans elle, le témoin
/// pourrait passer parce que le titre ne grouperait de toute façon pas.
#[tokio::test]
async fn sans_marqueur_le_meme_titre_rejoint_l_oeuvre_contre_epreuve_8953() {
    let b = banc();
    let floyd = artiste_de(&b, b.dsotm);
    let stereo = disque_dsd(
        &b,
        "The Dark Side Of The Moon (2023)(50th Anniversary)(Stereo) [DSD64 - SACD Sony Records Int'l Japan 2023]",
        floyd,
        "/m/dsotm50/Stereo",
    );
    let multi = disque_dsd(
        &b,
        "The Dark Side Of The Moon (2023)(50th Anniversary)(Multichannel) [DSD64 - SACD Sony Records Int'l Japan 2023]",
        floyd,
        "/m/dsotm50/Multichannel",
    );
    let (statut, corps) = appeler(
        &b.state,
        "POST",
        "/api/v1/library/albums/coffret",
        Some(json!({ "album_ids": [stereo, multi] })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    b.state
        .backend
        .execute(
            "DELETE FROM album_metadata WHERE album_id = ?1 AND key = 'coffret'",
            &[&stereo as &dyn ToSqlValue],
        )
        .unwrap();
    let (statut, fiche) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{stereo}/versions"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    assert!(
        ids(&fiche).contains(&b.dsotm),
        "sans marqueur, le coffret doit rejoindre l'œuvre DSOTM : {fiche}"
    );
}

/// Un album de DSOTM, du groupe MusicBrainz de l'œuvre, marqué coffret
/// d'origine `origine`.
fn coffret_marque(b: &Banc, origine: &str) -> i64 {
    let floyd = artiste_de(b, b.dsotm);
    let id = album(
        &b.state,
        "The Dark Side of the Moon (Immersion Box Set)",
        floyd,
        Some("rg-dsotm"),
        48_000,
    );
    let marqueur = json!({ "origine": origine, "cle": "dsotm-immersion" }).to_string();
    tune_core::db::album_metadata_repo::AlbumMetadataRepo::with_backend(b.state.backend.clone())
        .set(id, tune_core::db::coffrets_auto::CLE_COFFRET, &marqueur)
        .unwrap();
    id
}

async fn ids_de_l_oeuvre(b: &Banc, album: i64) -> Vec<i64> {
    let (statut, fiche) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{album}/versions"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    ids(&fiche)
}

/// Relecture de #9217 — un coffret AUTO (« Disc 1 / Disc 2 » réunis) est un
/// album à plusieurs disques ordinaire : avec le groupe MusicBrainz de
/// l'œuvre, il y reste (#7617).
#[tokio::test]
async fn un_coffret_auto_du_groupe_mb_reste_dans_son_oeuvre_8953() {
    let b = banc();
    let auto = coffret_marque(&b, "auto");
    assert!(ids_de_l_oeuvre(&b, b.dsotm).await.contains(&auto));
}

/// Contre-épreuve : le même album, marqué coffret MANUEL, sort de l'œuvre.
/// Sans elle, le témoin auto passerait aussi si le marqueur était ignoré.
#[tokio::test]
async fn le_meme_album_en_coffret_manuel_sort_de_l_oeuvre_contre_epreuve_8953() {
    let b = banc();
    let manuel = coffret_marque(&b, "manuel");
    assert!(!ids_de_l_oeuvre(&b, b.dsotm).await.contains(&manuel));
    assert_eq!(ids_de_l_oeuvre(&b, manuel).await, vec![manuel]);
}

// ── Fil 3012 : le master Discogs, une seule liste ──────────────────────────

fn d(mut cand: Candidat, master: &str) -> Candidat {
    cand.discogs_master = Some(master.to_string());
    cand
}

#[test]
fn discogs_deux_editions_sans_mb_du_meme_master_sont_regroupees_3012() {
    let albums = [
        d(c(1, "Physical Graffiti", Some(9), None), "12345"),
        d(c(2, "フィジカル・グラフィティ", Some(9), None), "12345"),
        c(3, "Presence", Some(9), None),
    ];
    let o = grouper(&albums);
    assert_eq!(partition(&o), vec![vec![1, 2], vec![3]]);
    assert_eq!(oeuvre_de(&o, 1).methode, Methode::DiscogsMaster);
    assert_eq!(oeuvre_de(&o, 1).work_id, "dg:12345");
}

/// Contre-épreuve : les mêmes albums sans master restent séparés (titres
/// différents) — c'est bien le master qui les réunit.
#[test]
fn sans_master_les_memes_editions_restent_separees_contre_epreuve_3012() {
    let albums = [
        c(1, "Physical Graffiti", Some(9), None),
        c(2, "フィジカル・グラフィティ", Some(9), None),
        c(3, "Presence", Some(9), None),
    ];
    assert_eq!(
        partition(&grouper(&albums)),
        vec![vec![1], vec![2], vec![3]]
    );
}

#[test]
fn discogs_deux_masters_differents_restent_separes_meme_au_meme_titre_3012() {
    let albums = [
        d(c(1, "Live", Some(9), None), "111"),
        d(c(2, "Live", Some(9), None), "222"),
    ];
    assert_eq!(partition(&grouper(&albums)), vec![vec![1], vec![2]]);
    // Contre-épreuve : sans master, le titre les réunit.
    let nus = [c(1, "Live", Some(9), None), c(2, "Live", Some(9), None)];
    assert_eq!(partition(&grouper(&nus)), vec![vec![1, 2]]);
}

/// Dimitri (fil 3012, 11 h 17) : UNE liste. A (MB = x, Discogs = y) relie B
/// (MB = x seulement) et C (Discogs = y seulement), même sous trois titres.
#[test]
fn un_album_mb_et_discogs_relie_deux_editions_qui_n_ont_qu_une_cle_3012() {
    let albums = [
        d(c(1, "Physical Graffiti", Some(9), Some("rg-pg")), "12345"),
        c(2, "Physical Graffiti [Deluxe]", Some(9), Some("rg-pg")),
        d(
            c(3, "Physical Graffiti (Japan 32XD-566/7)", Some(9), None),
            "12345",
        ),
    ];
    let o = grouper(&albums);
    assert_eq!(partition(&o), vec![vec![1, 2, 3]]);
    assert_eq!(oeuvre_de(&o, 3).work_id, "mb:rg-pg");
    assert_eq!(oeuvre_de(&o, 3).methode, Methode::MusicBrainz);
    // L'ordre d'arrivée ne change rien.
    let inverse = [albums[2].clone(), albums[1].clone(), albums[0].clone()];
    assert_eq!(partition(&grouper(&inverse)), vec![vec![1, 2, 3]]);
}

/// Contre-épreuve : sans le pont (A sans master), C reste à part.
#[test]
fn sans_l_album_pont_l_edition_discogs_reste_a_part_contre_epreuve_3012() {
    let albums = [
        c(1, "Physical Graffiti", Some(9), Some("rg-pg")),
        c(2, "Physical Graffiti [Deluxe]", Some(9), Some("rg-pg")),
        d(
            c(3, "Physical Graffiti (Japan 32XD-566/7)", Some(9), None),
            "12345",
        ),
    ];
    assert_eq!(partition(&grouper(&albums)), vec![vec![1, 2], vec![3]]);
}

#[test]
fn des_groupes_mb_differents_ne_fusionnent_jamais_par_discogs_3012() {
    let albums = [
        d(c(1, "Physical Graffiti", Some(9), Some("rg-x")), "12345"),
        d(c(2, "Physical Graffiti", Some(9), Some("rg-z")), "12345"),
        d(c(3, "Physical Graffiti (Japan)", Some(9), None), "12345"),
        c(4, "Physical Graffiti", Some(9), None),
    ];
    let o = grouper(&albums);
    let p = partition(&o);
    assert!(
        !p.iter().any(|g| g.contains(&1) && g.contains(&2)),
        "deux groupes MusicBrainz fusionnés : {p:?}"
    );
    // Le master, ambigu, ne relie l'album 3 à aucun des deux ; le titre ne
    // relie pas l'album 4 (trois clés possibles).
    assert_eq!(p, vec![vec![1], vec![2], vec![3], vec![4]]);
}

/// Le repli par le titre ne vaut que pour un album sans clé, et ne fusionne
/// jamais deux composantes.
#[test]
fn le_repli_par_le_titre_ne_fusionne_pas_deux_composantes_3012() {
    let albums = [
        c(1, "Physical Graffiti", Some(9), Some("rg-x")),
        d(c(2, "Physical Graffiti", Some(9), None), "12345"),
        c(3, "Physical Graffiti (Remaster)", Some(9), None),
    ];
    assert_eq!(
        partition(&grouper(&albums)),
        vec![vec![1], vec![2], vec![3]]
    );
    // Un album sans clé rejoint la SEULE composante de son titre, Discogs
    // compris.
    let seule = [
        d(c(2, "Physical Graffiti", Some(9), None), "12345"),
        c(3, "Physical Graffiti (Remaster)", Some(9), None),
    ];
    let o = grouper(&seule);
    assert_eq!(partition(&o), vec![vec![2, 3]]);
    assert_eq!(oeuvre_de(&o, 3).work_id, "dg:12345");
}

// ── Fil 3012 : les routes ──────────────────────────────────────────────────

async fn fiche(b: &Banc, album: i64) -> Value {
    let (statut, corps) = appeler(
        &b.state,
        "GET",
        &format!("/api/v1/library/albums/{album}/versions"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps
}

async fn poser_master_album(b: &Banc, album: i64, valeur: &str) -> (StatusCode, Value) {
    appeler(
        &b.state,
        "PUT",
        &format!("/api/v1/library/albums/{album}/metadata"),
        Some(json!({ "discogs_master_id": valeur })),
    )
    .await
}

/// Un greffon pose le master sur l'ALBUM, sous toutes ses formes : deux
/// éditions sans MusicBrainz, de titres sans rapport, deviennent UNE œuvre —
/// dans la fiche comme dans la grille. Vide : effacé, elles se séparent.
#[tokio::test]
async fn route_un_greffon_pose_le_master_sur_l_album_et_les_editions_se_rejoignent_3012() {
    let b = banc();
    let zep = artiste(&b.state, "Led Zeppelin");
    let pg = album(&b.state, "Physical Graffiti", zep, None, 44_100);
    let jp = album(&b.state, "フィジカル・グラフィティ", zep, None, 44_100);
    assert_eq!(ids(&fiche(&b, pg).await), vec![pg], "avant : seul");

    let (s1, r1) = poser_master_album(
        &b,
        pg,
        "https://www.discogs.com/master/12345-Led-Zeppelin-Physical-Graffiti",
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "{r1}");
    let (s2, r2) = poser_master_album(&b, jp, "m12345").await;
    assert_eq!(s2, StatusCode::OK, "{r2}");

    let o = fiche(&b, jp).await;
    let mut attendu = vec![pg, jp];
    attendu.sort();
    assert_eq!(ids(&o), attendu, "{o}");
    assert_eq!(o["method"], json!("discogs_master"), "{o}");
    assert_eq!(o["work_id"], json!("dg:12345"), "{o}");
    let (replies, gardes) = membres(&b).await;
    assert!(
        (replies.contains(&pg) && gardes.contains(&jp))
            || (replies.contains(&jp) && gardes.contains(&pg)),
        "la grille ne les réunit pas : {replies:?} / {gardes:?}"
    );
    // Rangé en chiffres.
    let rangee = b
        .state
        .backend
        .query_one(
            "SELECT value FROM album_metadata WHERE album_id = ?1 AND key = 'discogs_master_id'",
            &[&pg as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_string();
    assert_eq!(rangee.as_deref(), Some("12345"));

    // Effacer : valeur vide.
    let (s3, r3) = poser_master_album(&b, jp, "").await;
    assert_eq!(s3, StatusCode::OK, "{r3}");
    assert_eq!(ids(&fiche(&b, jp).await), vec![jp]);
}

/// Contre-épreuve : une valeur qui n'est pas un master (une RELEASE) est
/// refusée et rien n'est écrit.
#[tokio::test]
async fn route_une_release_n_est_pas_acceptee_comme_master_3012() {
    let b = banc();
    let (statut, corps) =
        poser_master_album(&b, b.abbey, "https://www.discogs.com/release/999-x").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    let n = b
        .state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM album_metadata WHERE key = 'discogs_master_id'",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64();
    assert_eq!(n, Some(0));
}

/// Une piste locale de l'album.
fn piste(b: &Banc, album: i64, chemin: &str) -> i64 {
    b.state
        .backend
        .execute(
            "INSERT INTO tracks (title, album_id, file_path, source, track_number, disc_number, \
             format) VALUES ('piste', ?1, ?2, 'local', 1, 1, 'flac')",
            &[&album as &dyn ToSqlValue, &chemin],
        )
        .unwrap();
    b.state.backend.last_insert_rowid()
}

/// La porte de la balise VERSION (`PUT /tracks/{id}/metadata`) porte aussi
/// le master : l'album prend la valeur commune de ses pistes.
#[tokio::test]
async fn route_le_master_pose_sur_les_pistes_regroupe_les_albums_3012() {
    let b = banc();
    let zep = artiste(&b.state, "Led Zeppelin");
    let pg = album(&b.state, "Physical Graffiti", zep, None, 44_100);
    let jp = album(&b.state, "フィジカル・グラフィティ", zep, None, 44_100);
    let t1 = piste(&b, pg, "/m/pg/01.flac");
    let t2 = piste(&b, jp, "/m/jp/01.flac");
    for t in [t1, t2] {
        let (statut, corps) = appeler(
            &b.state,
            "PUT",
            &format!("/api/v1/library/tracks/{t}/metadata"),
            Some(json!({ "discogs_master_id": "[m12345]", "version": "Japan · 1987" })),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
    }
    let o = fiche(&b, pg).await;
    let mut attendu = vec![pg, jp];
    attendu.sort();
    assert_eq!(ids(&o), attendu, "{o}");
    // Contre-épreuve : une piste d'un AUTRE master rend la valeur de l'album
    // ambiguë, il n'en prend aucune.
    let t3 = piste(&b, jp, "/m/jp/02.flac");
    let (statut, _) = appeler(
        &b.state,
        "PUT",
        &format!("/api/v1/library/tracks/{t3}/metadata"),
        Some(json!({ "discogs_master_id": "777" })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(ids(&fiche(&b, pg).await), vec![pg]);
}

/// La fiche suit la composante de proche en proche, comme la grille : B
/// (MB seulement) et C (Discogs seulement, AUTRE artiste) se voient par A.
#[tokio::test]
async fn route_la_fiche_suit_la_composante_mb_et_discogs_3012() {
    let b = banc();
    let zep = artiste(&b.state, "Led Zeppelin");
    let autre = artiste(&b.state, "Led Zeppelin (2)");
    let a = album(&b.state, "Physical Graffiti", zep, Some("rg-pg"), 44_100);
    let bb = album(
        &b.state,
        "Physical Graffiti [Deluxe]",
        zep,
        Some("rg-pg"),
        96_000,
    );
    let cc = album(&b.state, "Physical Graffiti (Japan)", autre, None, 44_100);
    poser_master_album(&b, a, "12345").await;
    poser_master_album(&b, cc, "12345").await;
    let mut attendu = vec![a, bb, cc];
    attendu.sort();
    for depuis in [a, bb, cc] {
        let o = fiche(&b, depuis).await;
        assert_eq!(ids(&o), attendu, "depuis {depuis} : {o}");
        assert_eq!(o["work_id"], json!("mb:rg-pg"));
    }
}

/// Un disque d'une sortie, une piste, sous son dossier.
fn disque(b: &Banc, titre: &str, artiste: i64, dossier: &str) -> i64 {
    let id = album(&b.state, titre, artiste, None, 44_100);
    piste(b, id, &format!("{dossier}/01.flac"));
    id
}

async fn composer(b: &Banc, ids_: &[i64], titre: Option<&str>) -> i64 {
    let (statut, corps) = appeler(
        &b.state,
        "POST",
        "/api/v1/library/albums/coffret",
        Some(json!({ "album_ids": ids_, "titre": titre })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps["cible"].as_i64().unwrap()
}

/// Dimitri : « Physical Graffiti (Disc 1) » et « (Disc 2) » réunis à la main
/// sont UNE édition — elle rejoint les Versions, et compte pour une.
#[tokio::test]
async fn un_coffret_manuel_des_disques_d_une_sortie_entre_dans_les_versions_3012() {
    let b = banc();
    let zep = artiste(&b.state, "Led Zeppelin");
    let remaster = album(
        &b.state,
        "Physical Graffiti (2015 Remaster)",
        zep,
        None,
        96_000,
    );
    let d1 = disque(&b, "Physical Graffiti (Disc 1)", zep, "/m/pg-jp/CD1");
    let d2 = disque(&b, "Physical Graffiti (Disc 2)", zep, "/m/pg-jp/CD2");
    let coffret = composer(&b, &[d1, d2], None).await;
    let o = fiche(&b, coffret).await;
    let mut attendu = vec![coffret, remaster];
    attendu.sort();
    assert_eq!(ids(&o), attendu, "{o}");
    assert_eq!(
        o["count"],
        json!(2),
        "le coffret compte pour UNE version : {o}"
    );
    let o = fiche(&b, remaster).await;
    assert_eq!(ids(&o), attendu, "{o}");
}

/// Contre-épreuve : un coffret manuel de DEUX sorties reste sa propre œuvre
/// (#8953), même sous un titre qui grouperait.
#[tokio::test]
async fn un_coffret_manuel_de_deux_sorties_reste_seul_contre_epreuve_3012() {
    let b = banc();
    let zep = artiste(&b.state, "Led Zeppelin");
    let remaster = album(
        &b.state,
        "Physical Graffiti (2015 Remaster)",
        zep,
        None,
        96_000,
    );
    let d1 = disque(&b, "Physical Graffiti (Disc 1)", zep, "/m/box/CD1");
    let d2 = disque(&b, "Houses of the Holy", zep, "/m/box/CD2");
    // Le nom saisi grouperait par le titre avec le remaster : seule la
    // règle des coffrets l'en tient à l'écart.
    let coffret = composer(&b, &[d1, d2], Some("Physical Graffiti")).await;
    let o = fiche(&b, coffret).await;
    assert_eq!(ids(&o), vec![coffret], "{o}");
    assert_eq!(ids(&fiche(&b, remaster).await), vec![remaster]);
}
