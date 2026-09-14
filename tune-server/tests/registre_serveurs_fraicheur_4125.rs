//! #4125 — `reachable: false` sur trois serveurs multimédia parfaitement
//! joignables, et la date de dernière observation qui ne bouge plus jamais.
//!
//! # Le fait mesuré
//!
//! `GET http://192.168.1.18:8888/api/v1/network/media-servers`, le 14/09/2026,
//! deux relevés à 56 s d'intervalle :
//!
//! ```text
//! host:port            name                      reachable  presence  last_seen_secs  first_seen_at         last_seen_at
//! 192.168.1.41:26125   Asset UPnP: Mac-Studio-6  true       present   267             2026-09-14T09:15:45Z  2026-09-14T09:15:45Z
//! 192.168.1.42:8888    Tune Server               false      present   3021            2026-09-14T08:29:51Z  2026-09-14T08:29:51Z
//! 192.168.1.41:8888    Tune Server               false      present   2711            2026-09-14T08:35:01Z  2026-09-14T08:35:01Z
//! 192.168.1.15:8888    Tune Server               false      present   3021            2026-09-14T08:29:51Z  2026-09-14T08:29:51Z
//! ```
//!
//! Les trois répondent HTTP 200 sur leur `description.xml`, vérifié à la même
//! minute. Et `first_seen_at == last_seen_at` sur les QUATRE lignes, à la
//! seconde près : entre les deux relevés `last_seen_secs` a grandi de 56 s et
//! `last_seen_at` n'a pas bougé.
//!
//! # Ce que `reachable` mesurait, et ce qu'il mesure
//!
//! La route portait DEUX horloges pour une seule question. `presence` se
//! calcule sur l'âge du registre durable (`ServeurEnregistre::age_secs()`,
//! horloge absolue) ; `reachable` se calculait sur un SECOND `Instant`, celui
//! de `AppState::media_servers` — une copie gelée à la première découverte,
//! puisque son unique écrivain est l'évènement `MediaServerDiscovered` qui
//! n'est émis qu'alors. Quinze minutes après sa découverte, tout serveur
//! passait `reachable: false` et n'en revenait jamais.
//!
//! `reachable` se lit désormais sur le MÊME âge que `presence`. Il ne sonde
//! personne — ce n'est ni une requête HTTP, ni un `Browse` réussi : c'est
//! « le balayage SSDP l'a revu il y a moins de 900 s ».
//!
//! # Le témoin passe par les ROUTES MONTÉES
//!
//! `tune_server::routes::router`, et pas le gestionnaire en direct : « écrit
//! mais pas branché » est le défaut que ce dépôt connaît par cœur.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : sans l'entrée
//! `[[test]]` correspondante, ce fichier ne serait JAMAIS compilé et cette
//! garde serait verte contre rien.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::media_server_repo::{MediaServerRepo, ObservationServeurRecue};
use tune_server::state::AppState;

async fn obtenir(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Le `.42` du réseau de Bertrand, tel que le registre le porte.
fn observation(udn: &str, host: &str) -> ObservationServeurRecue {
    ObservationServeurRecue {
        udn: udn.into(),
        name: "Tune Server".into(),
        manufacturer: Some("MozAIk Labs".into()),
        model: Some("Tune".into()),
        device_type: "upnp_media_server".into(),
        location: format!("http://{host}:8888/upnp/description.xml"),
        content_directory_url: Some(format!("http://{host}:8888/upnp/cd/control")),
        host: Some(host.into()),
        port: Some(8888),
        max_age_secs: Some(1800),
    }
}

/// Écrire une ligne du registre durable datée d'il y a `age_secs`, sans
/// attendre que le temps passe.
fn observer_il_y_a(state: &AppState, udn: &str, host: &str, age_secs: i64) {
    MediaServerRepo::with_backend(state.backend.clone())
        .enregistrer_observation_a(
            &observation(udn, host),
            &tune_core::db::media_server_repo::horodatage_il_y_a(age_secs),
        )
        .unwrap();
}

fn ligne<'a>(liste: &'a Value, udn: &str) -> &'a Value {
    liste["items"]
        .as_array()
        .unwrap_or_else(|| panic!("la route ne rend pas de liste : {liste}"))
        .iter()
        .find(|i| i["id"] == udn)
        .unwrap_or_else(|| panic!("{udn} absent de la liste : {liste}"))
}

/// Le témoin de l'anomalie.
///
/// Un serveur revu il y a une minute est JOIGNABLE. Avant la correction,
/// `reachable` consultait `AppState::media_servers` — vide ici, et gelée sur
/// le `.18` — et rendait donc `false` pour une observation vieille d'une
/// minute. C'est exactement ce que trois serveurs vivants affichaient.
#[tokio::test]
async fn un_serveur_revu_il_y_a_une_minute_est_joignable() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    observer_il_y_a(&state, "uuid:2c35bec3", "192.168.1.42", 60);

    let (code, liste) = obtenir(&app, "/api/v1/network/media-servers").await;
    assert_eq!(code, StatusCode::OK, "{liste}");

    let s = ligne(&liste, "uuid:2c35bec3");
    assert_eq!(
        s["reachable"], true,
        "un serveur observé il y a 60 s est joignable. `reachable` lisait un \
         SECOND `Instant`, celui de la copie en mémoire gelée à la première \
         découverte, au lieu de l'âge du registre durable que `presence` \
         utilise déjà.\n{liste}"
    );
    assert_eq!(s["presence"], "present", "{liste}");
    assert_eq!(s["proposable"], true, "{liste}");
}

/// Les DEUX horloges de la route n'en font plus qu'une.
///
/// Le défaut ne se voit pas sur un champ isolé, il se voit sur le DÉSACCORD :
/// la même ligne portait `last_seen_secs: 3021` (horloge du registre durable)
/// et `reachable: false` (horloge de la copie gelée). Un client qui lit les
/// deux ne peut pas les concilier.
#[tokio::test]
async fn reachable_et_last_seen_secs_racontent_la_meme_chose() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    observer_il_y_a(&state, "uuid:frais", "192.168.1.42", 30);
    observer_il_y_a(&state, "uuid:silencieux", "192.168.1.15", 1_200);

    let (_, liste) = obtenir(&app, "/api/v1/network/media-servers").await;

    for udn in ["uuid:frais", "uuid:silencieux"] {
        let s = ligne(&liste, udn);
        let age = s["last_seen_secs"].as_i64().unwrap();
        let joignable = s["reachable"].as_bool().unwrap();
        assert_eq!(
            joignable,
            age < 900,
            "{udn} : `reachable` = {joignable} pour un âge de {age} s. Les deux \
             champs doivent sortir de la MÊME horloge — le seuil est \
             `MEDIA_SERVER_STALE_AFTER` (900 s).\n{liste}"
        );
    }
}

/// Les deux seuils restent distincts, et dans cet ordre.
///
/// 900 s marque « plus revu depuis un moment » et n'a aucune conséquence ;
/// 5 400 s (`SERVEUR_ABSENT_APRES`) retire des propositions. La zone grise —
/// `reachable: false` et `proposable: true` — est exactement ce que le fil
/// forum 1425 demandait de montrer, et une correction qui l'écraserait ferait
/// disparaître un serveur vivant (ce que #2139 interdit).
#[tokio::test]
async fn non_joignable_n_est_pas_encore_absent() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    // 1 200 s : au-delà du marquage, en deçà de l'absence.
    observer_il_y_a(&state, "uuid:gris", "192.168.1.42", 1_200);
    let (_, liste) = obtenir(&app, "/api/v1/network/media-servers").await;
    let s = ligne(&liste, "uuid:gris");
    assert_eq!(s["reachable"], false, "{liste}");
    assert_eq!(
        s["proposable"], true,
        "un serveur non joignable n'est pas encore absent : il reste \
         proposé.\n{liste}"
    );

    // 6 000 s : au-delà de l'absence. Le plafond de bascule en masse ne
    // s'applique pas — un seul serveur, sous le plancher de 3.
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    observer_il_y_a(&state, "uuid:eteint", "192.168.1.42", 6_000);
    let (_, liste) = obtenir(&app, "/api/v1/network/media-servers").await;
    let s = ligne(&liste, "uuid:eteint");
    assert_eq!(s["reachable"], false, "{liste}");
    assert_eq!(s["presence"], "absent", "{liste}");
    assert_eq!(s["absence_reason"], "silence_prolonge", "{liste}");
}

/// La contre-épreuve de la phase 1, qu'on ne doit pas casser : relire la liste
/// ne RESSUSCITE personne.
///
/// `synchroniser_le_registre` date chaque observation `maintenant - âge`. Une
/// erreur de signe, ou un « vu à l'instant » posé par confort, rendrait tout
/// serveur éteint éternellement présent — le travers que la migration 95
/// nomme déjà pour les zones.
#[tokio::test]
async fn relire_la_liste_ne_ressuscite_pas_un_serveur_eteint() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    observer_il_y_a(&state, "uuid:eteint", "192.168.1.42", 6_000);

    let (_, premiere) = obtenir(&app, "/api/v1/network/media-servers").await;
    let (_, seconde) = obtenir(&app, "/api/v1/network/media-servers").await;

    for liste in [&premiere, &seconde] {
        let s = ligne(liste, "uuid:eteint");
        assert_eq!(
            s["presence"], "absent",
            "relire la liste a fait repasser un serveur éteint pour \
             présent.\n{liste}"
        );
        assert_eq!(s["reachable"], false, "{liste}");
    }
    assert_eq!(
        ligne(&premiere, "uuid:eteint")["last_seen_at"],
        ligne(&seconde, "uuid:eteint")["last_seen_at"],
        "la date de dernière observation a bougé sans qu'aucune observation \
         n'ait eu lieu"
    );
}

/// Le registre partagé n'est ni vidé ni amputé par la reprise de fraîcheur.
///
/// La reprise lit la carte du balayage, vide dans cette épreuve. Elle ne doit
/// alors RIEN faire : ni retirer une ligne du registre durable, ni retirer une
/// entrée de la copie partagée. L'oubli a son évènement (`MediaServerLost`) et
/// son unique chemin.
#[tokio::test]
async fn la_reprise_de_fraicheur_ne_retire_rien_quand_le_balayage_est_muet() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    observer_il_y_a(&state, "uuid:connu", "192.168.1.42", 120);
    state.media_servers.lock().await.insert(
        "uuid:connu".into(),
        tune_core::discovery::ssdp::MediaServerInfo {
            id: "uuid:connu".into(),
            name: "Tune Server".into(),
            manufacturer: "MozAIk Labs".into(),
            model: "Tune".into(),
            location: "http://192.168.1.42:8888/upnp/description.xml".into(),
            content_directory_url: "http://192.168.1.42:8888/upnp/cd/control".into(),
            host: "192.168.1.42".into(),
            port: 8888,
            last_seen: std::time::Instant::now(),
            max_age: std::time::Duration::from_secs(1800),
        },
    );

    let (code, liste) = obtenir(&app, "/api/v1/network/media-servers").await;
    assert_eq!(code, StatusCode::OK, "{liste}");
    assert_eq!(liste["total"], 1, "{liste}");
    assert_eq!(
        state.media_servers.lock().await.len(),
        1,
        "un balayage muet ne retire rien de la copie partagée"
    );
}
