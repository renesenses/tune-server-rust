//! #7974 — le bail d'écoute au niveau des routes, sur une zone DLNA factice
//! (`MockOutput`), sans son.
//!
//! - l'épisode de podcast sur une sortie réseau ne passe pas par `play()` :
//!   sa route demande le bail elle-même et refuse en 403
//!   `parental_lease_refused`, sans rien envoyer à l'appareil ;
//! - le profil qui fait le geste devient celui de la session : un adulte qui
//!   saute dans la file ou lance un épisode après l'enfant n'hérite pas de
//!   son bail, et l'enfant qui le fait après l'adulte ne passe pas ;
//! - un préréglage épinglé lancé dans un groupe suit la même règle.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::politique_parentale::{PolitiqueParentale, poser};
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

use crate::state::AppState;

const APPAREIL: &str = "uuid:salon-bail-7974";
/// Le profil d'une requête sans `X-Profile-Id`, sur une base vierge.
const ADULTE: i64 = 1;

struct Banc {
    state: AppState,
    zone: i64,
    enfant: i64,
}

/// Une zone DLNA, une piste en file, un enfant au budget du jour épuisé.
async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin 7974", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    let piste = TrackRepo::with_backend(state.backend.clone())
        .create(&tune_core::db::models::Track::new("Titre 7974".into()))
        .unwrap();
    PlayQueueRepo::with_backend(state.backend.clone())
        .set_queue(zone, &[piste])
        .unwrap();
    let enfant = ProfileRepo::with_backend(state.backend.clone())
        .create("enfant-7974", None, None)
        .unwrap();
    poser(
        &state.backend,
        enfant,
        ADULTE,
        0,
        &PolitiqueParentale {
            budget_quotidien_minutes: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    Banc {
        state,
        zone,
        enfant,
    }
}

async fn appel(b: &Banc, uri: String, profil: Option<i64>, corps: Value) -> (StatusCode, Value) {
    let mut req = Request::post(uri).header("content-type", "application/json");
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p.to_string());
    }
    let reponse = crate::routes::router(b.state.clone())
        .oneshot(req.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn episode(b: &Banc, profil: Option<i64>) -> (StatusCode, Value) {
    appel(
        b,
        format!("/api/v1/podcasts/play/{}", b.zone),
        profil,
        json!({"audio_url": "http://127.0.0.1:9/episode-7974.mp3", "title": "Épisode"}),
    )
    .await
}

async fn sauter(b: &Banc, profil: Option<i64>) -> (StatusCode, Value) {
    appel(
        b,
        format!("/api/v1/zones/{}/queue/jump", b.zone),
        profil,
        json!({"position": 0}),
    )
    .await
}

async fn envois_a_l_appareil(b: &Banc) -> usize {
    let registre = b.state.outputs.lock().await;
    let sortie = registre.get(APPAREIL).expect("sortie enregistrée");
    let garde = sortie.lock().await;
    let mock = garde.as_any().downcast_ref::<MockOutput>().unwrap();
    mock.play_mime_types().await.len() + mock.play_titles().await.len()
}

fn refuse(statut: StatusCode, corps: &Value) -> bool {
    statut == StatusCode::FORBIDDEN && corps["code"] == "parental_lease_refused"
}

/// L'épisode lancé par l'enfant sur la sortie réseau : 403 dédié, rien
/// n'atteint l'appareil.
#[tokio::test]
async fn episode_sur_sortie_reseau_refuse_a_l_enfant_7974() {
    let b = banc().await;
    let (statut, corps) = episode(&b, Some(b.enfant)).await;
    assert!(
        refuse(statut, &corps),
        "budget épuisé : 403 parental_lease_refused attendu, reçu {statut} {corps}"
    );
    assert_eq!(corps["reason"], "budget_quotidien_epuise", "{corps}");
    assert_eq!(
        envois_a_l_appareil(&b).await,
        0,
        "rien n'est envoyé à l'appareil"
    );
}

/// L'adulte qui lance un épisode juste après l'enfant n'hérite pas de son
/// bail.
#[tokio::test]
async fn episode_de_l_adulte_apres_l_enfant_non_refuse_7974() {
    let b = banc().await;
    let _ = episode(&b, Some(b.enfant)).await;
    let (statut, corps) = episode(&b, None).await;
    assert!(
        !refuse(statut, &corps),
        "l'adulte n'a pas de politique : aucun refus, reçu {statut} {corps}"
    );
}

/// Sauter dans la file : l'enfant après l'adulte est refusé, l'adulte après
/// l'enfant ne l'est pas.
#[tokio::test]
async fn saut_dans_la_file_suit_le_profil_du_geste_7974() {
    let b = banc().await;
    b.state
        .playback
        .set_session_profile(b.zone, Some(ADULTE))
        .await;
    let (statut, corps) = sauter(&b, Some(b.enfant)).await;
    assert!(
        refuse(statut, &corps),
        "l'enfant saute dans la file après l'adulte : 403 attendu, reçu {statut} {corps}"
    );

    b.state
        .playback
        .set_session_profile(b.zone, Some(b.enfant))
        .await;
    let (statut, corps) = sauter(&b, None).await;
    assert!(
        !refuse(statut, &corps),
        "l'adulte saute dans la file après l'enfant : aucun refus, reçu {statut} {corps}"
    );
}

/// La meneuse du groupe dont la zone du banc est suiveuse.
const MENEUSE: i64 = 9_974;

fn grouper(b: &Banc) {
    let groupes = json!([{ "id": 1, "leader_id": MENEUSE, "zone_ids": [MENEUSE, b.zone] }]);
    tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone())
        .set("zone_groups", &groupes.to_string())
        .unwrap();
}

fn meneuse_du_groupe(b: &Banc) -> Value {
    let texte = tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone())
        .get("zone_groups")
        .unwrap()
        .unwrap();
    serde_json::from_str::<Value>(&texte).unwrap()[0]["leader_id"].clone()
}

/// Dans un groupe (#7717), les routes passent par le relais de groupe : le
/// profil du geste doit le traverser. L'enfant est refusé sur l'épisode et le
/// saut dans la file, et la tête revient à la meneuse d'avant ; l'adulte qui
/// suit n'hérite pas du bail de l'enfant.
#[tokio::test]
async fn le_profil_du_geste_traverse_le_relais_de_groupe_7974() {
    let b = banc().await;
    grouper(&b);

    let (statut, corps) = episode(&b, Some(b.enfant)).await;
    assert!(
        refuse(statut, &corps),
        "épisode de l'enfant dans un groupe : 403 attendu, reçu {statut} {corps}"
    );
    assert_eq!(
        meneuse_du_groupe(&b),
        json!(MENEUSE),
        "refusé : la tête du groupe revient à la meneuse d'avant"
    );

    b.state
        .playback
        .set_session_profile(b.zone, Some(ADULTE))
        .await;
    let (statut, corps) = sauter(&b, Some(b.enfant)).await;
    assert!(
        refuse(statut, &corps),
        "saut de l'enfant dans un groupe : 403 attendu, reçu {statut} {corps}"
    );

    let (statut, corps) = episode(&b, None).await;
    assert!(
        !refuse(statut, &corps),
        "épisode de l'adulte après l'enfant dans un groupe : aucun refus, reçu {statut} {corps}"
    );
}

/// Un préréglage épinglé (stocké par Tune, la zone n'est pas OpenHome).
fn epingler(b: &Banc) {
    let pins = json!([{
        "index": 0,
        "title": "Radio 7974",
        "uri": "http://127.0.0.1:9/radio-7974.mp3",
        "type": "radio"
    }]);
    tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone())
        .set(&format!("zone_{}_pins", b.zone), &pins.to_string())
        .unwrap();
}

async fn lancer_le_prereglage(b: &Banc, profil: Option<i64>) -> (StatusCode, Value) {
    appel(
        b,
        format!("/api/v1/zones/{}/pins/0/invoke", b.zone),
        profil,
        json!({}),
    )
    .await
}

/// `invoke_zone_pin_en_tete_du_groupe` : dans un groupe, l'enfant qui lance
/// un préréglage après l'adulte est refusé et la tête revient à la meneuse
/// d'avant ; l'adulte qui le lance après l'enfant n'hérite pas de son bail.
#[tokio::test]
async fn prereglage_dans_un_groupe_suit_le_profil_du_geste_7974() {
    let b = banc().await;
    grouper(&b);
    epingler(&b);

    b.state
        .playback
        .set_session_profile(b.zone, Some(ADULTE))
        .await;
    let (statut, corps) = lancer_le_prereglage(&b, Some(b.enfant)).await;
    assert!(
        refuse(statut, &corps),
        "préréglage de l'enfant dans un groupe : 403 attendu, reçu {statut} {corps}"
    );
    assert_eq!(
        meneuse_du_groupe(&b),
        json!(MENEUSE),
        "refusé : la tête du groupe revient à la meneuse d'avant"
    );

    b.state
        .playback
        .set_session_profile(b.zone, Some(b.enfant))
        .await;
    let (statut, corps) = lancer_le_prereglage(&b, None).await;
    assert!(
        !refuse(statut, &corps),
        "préréglage de l'adulte après l'enfant dans un groupe : aucun refus, reçu {statut} {corps}"
    );
}
