//! #8680 (fil 2800) — groupes enregistrés qui se chevauchent : « playing
//! everywhere or nowhere ».
//!
//! Le réglage du testeur (rc11, groupes empilés avant la rc11) : la zone 10
//! dans 23 groupes, la zone 11 suiveuse de 10 ET meneuse d'un groupe (16, 23,
//! 9), la zone 9 dans les deux. Une lecture lancée sur 10 allumait 9 et 11,
//! puis le `started` de 11 relayait à 16 et 23 : toutes les pièces.
//!
//! Banc : zones DLNA factices (`MockOutput`), une vraie piste FLAC en file,
//! le relais lancé comme au démarrage. Aucun son.
//!
//! Contre-épreuves (chacune fait tomber son essai) :
//! - `groupes_de` rendant `groupes_bruts` (le relais lit le réglage tel quel)
//!   fait tomber `une_lecture_sur_la_meneuse_ne_part_pas_en_cascade` : la zone
//!   « Chambre », d'un autre groupe, reçoit la piste ;
//! - retirer l'appel à `normaliser_les_groupes_au_demarrage` de
//!   `recalculer_les_sorties_doubles` fait tomber
//!   `le_demarrage_remet_les_groupes_dans_la_regle` ;
//! - revenir à `groups[i]["zone_ids"] = json!(zone_ids)` dans `patch_group`
//!   fait tomber `etendre_un_groupe_retire_ses_zones_des_autres_groupes`.

use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

use super::relais_de_groupe_6155::{
    groupes_dans_la_regle, lancer_le_relais_de_groupe, meneuses_de, zones_suiveuses,
};
use super::{PatchGroup, patch_group, recalculer_les_sorties_doubles};
use crate::state::AppState;

fn groupe(id: i64, meneuse: i64, zones: &[i64]) -> Value {
    json!({ "id": id, "group_id": id.to_string(), "name": format!("G{id}"),
            "leader_id": meneuse, "zone_ids": zones })
}

/// Le réglage du fil 2800 : vingt groupes {10, 9} empilés, puis 11 qui mène
/// (16, 23, 9), puis le dernier groupe formé depuis la fiche de 10.
fn reglage_du_fil_2800(z10: i64, z9: i64, z11: i64, z16: i64, z23: i64) -> Vec<Value> {
    let mut g: Vec<Value> = (1..=20).map(|id| groupe(id, z10, &[z10, z9])).collect();
    g.push(groupe(21, z11, &[z11, z16, z23, z9]));
    g.push(groupe(22, z10, &[z10, z9, z11]));
    g
}

fn lire_les_groupes(state: &AppState) -> Vec<Value> {
    tune_core::zones::groupes_enregistres::lire(&SettingsRepo::with_backend(state.backend.clone()))
}

fn poser_les_groupes(state: &AppState, groupes: &[Value]) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("zone_groups", &serde_json::to_string(groupes).unwrap())
        .unwrap();
}

/// Chaque zone n'apparaît que dans un groupe au plus.
fn une_zone_un_groupe(groupes: &[Value]) -> bool {
    let mut toutes: Vec<i64> = groupes
        .iter()
        .flat_map(|g| {
            g["zone_ids"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    let n = toutes.len();
    toutes.sort_unstable();
    toutes.dedup();
    toutes.len() == n
}

// --- Règle pure du relais -----------------------------------------------------

#[test]
fn une_suiveuse_ne_mene_rien_et_ne_suit_qu_une_meneuse() {
    let g = groupes_dans_la_regle(reglage_du_fil_2800(10, 9, 11, 16, 23));
    assert_eq!(zones_suiveuses(&g, 10), vec![9, 11]);
    assert!(
        zones_suiveuses(&g, 11).is_empty(),
        "11 suit 10 : son `started` ne doit rien relayer (cascade du fil 2800)"
    );
    assert_eq!(meneuses_de(&g, 9), vec![10], "9 ne suit qu'une meneuse");
    assert!(une_zone_un_groupe(&g), "{g:?}");
    // 16 et 23 restent groupées entre elles, sous une meneuse du groupe.
    assert_eq!(zones_suiveuses(&g, 16), vec![23]);
}

// --- Démarrage ----------------------------------------------------------------

async fn serveur() -> AppState {
    let state = AppState::new(":memory:", 0, crate::config::TuneConfig::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    state
}

#[tokio::test]
async fn le_demarrage_remet_les_groupes_dans_la_regle() {
    let state = serveur().await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for nom in ["Salon", "Cuisine", "Bureau", "Chambre", "Salon AirPlay"] {
        ids.push(
            repo.create(nom, Some("dlna"), Some(&format!("uuid:8680-{nom}")))
                .unwrap(),
        );
    }
    let (z10, z9, z11, z16, z23) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    let mut reglage = reglage_du_fil_2800(z10, z9, z11, z16, z23);
    // Un des vingt groupes empilés portait une sortie double sous une meneuse
    // (zone 9) qui ne mènera plus rien : son réglage interne doit partir.
    reglage[3] = json!({ "id": 4, "group_id": "4", "leader_id": z9,
                         "zone_ids": [z9, z10], "sortie_double": {"suiveurs": []} });
    let cle = tune_core::outputs::sortie_double::cle_de_zone(z9);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set(&cle, "[\"local:fantome\"]").unwrap();
    poser_les_groupes(&state, &reglage);

    recalculer_les_sorties_doubles(&state).await;

    let groupes = lire_les_groupes(&state);
    assert!(
        une_zone_un_groupe(&groupes),
        "après le démarrage, une zone n'est que dans un groupe : {groupes:?}"
    );
    assert_eq!(groupes.len(), 2, "{groupes:?}");
    assert_eq!(
        groupes[1]["id"],
        json!(22),
        "le plus récent garde 10, 9, 11"
    );
    assert_eq!(groupes[1]["zone_ids"], json!([z10, z9, z11]));
    assert_eq!(groupes[0]["zone_ids"], json!([z16, z23]));
    assert_eq!(
        groupes[0]["leader_id"],
        json!(z16),
        "la meneuse 11, partie, cède sa place"
    );
    assert_eq!(
        settings.get(&cle).unwrap(),
        None,
        "la sortie double d'une meneuse qui ne mène plus rien est retirée"
    );

    // Idempotent : un second démarrage ne réécrit rien.
    let texte = settings.get("zone_groups").unwrap();
    recalculer_les_sorties_doubles(&state).await;
    assert_eq!(settings.get("zone_groups").unwrap(), texte);
}

// --- PATCH d'un groupe --------------------------------------------------------

fn en_francais() -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(axum::http::header::ACCEPT_LANGUAGE, "fr".parse().unwrap());
    h
}

async fn patcher(state: &AppState, gid: i64, corps: Value) -> u16 {
    let corps: PatchGroup = serde_json::from_value(corps).unwrap();
    patch_group(State(state.clone()), Path(gid), en_francais(), Json(corps))
        .await
        .ok()
        .unwrap()
        .into_response()
        .status()
        .as_u16()
}

#[tokio::test]
async fn etendre_un_groupe_retire_ses_zones_des_autres_groupes() {
    let state = serveur().await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut z = Vec::new();
    for nom in ["A", "B", "C", "D", "E"] {
        z.push(
            repo.create(nom, Some("dlna"), Some(&format!("uuid:8680-p-{nom}")))
                .unwrap(),
        );
    }
    let (a, b, c, d, e) = (z[0], z[1], z[2], z[3], z[4]);
    poser_les_groupes(&state, &[groupe(1, a, &[a, b]), groupe(2, c, &[c, d, e])]);

    // Le groupe 1 prend C : C quitte le groupe 2.
    assert_eq!(
        patcher(&state, 1, json!({ "zone_ids": [a, b, c] })).await,
        200
    );
    let groupes = lire_les_groupes(&state);
    assert!(une_zone_un_groupe(&groupes), "{groupes:?}");
    assert_eq!(
        groupes,
        vec![groupe(2, d, &[d, e]), groupe(1, a, &[a, b, c]),],
        "C quitte le groupe 2, qui change de meneuse ; le groupe 1 passe en fin de liste"
    );

    // Le groupe 1 prend D, et perd sa meneuse A : le groupe 2 tombe à une
    // zone et disparaît, la meneuse devient B.
    assert_eq!(
        patcher(&state, 1, json!({ "zone_ids": [b, c, d, d] })).await,
        200
    );
    let groupes = lire_les_groupes(&state);
    assert_eq!(groupes, vec![groupe(1, b, &[b, c, d])]);

    // Une composition de moins de deux zones est refusée, comme à la création.
    assert_eq!(patcher(&state, 1, json!({ "zone_ids": [b, b] })).await, 400);
    assert_eq!(lire_les_groupes(&state), vec![groupe(1, b, &[b, c, d])]);
}

#[tokio::test]
async fn le_gestionnaire_de_zones_applique_la_meme_regle() {
    let state = serveur().await;
    let app = crate::routes::router(state.clone());
    poser_les_groupes(&state, &[groupe(1, 1, &[1, 2]), groupe(2, 3, &[3, 4])]);
    let rep = app
        .oneshot(
            Request::patch("/api/v1/zone-manager/groups/1")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "zone_ids": [1, 2, 3] }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep.status(), StatusCode::OK);
    assert_eq!(lire_les_groupes(&state), vec![groupe(1, 1, &[1, 2, 3])]);
}

// --- Relais de bout en bout ---------------------------------------------------

struct Banc {
    state: AppState,
    z10: i64,
    z9: i64,
    z11: i64,
    z16: i64,
    _dir: tempfile::TempDir,
}

const NOMS: [&str; 4] = ["Salon", "Cuisine", "Bureau", "Chambre"];

async fn banc() -> Banc {
    let state = serveur().await;
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste.flac");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/test.flac"
        ),
        &chemin,
    )
    .unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                                 duration_ms, sample_rate, bit_depth, channels, source) \
             VALUES (1, 'Canopée', 1, 1, '{}', 'flac', 300000, 44100, 16, 2, 'local');",
            chemin.to_string_lossy()
        ))
        .unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for nom in NOMS {
        let appareil = format!("uuid:8680-{nom}");
        ids.push(repo.create(nom, Some("dlna"), Some(&appareil)).unwrap());
        state
            .outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new(&appareil, nom).with_type("dlna")));
    }
    let (z10, z9, z11, z16) = (ids[0], ids[1], ids[2], ids[3]);
    // Le réglage d'avant la rc11, tel quel (le relais ne doit pas dépendre
    // de la normalisation du démarrage) : 11 suit 10 et mène 16 et 9.
    poser_les_groupes(
        &state,
        &[
            groupe(21, z11, &[z11, z16, z9]),
            groupe(22, z10, &[z10, z9, z11]),
        ],
    );
    lancer_le_relais_de_groupe(&state);
    let app = crate::routes::router(state.clone());
    let rep = app
        .oneshot(
            Request::post(format!("/api/v1/zones/{z10}/queue/add"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "track_ids": [1] }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep.status(), StatusCode::CREATED);
    Banc {
        state,
        z10,
        z9,
        z11,
        z16,
        _dir: dir,
    }
}

impl Banc {
    async fn joues_par(&self, nom: &str) -> Vec<String> {
        let registre = self.state.outputs.lock().await;
        let sortie = registre.get(&format!("uuid:8680-{nom}")).unwrap();
        let garde = sortie.lock().await;
        garde
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_titles()
            .await
    }
}

#[tokio::test]
async fn une_lecture_sur_la_meneuse_ne_part_pas_en_cascade() {
    let banc = banc().await;
    banc.state
        .orchestrator
        .play_from_queue(banc.z10, 0)
        .await
        .expect("lecture sur la meneuse");

    // Les deux suiveuses de 10 la rejoignent.
    let mut rejointes = false;
    for _ in 0..200 {
        if banc.joues_par("Cuisine").await.len() == 1 && banc.joues_par("Bureau").await.len() == 1 {
            rejointes = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        rejointes,
        "9 et 11 suivent 10 : Cuisine {:?}, Bureau {:?}",
        banc.joues_par("Cuisine").await,
        banc.joues_par("Bureau").await
    );
    // Laisser au relais le temps de « cascader » s'il le devait.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        banc.joues_par("Chambre").await.is_empty(),
        "la zone {} (Chambre) n'est pas du groupe de 10 : le `started` de 11, \
         suiveuse, ne relaie rien (cascade du fil 2800) — reçu {:?}",
        banc.z16,
        banc.joues_par("Chambre").await
    );
    assert_eq!(
        banc.joues_par("Cuisine").await.len(),
        1,
        "la zone {} (Cuisine), déjà sur la piste, n'est pas relancée",
        banc.z9
    );
    assert_eq!(banc.joues_par("Bureau").await.len(), 1, "zone {}", banc.z11);
    assert_eq!(
        banc.joues_par("Salon").await.len(),
        1,
        "la meneuse {} n'est pas relancée",
        banc.z10
    );
}
