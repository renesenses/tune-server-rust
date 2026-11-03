//! #6688 — le suivi d'une playlist locale partagée avec un cercle.
//!
//! Bertrand, 21/10 : il modifie la playlist d'origine après « Partager avec
//! un cercle », et le cercle ne voit pas le changement. Décision : suivi
//! automatique, l'originale l'emporte.
//!
//! Contre le faux mozaiklabs de `commun` : ajout, retrait, déplacement et
//! renommage reportés ; un conflit rejoué ; l'écart (le cercle avait changé
//! sa copie) remplacé, journalisé et rendu ; la reprise après une coupure ;
//! « Ne plus suivre » ; et aucune requête au cloud pour une playlist qui
//! n'est pas suivie.

mod commun;

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_circle::lecture::Lecture;
use tune_circle::playlists::{self, Collaboratif};
use tune_circle::relais::Relais;
use tune_circle::resolution::Resolveur;
use tune_circle::suivi::{self, Bilan, Suivi};
use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::QueueInput;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::streaming::ServiceRegistry;

use commun::*;

struct SansLecture;

#[async_trait]
impl Lecture for SansLecture {
    async fn jouer(&self, _zone_id: i64, _e: Vec<QueueInput>) -> Result<usize, String> {
        Ok(0)
    }
}

struct Banc {
    faux: Serveur,
    app: Router,
    backend: Arc<dyn DbBackend>,
    suivi: Arc<Suivi>,
    /// La playlist LOCALE d'origine.
    locale: i64,
    /// Les `tracks.id` des pistes A, B, C, D, E.
    pistes: Vec<i64>,
}

const TITRES: [&str; 5] = ["Alpha", "Bravo", "Charlie", "Delta", "Echo"];

fn repo(b: &Banc) -> PlaylistRepo {
    PlaylistRepo::with_backend(b.backend.clone())
}

/// Le faux cloud, une base avec cinq pistes, et une playlist locale
/// « Dimanche local » portant Alpha, Bravo, Charlie. Rien n'est encore suivi.
async fn banc() -> Banc {
    let faux = demarrer().await;
    let backend = base(&faux.base, Some(JETON));
    let pistes: Vec<i64> = TITRES
        .iter()
        .map(|t| {
            let mut piste = tune_core::db::models::Track::new((*t).into());
            piste.file_path = Some(format!("/musique/6688/{t}.flac"));
            piste.duration_ms = 200_000;
            tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
                .create(&piste)
                .unwrap()
        })
        .collect();
    let locale = PlaylistRepo::with_backend(backend.clone())
        .create_with_tracks("Dimanche local", None, 1, &pistes[..3])
        .unwrap()
        .0;
    let relais = Arc::new(Relais::new(backend.clone()));
    let suivi = Arc::new(Suivi::new(relais.clone(), None));
    let app = playlists::router(Arc::new(Collaboratif {
        relais,
        resolveur: Resolveur::new(
            backend.clone(),
            Arc::new(tokio::sync::Mutex::new(ServiceRegistry::new())),
        ),
        lecture: Arc::new(SansLecture),
    }))
    .merge(suivi::router(suivi.clone()));
    Banc {
        faux,
        app,
        backend,
        suivi,
        locale,
        pistes,
    }
}

/// « Partager avec un cercle » tel que l'écran le fait (playlist de cercle
/// neuve, id 6 chez le faux), puis le lien de suivi. Rend la réponse du lien.
async fn partager_et_suivre(b: &Banc) -> Value {
    let r = appel(
        &b.app,
        "POST",
        "/playlists",
        Some(json!({ "circle_id": 1, "name": "Dimanche local" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::CREATED);
    let r = appel(
        &b.app,
        "PUT",
        "/playlists/6/follow",
        Some(json!({ "playlist_id": b.locale, "circle_id": 1 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    r.json()
}

fn la_playlist_du_cercle(b: &Banc) -> Value {
    let f = b.faux.etat.lock().unwrap();
    f.playlists.iter().find(|p| p["id"] == 6).unwrap().clone()
}

fn titres_du_cercle(b: &Banc) -> Vec<String> {
    la_playlist_du_cercle(b)["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect()
}

fn titres_locaux(b: &Banc) -> Vec<String> {
    repo(b)
        .get_track_ids(b.locale)
        .unwrap()
        .iter()
        .map(|id| TITRES[b.pistes.iter().position(|p| p == id).unwrap()].to_string())
        .collect()
}

fn routes_ecrites(b: &Banc) -> Vec<String> {
    b.faux
        .etat
        .lock()
        .unwrap()
        .ecritures_de_playlist
        .iter()
        .map(|(r, _)| r.clone())
        .collect()
}

fn appels(b: &Banc) -> usize {
    b.faux.etat.lock().unwrap().appels
}

/// Un tour de la boucle de fond, pour la playlist de cercle 6.
async fn tour(b: &Banc) -> Bilan {
    let bilans = b.suivi.tour().await;
    bilans
        .into_iter()
        .find(|(id, _)| id == "6")
        .map(|(_, b)| b)
        .unwrap_or(Bilan::Inchange)
}

async fn etat_du_suivi(b: &Banc) -> Value {
    let r = appel(&b.app, "GET", "/playlists/6/follow", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    r.json()
}

#[tokio::test]
async fn suivre_relie_et_fait_le_premier_report() {
    let b = banc().await;
    let r = partager_et_suivre(&b).await;
    assert_eq!(r["sync"]["status"], "synced", "{r}");
    assert_eq!(r["follow"]["following"], true);
    assert_eq!(r["follow"]["local_name"], "Dimanche local");
    // Le nom du cercle est lu au cloud (`GET /` → circles).
    assert_eq!(r["follow"]["circle_name"], "Famille");
    assert!(r["follow"]["last_sync"].is_string());
    assert_eq!(titres_du_cercle(&b), ["Alpha", "Bravo", "Charlie"]);

    // La fiche de la playlist de cercle dit ce qu'elle suit.
    let fiche = appel(&b.app, "GET", "/playlists/6", None).await;
    assert_eq!(fiche.statut, StatusCode::OK);
    let j = fiche.json();
    assert_eq!(j["follow"]["local_name"], "Dimanche local");
    assert_eq!(j["follow"]["following"], true);
    assert!(j["follow"]["last_sync"].is_string());
    // Une playlist de cercle NON suivie passe telle quelle.
    let autre = appel(&b.app, "GET", "/playlists/5", None).await.json();
    assert!(autre.get("follow").is_none(), "{autre}");
}

#[tokio::test]
async fn un_ajout_dans_l_originale_est_reporte_dans_le_cercle() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();

    let bilan = tour(&b).await;
    assert!(
        matches!(
            bilan,
            Bilan::Reporte {
                ajoutes: 1,
                retires: 0,
                ..
            }
        ),
        "#6688 — l'ajout dans l'originale n'a pas été reporté : {bilan:?}"
    );
    assert_eq!(
        titres_du_cercle(&b),
        titres_locaux(&b),
        "#6688 — le cercle ne suit pas l'ajout fait dans l'originale"
    );
}

#[tokio::test]
async fn un_retrait_dans_l_originale_est_reporte_sous_version() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    let version = la_playlist_du_cercle(&b)["version"].as_i64().unwrap();
    assert_eq!(
        repo(&b).remove_tracks_at_positions(b.locale, &[1]).unwrap(),
        1
    );

    let bilan = tour(&b).await;
    assert!(
        matches!(bilan, Bilan::Reporte { retires: 1, .. }),
        "#6688 — le retrait dans l'originale n'a pas été reporté : {bilan:?}"
    );
    assert_eq!(
        titres_du_cercle(&b),
        ["Alpha", "Charlie"],
        "#6688 — le retrait fait dans l'originale n'est pas arrivé au cercle"
    );
    // Le retrait porte la version courante, en requête.
    assert_eq!(
        b.faux.etat.lock().unwrap().requete_de_retrait.as_deref(),
        Some(format!("version={version}").as_str())
    );
}

#[tokio::test]
async fn un_deplacement_et_un_renommage_sont_reportes() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    assert!(repo(&b).reorder_by_ranks(b.locale, &[2, 0, 1]).unwrap());
    repo(&b)
        .update(b.locale, Some("Lundi local"), None)
        .unwrap();

    let bilan = tour(&b).await;
    assert!(
        matches!(
            bilan,
            Bilan::Reporte {
                reordonne: true,
                renomme: true,
                ..
            }
        ),
        "#6688 — déplacement ou renommage non reporté : {bilan:?}"
    );
    assert_eq!(titres_du_cercle(&b), ["Charlie", "Alpha", "Bravo"]);
    assert_eq!(la_playlist_du_cercle(&b)["name"], "Lundi local");
    assert_eq!(etat_du_suivi(&b).await["local_name"], "Lundi local");
    let routes = routes_ecrites(&b);
    assert!(routes.contains(&"PUT /playlists/{id}/order".to_string()));
    assert!(routes.contains(&"PATCH /playlists/{id}".to_string()));
}

/// Le cercle a changé sa copie entre deux reports (Alice y a ajouté un
/// morceau) : l'originale l'emporte, l'écart est rendu au client.
#[tokio::test]
async fn conflit_l_originale_l_emporte_avec_un_avertissement() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    {
        let mut f = b.faux.etat.lock().unwrap();
        let p = f.playlists.iter_mut().find(|p| p["id"] == 6).unwrap();
        p["items"].as_array_mut().unwrap().push(json!({
            "item_id": 999, "title": "Ajout d'Alice",
            "added_by": { "user_id": 7, "name": "Alice" }
        }));
        p["version"] = json!(p["version"].as_i64().unwrap() + 1);
    }
    repo(&b).add_tracks(b.locale, &[b.pistes[4]], None).unwrap();

    let bilan = tour(&b).await;
    let Bilan::Reporte {
        ecart: Some(ecart), ..
    } = &bilan
    else {
        panic!("#6688 — l'écart du cercle n'a pas été signalé : {bilan:?}");
    };
    assert_eq!(ecart["code"], suivi::CODE_ECART);
    assert_eq!(ecart["discarded_items"], 1);
    assert_eq!(
        titres_du_cercle(&b),
        titres_locaux(&b),
        "#6688 — l'originale doit l'emporter sur la copie du cercle"
    );
    // L'avertissement est rendu au client, sur la fiche et sur le lien.
    assert_eq!(
        etat_du_suivi(&b).await["warning"]["code"],
        suivi::CODE_ECART
    );
    let fiche = appel(&b.app, "GET", "/playlists/6", None).await.json();
    assert_eq!(fiche["follow"]["warning"]["code"], suivi::CODE_ECART);
}

/// Un 409 pendant le report (un membre écrit au même instant) : tout est
/// relu et le report rejoué une fois.
#[tokio::test]
async fn un_conflit_de_version_est_rejoue_une_fois() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    b.faux.etat.lock().unwrap().conflits_a_simuler = 1;
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();

    let bilan = tour(&b).await;
    assert!(
        matches!(bilan, Bilan::Reporte { ajoutes: 1, .. }),
        "#6688 — le report n'a pas été rejoué après le 409 : {bilan:?}"
    );
    assert_eq!(titres_du_cercle(&b), titres_locaux(&b));
}

/// Cloud injoignable : le report attend, puis est rejoué au retour, sans
/// perte — y compris d'un changement fait pendant la coupure.
#[tokio::test]
async fn reprise_apres_une_coupure_du_cloud() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    b.faux.etat.lock().unwrap().panne = true;
    assert_eq!(
        repo(&b).remove_tracks_at_positions(b.locale, &[0]).unwrap(),
        1
    );

    let bilan = tour(&b).await;
    assert!(
        matches!(bilan, Bilan::EnAttente(_)),
        "#6688 — une coupure doit mettre le report en attente : {bilan:?}"
    );
    let etat = etat_du_suivi(&b).await;
    assert_eq!(etat["pending"], true, "{etat}");
    assert_eq!(titres_du_cercle(&b), ["Alpha", "Bravo", "Charlie"]);

    // Un second changement pendant la coupure.
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();
    assert!(matches!(tour(&b).await, Bilan::EnAttente(_)));

    b.faux.etat.lock().unwrap().panne = false;
    let bilan = tour(&b).await;
    assert!(
        matches!(bilan, Bilan::Reporte { .. }),
        "#6688 — le report en attente n'a pas été rejoué au retour du cloud : {bilan:?}"
    );
    assert_eq!(
        titres_du_cercle(&b),
        ["Bravo", "Charlie", "Delta"],
        "#6688 — un changement fait pendant la coupure a été perdu"
    );
    assert_eq!(etat_du_suivi(&b).await["pending"], false);
}

#[tokio::test]
async fn ne_plus_suivre_arrete_les_reports() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    let r = appel(&b.app, "DELETE", "/playlists/6/follow", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let etat = etat_du_suivi(&b).await;
    assert_eq!(etat["following"], false, "{etat}");
    assert!(etat["stopped_at"].is_string());

    let avant = appels(&b);
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();
    tour(&b).await;
    assert_eq!(
        appels(&b),
        avant,
        "#6688 — un report est parti après « Ne plus suivre »"
    );
    assert_eq!(titres_du_cercle(&b), ["Alpha", "Bravo", "Charlie"]);
    // Déjà arrêté : 404.
    let r = appel(&b.app, "DELETE", "/playlists/6/follow", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    // Et « Synchroniser maintenant » ne relance rien.
    let r = appel(&b.app, "POST", "/playlists/6/follow/sync", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(appels(&b), avant);
}

/// Une playlist locale qui n'est suivie par aucun cercle : ses changements
/// ne font AUCUN appel au cloud. Et un lien dont l'originale n'a pas changé
/// n'en fait pas non plus.
#[tokio::test]
async fn une_playlist_non_partagee_ne_fait_aucun_appel_au_cloud() {
    let b = banc().await;
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();
    assert_eq!(
        repo(&b).remove_tracks_at_positions(b.locale, &[0]).unwrap(),
        1
    );
    assert!(b.suivi.tour().await.is_empty());
    assert_eq!(
        appels(&b),
        0,
        "#6688 — une playlist non partagée a provoqué un appel au cloud"
    );

    partager_et_suivre(&b).await;
    let avant = appels(&b);
    assert_eq!(
        tour(&b).await,
        Bilan::Inchange,
        "#6688 — un lien sans changement ne doit pas être reporté"
    );
    assert_eq!(
        appels(&b),
        avant,
        "#6688 — un lien sans changement a provoqué un appel au cloud"
    );
}

#[tokio::test]
async fn suivre_une_playlist_locale_inconnue_est_refuse() {
    let b = banc().await;
    let r = appel(
        &b.app,
        "PUT",
        "/playlists/6/follow",
        Some(json!({ "playlist_id": 4242 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], suivi::CODE_SOURCE_INCONNUE);
    assert_eq!(appels(&b), 0);
}

/// Un membre change la playlist de cercle `id` : Alice y ajoute un morceau.
fn alice_ajoute_au_cercle(b: &Banc, id: i64) {
    let mut f = b.faux.etat.lock().unwrap();
    let p = f.playlists.iter_mut().find(|p| p["id"] == id).unwrap();
    p["items"].as_array_mut().unwrap().push(json!({
        "item_id": 998, "title": "Ajout d'Alice pendant l'arrêt",
        "added_by": { "user_id": 7, "name": "Alice" }
    }));
    p["version"] = json!(p["version"].as_i64().unwrap() + 1);
}

/// Relecture rc7 : « Ne plus suivre », des membres changent la copie, puis un
/// nouveau `PUT …/follow`. Leurs changements sont remplacés — l'originale
/// l'emporte — mais PAS en silence : l'écart est levé.
#[tokio::test]
async fn reprendre_le_suivi_signale_ce_que_le_cercle_a_change_pendant_l_arret() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    let r = appel(&b.app, "DELETE", "/playlists/6/follow", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    alice_ajoute_au_cercle(&b, 6);

    let r = appel(
        &b.app,
        "PUT",
        "/playlists/6/follow",
        Some(json!({ "playlist_id": b.locale, "circle_id": 1 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    let j = r.json();
    assert_eq!(
        j["sync"]["warning"]["code"],
        suivi::CODE_ECART,
        "#6688 — la reprise du suivi a écrasé sans avertissement ce que le cercle avait changé : {j}"
    );
    assert_eq!(j["sync"]["warning"]["discarded_items"], 1);
    assert_eq!(j["follow"]["warning"]["code"], suivi::CODE_ECART);
    assert_eq!(titres_du_cercle(&b), titres_locaux(&b));
}

/// La reprise d'un suivi dont la copie n'a PAS bougé pendant l'arrêt, alors
/// que l'originale a changé : report normal, sans écart.
#[tokio::test]
async fn reprendre_le_suivi_sans_changement_du_cercle_ne_signale_rien() {
    let b = banc().await;
    partager_et_suivre(&b).await;
    appel(&b.app, "DELETE", "/playlists/6/follow", None).await;
    repo(&b).add_tracks(b.locale, &[b.pistes[3]], None).unwrap();

    let j = appel(
        &b.app,
        "PUT",
        "/playlists/6/follow",
        Some(json!({ "playlist_id": b.locale, "circle_id": 1 })),
    )
    .await
    .json();
    assert_eq!(j["sync"]["status"], "synced", "{j}");
    assert!(j["sync"]["warning"].is_null(), "écart à tort : {j}");
    assert_eq!(titres_du_cercle(&b), titres_locaux(&b));
}

/// Relecture rc7 : `PUT …/follow` sur une playlist de cercle EXISTANTE et
/// garnie (la 5, « Dimanche », du faux) : ce qu'elle portait est remplacé
/// avec l'avertissement d'écart.
#[tokio::test]
async fn suivre_une_playlist_de_cercle_deja_garnie_signale_l_ecart() {
    let b = banc().await;
    let avant = b.faux.etat.lock().unwrap().playlists[0]["items"]
        .as_array()
        .unwrap()
        .len();
    assert!(avant > 0);
    let r = appel(
        &b.app,
        "PUT",
        "/playlists/5/follow",
        Some(json!({ "playlist_id": b.locale, "circle_id": 1 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    let j = r.json();
    assert_eq!(
        j["sync"]["warning"]["code"],
        suivi::CODE_ECART,
        "#6688 — suivre une playlist de cercle garnie a écrasé son contenu sans avertissement : {j}"
    );
    assert_eq!(j["sync"]["warning"]["discarded_items"], avant);
}
