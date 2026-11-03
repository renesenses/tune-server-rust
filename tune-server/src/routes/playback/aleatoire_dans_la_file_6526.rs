//! #6526 — `POST /zones/{id}/shuffle` MÉLANGE réellement la file.
//!
//! Avant, la route n'armait qu'un mode : le serveur tirait un ordre caché que
//! `GET /zones/{id}/queue` ne rendait pas, et la file affichée par les clients
//! n'était pas l'ordre joué. Ces tests passent par le routeur complet, comme
//! un client : ils lisent la file que les écrans lisent.
//!
//! Aucun son : la zone n'a pas d'appareil de sortie, rien ne joue.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::poller::PositionPoller;

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut requete = Request::builder().method(methode).uri(uri);
    let corps = match corps {
        Some(c) => {
            requete = requete.header("Content-Type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app
        .clone()
        .oneshot(requete.body(corps).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Crée `n` pistes locales (sans fichier : rien ne se jouera) et rend leurs
/// identifiants.
fn pistes(state: &crate::state::AppState, debut: usize, n: usize) -> Vec<i64> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    (debut..debut + n)
        .map(|i| {
            let mut piste = tune_core::db::models::Track::new(format!("Piste {i}"));
            piste.file_path = Some(format!("/aucun/chemin/6526/piste{i}.flac"));
            piste.track_number = i as i32;
            piste.duration_ms = 180_000;
            repo.create(&piste).unwrap()
        })
        .collect()
}

/// Une zone dont la file compte `n` pistes, le titre en cours au rang
/// `courant` (marqué en base ET dans l'état de zone).
async fn zone_avec_une_file(state: &crate::state::AppState, n: usize, courant: i64) -> i64 {
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), None)
        .unwrap();
    let ids = pistes(state, 1, n);
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    repo.set_queue(zone_id, &ids).unwrap();
    repo.set_current_pos(zone_id, courant).unwrap();
    state
        .playback
        .update_queue_info(zone_id, courant, n as i64)
        .await;
    zone_id
}

/// Les titres de la file, dans l'ordre que `GET /zones/{id}/queue` rend.
async fn file_relue(app: &axum::Router, zone_id: i64) -> (Vec<String>, i64) {
    let (statut, v) = appeler(app, "GET", &format!("/api/v1/zones/{zone_id}/queue"), None).await;
    assert_eq!(statut, StatusCode::OK, "GET queue : {v}");
    let titres = v["tracks"]
        .as_array()
        .unwrap_or_else(|| panic!("pas de `tracks` dans {v}"))
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect();
    (titres, v["position"].as_i64().unwrap_or(-99))
}

fn titres_d_origine(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("Piste {i}")).collect()
}

fn trie(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Activer : la suite est rebattue, le titre en cours et les titres joués ne
/// bougent pas, et `GET …/queue` rend l'ordre que la zone jouera — l'avance
/// suit la file (plus aucun ordre caché).
#[tokio::test]
async fn activer_melange_la_suite_et_la_file_relue_est_l_ordre_joue() {
    let (app, state) = serveur();
    let n = 20;
    let zone_id = zone_avec_une_file(&state, n, 4).await;

    let (statut, v) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone_id}/shuffle?enabled=true"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["shuffle"], true, "{v}");
    assert_eq!(v["queue_reordered"], true, "{v}");
    assert_eq!(v["queue_position"], 4, "{v}");
    assert_eq!(v["queue_length"], n as i64, "{v}");

    let origine = titres_d_origine(n);
    let (titres, position) = file_relue(&app, zone_id).await;
    assert_eq!(position, 4, "le titre en cours reste au même rang");
    assert_eq!(
        &titres[..5],
        &origine[..5],
        "titres joués et titre en cours figés : {titres:?}"
    );
    assert_eq!(
        trie(titres.clone()),
        trie(origine.clone()),
        "une permutation"
    );
    assert_ne!(
        &titres[5..],
        &origine[5..],
        "la suite n'a pas été rebattue : la file relue est encore l'ordre d'origine (#6526)"
    );

    let etat = state.playback.get_state(zone_id).await;
    assert!(etat.shuffle);
    assert!(
        etat.shuffle_order.is_empty(),
        "plus d'ordre caché : c'est la file qui fait foi"
    );
    assert_eq!(
        PositionPoller::next_position(&etat),
        Some(5),
        "le suivant est la ligne suivante de la file relue, {}",
        titres[5]
    );
}

/// Désactiver : l'ordre d'origine revient, et le curseur suit le titre en
/// cours jusqu'à son rang d'origine.
#[tokio::test]
async fn desactiver_retablit_l_ordre_d_origine_sur_le_meme_titre() {
    let (app, state) = serveur();
    let n = 20;
    let zone_id = zone_avec_une_file(&state, n, 4).await;
    let uri = format!("/api/v1/zones/{zone_id}/shuffle");
    appeler(&app, "POST", &format!("{uri}?enabled=true"), None).await;

    // On écoute la file mélangée jusqu'au rang 9.
    let (melangee, _) = file_relue(&app, zone_id).await;
    let ecoute = melangee[9].clone();
    PlayQueueRepo::with_backend(state.backend.clone())
        .set_current_pos(zone_id, 9)
        .unwrap();
    state.playback.update_queue_info(zone_id, 9, n as i64).await;

    let (statut, v) = appeler(&app, "POST", &format!("{uri}?enabled=false"), None).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["shuffle"], false, "{v}");

    let origine = titres_d_origine(n);
    let (titres, position) = file_relue(&app, zone_id).await;
    assert_eq!(titres, origine, "l'ordre d'origine n'est pas rétabli");
    let rang = origine.iter().position(|t| *t == ecoute).unwrap() as i64;
    assert_eq!(position, rang, "la file relue doit pointer {ecoute}");
    assert_eq!(v["queue_position"], rang, "{v}");
    assert_eq!(
        state.playback.get_state(zone_id).await.queue_position,
        rang,
        "le curseur de zone doit suivre {ecoute} à son rang d'origine"
    );
}

/// « Lire ensuite » pendant l'aléatoire : la règle de la route d'ajout
/// (position = rang en cours + 1) tient, et le titre ajouté est bien le
/// SUIVANT joué. Le tirage caché le perdait : il était refait au changement de
/// longueur. Il reste juste après le titre en cours quand l'aléatoire s'éteint.
#[tokio::test]
async fn lire_ensuite_pendant_l_aleatoire_joue_ensuite() {
    let (app, state) = serveur();
    let n = 12;
    let zone_id = zone_avec_une_file(&state, n, 3).await;
    let uri = format!("/api/v1/zones/{zone_id}/shuffle");
    appeler(&app, "POST", &format!("{uri}?enabled=true"), None).await;

    let ajout = pistes(&state, 100, 1)[0];
    let (statut, v) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone_id}/queue/add"),
        Some(json!({ "track_ids": [ajout], "position": 4 })),
    )
    .await;
    assert!(statut.is_success(), "queue/add : {statut} {v}");

    let (titres, position) = file_relue(&app, zone_id).await;
    assert_eq!(position, 3);
    assert_eq!(titres[4], "Piste 100", "file {titres:?}");
    let etat = state.playback.get_state(zone_id).await;
    assert_eq!(etat.queue_length, n as i64 + 1);
    assert_eq!(
        PositionPoller::next_position(&etat),
        Some(4),
        "« Lire ensuite » doit être le suivant joué"
    );

    appeler(&app, "POST", &format!("{uri}?enabled=false"), None).await;
    let (titres, position) = file_relue(&app, zone_id).await;
    let mut attendu = titres_d_origine(n);
    attendu.insert(4, "Piste 100".into());
    assert_eq!(
        titres, attendu,
        "rétabli, « Lire ensuite » reste le suivant"
    );
    assert_eq!(position, 3);
}

/// Compatibilité : l'ancienne forme de la route (sans paramètre = bascule)
/// rend toujours `{"shuffle": bool}`, et l'événement `shuffle` habituel part
/// APRÈS l'écriture — un client qui relit la file en le recevant lit déjà le
/// nouvel ordre.
#[tokio::test]
async fn l_ancienne_route_bascule_et_l_evenement_part_apres_l_ecriture() {
    let (app, state) = serveur();
    let n = 15;
    let zone_id = zone_avec_une_file(&state, n, 0).await;
    let uri = format!("/api/v1/zones/{zone_id}/shuffle");
    let mut evenements = state.playback.subscribe();

    let (statut, v) = appeler(&app, "POST", &uri, None).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["shuffle"], true, "sans paramètre, la route bascule : {v}");

    let ev = loop {
        let ev = evenements.recv().await.unwrap();
        if ev.event == "shuffle" && ev.zone_id == zone_id {
            break ev;
        }
    };
    assert_eq!(ev.data["enabled"], true);
    // L'événement est reçu après la réponse ici, mais il a été émis après
    // l'écriture : la base porte déjà le nouvel ordre.
    let (titres, _) = file_relue(&app, zone_id).await;
    assert_eq!(titres[0], "Piste 1");
    assert_ne!(titres, titres_d_origine(n), "file non mélangée");

    let (_, v) = appeler(&app, "POST", &uri, None).await;
    assert_eq!(v["shuffle"], false, "{v}");
    let (titres, _) = file_relue(&app, zone_id).await;
    assert_eq!(titres, titres_d_origine(n));
}

/// Une file NEUVE lancée pendant l'aléatoire (« Tout lire » d'un album avec
/// départ au 4e titre) part du titre lancé, et le reste suit mélangé — ce que
/// le tirage caché donnait déjà, désormais visible dans la file. La lecture
/// elle-même échoue (aucune sortie) ; la file, elle, est écrite avant.
#[tokio::test]
async fn une_file_neuve_sous_l_aleatoire_part_du_titre_lance() {
    let (app, state) = serveur();
    let zone_id = zone_avec_une_file(&state, 3, 0).await;
    appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone_id}/shuffle?enabled=true"),
        None,
    )
    .await;

    let album = pistes(&state, 200, 16);
    let _ = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone_id}/play"),
        Some(json!({ "track_ids": album, "start_index": 3 })),
    )
    .await;

    let (titres, _) = file_relue(&app, zone_id).await;
    let origine: Vec<String> = (200..216).map(|i| format!("Piste {i}")).collect();
    assert_eq!(
        titres[0], "Piste 203",
        "la file neuve part du titre lancé : {titres:?}"
    );
    assert_eq!(trie(titres.clone()), trie(origine.clone()));
    assert_ne!(titres, origine, "file neuve non mélangée");
}
