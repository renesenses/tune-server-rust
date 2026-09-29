//! Tune Circle, étape T3 (#5326) : les étiquettes et les collections
//! intelligentes partagées par cercle — les « rayons ».
//!
//! Contre le faux mozaiklabs de `commun` : ce qui part au cloud (membres
//! résolus ICI, jamais fournis par le client ; streaming en références, jamais
//! un chemin), le battement (ne repousse que ce qui a changé), et la
//! révocation (le 404 du cloud revient tel quel, rien n'est servi de mémoire).

mod commun;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_circle::ensembles::{Membres, Reference};
use tune_core::cloud::library_sync::ressemble_a_un_chemin;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

/// Une base connectée, liée, dont le cercle 1 partage la bibliothèque de CE
/// serveur, avec une petite bibliothèque, deux étiquettes et une collection.
async fn monde() -> (
    Serveur,
    Arc<dyn DbBackend>,
    Arc<FauxHote>,
    axum::Router,
    Arc<tune_circle::battement::Pousseur>,
) {
    let faux = demarrer().await;
    {
        let mut f = faux.etat.lock().unwrap();
        f.t2 = true;
        f.partages.push((1, SERVEUR_DU_COMPTE.into()));
    }
    let backend = base(&faux.base, Some(JETON));
    SettingsRepo::with_backend(backend.clone())
        .set("server_id", SERVEUR_DU_COMPTE)
        .unwrap();
    backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
             INSERT INTO albums (id, title, artist_id, year) VALUES (10, 'Kind of Blue', 1, 1959);
             INSERT INTO tracks (id, title, album_id, artist_id, file_path)
                 VALUES (100, 'So What', 10, 1, '/home/moi/Musique/Kind of Blue/01.flac');
             INSERT INTO tags (id, name) VALUES (3, 'Jazz ECM'), (4, 'Vide');
             INSERT INTO item_tags (tag_id, item_type, item_id) VALUES
                 (3, 'album', 10), (3, 'track', 100), (3, 'artist', 1),
                 (3, 'playlist', 5), (3, 'smart_collection', 2);
             INSERT INTO streaming_item_tags (tag_id, item_type, source, source_id, title, artist, album, cover_url)
                 VALUES (3, 'album', 'qobuz', 'q-123', 'A Love Supreme', 'John Coltrane', NULL,
                         'https://static.qobuz.com/images/covers/x.jpg'),
                        (3, 'track', 'tidal', '/srv/fichier.flac', 'Naima', 'John Coltrane', 'Giant Steps', NULL);
             INSERT INTO smart_collections (id, name, rules, match_mode, sort_by, sort_order)
                 VALUES (1000, 'Vinyles rippés', '[]', 'all', 'title', 'asc');",
        )
        .unwrap();
    let hote = Arc::new(FauxHote::default());
    let (app, pousseur) = app_avec_rayons(backend.clone(), hote.clone());
    (faux, backend, hote, app, pousseur)
}

fn collection(albums: &[i64]) -> Membres {
    Membres {
        nom: "Vinyles rippés".into(),
        albums: albums.to_vec(),
        references: vec![Reference {
            genre: "album".into(),
            titre: "Blue Train".into(),
            artiste: Some("John Coltrane".into()),
            album: None,
            service: "qobuz".into(),
            id_de_service: "q-777".into(),
        }],
        ..Membres::default()
    }
}

fn corps_recus(faux: &Serveur) -> Vec<Value> {
    faux.etat.lock().unwrap().corps_des_ensembles.clone()
}

/// Toutes les chaînes d'un JSON, à toute profondeur.
fn chaines(v: &Value, sortie: &mut Vec<String>) {
    match v {
        Value::String(s) => sortie.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| chaines(x, sortie)),
        Value::Object(o) => o.values().for_each(|x| chaines(x, sortie)),
        _ => {}
    }
}

// 1. Rien par défaut ---------------------------------------------------------

#[tokio::test]
async fn rien_n_est_coche_par_defaut_et_la_liste_locale_est_rendue() {
    let (faux, _b, _h, app, _p) = monde().await;
    let r = appel(&app, "GET", "/circles/1/sets", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let v = r.json();
    assert_eq!(
        v["tags"],
        json!([
            { "kind": "tag", "source_id": 3, "name": "Jazz ECM", "count": 5, "shared": false },
            { "kind": "tag", "source_id": 4, "name": "Vide", "count": 0, "shared": false }
        ]),
        "{v}"
    );
    // Les collections du semis sont là aussi ; la nôtre, non cochée.
    let smart = v["smart_collections"].as_array().unwrap();
    assert!(smart.iter().all(|c| c["shared"] == false), "{v}");
    assert!(
        smart.contains(&json!({ "kind": "smart_collection", "source_id": 1000,
                                "name": "Vinyles rippés", "count": null, "shared": false })),
        "{v}"
    );
    // Rien n'est parti au cloud qu'une lecture.
    assert!(corps_recus(&faux).is_empty());
}

// 2. Cocher : membres résolus ici, liste blanche, pas de chemin --------------

#[tokio::test]
async fn cocher_une_etiquette_pousse_ses_membres_resolus_et_ses_references() {
    let (faux, _b, _h, app, _p) = monde().await;
    let r = appel_avec_entetes(
        &app,
        "PUT",
        "/circles/1/sets/tag/3",
        None,
        &[("X-Profile-Id", "2")],
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    let corps = corps_recus(&faux);
    assert_eq!(corps.len(), 1);
    let c = &corps[0];
    let mut cles: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
    cles.sort_unstable();
    assert_eq!(
        cles,
        [
            "albums",
            "artists",
            "digest",
            "name",
            "profile_id",
            "server_id",
            "streaming",
            "tracks"
        ]
    );
    assert_eq!(c["name"], "Jazz ECM");
    assert_eq!(c["server_id"], SERVEUR_DU_COMPTE);
    assert_eq!(c["profile_id"], 2);
    assert_eq!(c["albums"], json!([10]));
    assert_eq!(c["tracks"], json!([100]));
    assert_eq!(c["artists"], json!([1]));
    // Streaming : des RÉFÉRENCES. La pochette ne part pas ; un identifiant de
    // service qui a la forme d'un chemin ne part pas non plus.
    assert_eq!(
        c["streaming"],
        json!([
            { "type": "album", "title": "A Love Supreme", "artist_name": "John Coltrane",
              "qobuz_id": "q-123" },
            { "type": "track", "title": "Naima", "artist_name": "John Coltrane",
              "album_title": "Giant Steps" }
        ])
    );
    let mut toutes = Vec::new();
    chaines(c, &mut toutes);
    for s in &toutes {
        assert!(!ressemble_a_un_chemin(s), "un chemin est parti : {s}");
        assert!(!s.contains("http"), "une adresse est partie : {s}");
    }
    assert!(c["digest"].as_str().unwrap().starts_with("v1:"));
}

#[tokio::test]
async fn le_client_ne_peut_pas_imposer_la_liste_des_membres() {
    let (faux, _b, _h, app, _p) = monde().await;
    let impose = json!({
        "name": "Autre chose", "server_id": SERVEUR_D_UN_AUTRE, "profile_id": 99,
        "albums": [424242], "tracks": [1, 2, 3], "artists": [], "digest": "v1:faux",
        "streaming": [{ "type": "track", "title": "/etc/passwd" }]
    });
    let r = appel(&app, "PUT", "/circles/1/sets/tag/3", Some(impose)).await;
    assert_eq!(r.statut, StatusCode::OK);
    let c = &corps_recus(&faux)[0];
    assert_eq!(c["name"], "Jazz ECM");
    assert_eq!(c["server_id"], SERVEUR_DU_COMPTE);
    assert_eq!(c["profile_id"], 1);
    assert_eq!(c["albums"], json!([10]));
    assert_eq!(c["tracks"], json!([100]));
    assert_ne!(c["digest"], "v1:faux");
}

#[tokio::test]
async fn une_etiquette_ou_un_genre_inconnu_rend_404_sans_rien_envoyer() {
    let (faux, _b, _h, app, _p) = monde().await;
    for chemin in [
        "/circles/1/sets/tag/999",
        "/circles/1/sets/playlist/3",
        "/circles/1/sets/tag/0",
        "/circles/1/sets/tag/abc",
        "/circles/1/sets/smart_collection/1000", // le faux hôte ne la connaît pas
    ] {
        let r = appel(&app, "PUT", chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{chemin}");
        assert_eq!(r.json(), json!({ "code": "circle.not_found" }), "{chemin}");
    }
    assert!(corps_recus(&faux).is_empty());
}

#[tokio::test]
async fn un_cercle_sans_partage_de_bibliotheque_rend_le_409_du_cloud() {
    let (faux, _b, _h, app, _p) = monde().await;
    faux.etat.lock().unwrap().partages.clear();
    let r = appel(&app, "PUT", "/circles/1/sets/tag/3", None).await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.json(), json!({ "error": "library_not_shared" }));
}

// 3. Collection intelligente : profil de celui qui coche, battement ----------

#[tokio::test]
async fn une_collection_se_resout_avec_le_profil_qui_la_coche_et_suit_ses_changements() {
    let (faux, _b, hote, app, pousseur) = monde().await;
    hote.ranger(1000, 3, collection(&[10]));
    let r = appel_avec_entetes(
        &app,
        "PUT",
        "/circles/1/sets/smart_collection/1000",
        None,
        &[("X-Profile-Id", "3")],
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK, "{:?}", r.json());
    let c = &corps_recus(&faux)[0];
    assert_eq!(c["profile_id"], 3);
    assert_eq!(c["albums"], json!([10]));
    assert_eq!(
        c["streaming"],
        json!([{ "type": "album", "title": "Blue Train", "artist_name": "John Coltrane",
                 "qobuz_id": "q-777" }])
    );
    assert_eq!(*hote.resolutions.lock().unwrap(), vec![(1000, 3)]);

    // Rien n'a changé : le battement ne repousse rien.
    let b = pousseur.tour_complet().await;
    assert_eq!((b.examines, b.envoyes), (1, 0), "{b:?}");
    assert_eq!(corps_recus(&faux).len(), 1);

    // La collection gagne un album (nouvelle règle, nouvel album conforme) :
    // le battement suivant repousse, avec le MÊME profil.
    hote.ranger(1000, 3, collection(&[10, 11]));
    let b = pousseur.tour_complet().await;
    assert_eq!(b.envoyes, 1, "{b:?}");
    let corps = corps_recus(&faux);
    assert_eq!(corps.len(), 2);
    assert_eq!(corps[1]["albums"], json!([10, 11]));
    assert_eq!(corps[1]["profile_id"], 3);
    assert_eq!(hote.resolutions.lock().unwrap().last(), Some(&(1000, 3)));
}

#[tokio::test]
async fn une_regle_modifiee_est_repoussee_au_tour_des_definitions() {
    let (faux, backend, hote, app, pousseur) = monde().await;
    hote.ranger(1000, 1, collection(&[10]));
    appel(&app, "PUT", "/circles/1/sets/smart_collection/1000", None).await;
    assert_eq!(corps_recus(&faux).len(), 1);

    // Définition inchangée : rien n'est même résolu.
    let avant = hote.resolutions.lock().unwrap().len();
    let b = pousseur.tour_des_definitions().await;
    assert_eq!(b.examines, 0, "{b:?}");
    assert_eq!(hote.resolutions.lock().unwrap().len(), avant);

    // La règle change : résolue et repoussée, sans attendre le battement.
    hote.ranger(1000, 1, collection(&[]));
    backend
        .execute_batch(
            "UPDATE smart_collections SET rules = '[{\"field\":\"year\",\"op\":\">\",\"value\":\"2000\"}]' WHERE id = 1000",
        )
        .unwrap();
    let lectures = faux.etat.lock().unwrap().lectures_des_ensembles;
    let b = pousseur.tour_des_definitions().await;
    assert_eq!(b.envoyes, 1, "{b:?}");
    assert_eq!(corps_recus(&faux)[1]["albums"], json!([]));
    assert_eq!(
        faux.etat.lock().unwrap().lectures_des_ensembles,
        lectures + 1,
        "le changement passe par `GET /sets`, la seule vérité sur ce qui est coché"
    );
}

#[tokio::test]
async fn une_etiquette_posee_est_repoussee_au_tour_des_definitions() {
    let (faux, backend, _h, app, pousseur) = monde().await;
    appel(&app, "PUT", "/circles/1/sets/tag/4", None).await;
    assert_eq!(corps_recus(&faux)[0]["albums"], json!([]));
    assert_eq!(pousseur.tour_des_definitions().await.envoyes, 0);

    backend
        .execute_batch("INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (4, 'album', 10)")
        .unwrap();
    assert_eq!(pousseur.tour_des_definitions().await.envoyes, 1);
    assert_eq!(corps_recus(&faux)[1]["albums"], json!([10]));
}

/// Couper le partage d'un cercle (ou le déplacer, ou délier le serveur)
/// SUPPRIME ses ensembles côté cloud. Le greffon ne les recrée jamais de
/// lui-même : ni au tour des définitions, ni au battement, même si
/// l'étiquette change ensuite et que le partage est rallumé.
#[tokio::test]
async fn un_ensemble_supprime_cote_cloud_n_est_jamais_repousse() {
    let (faux, backend, _h, app, pousseur) = monde().await;
    appel(&app, "PUT", "/circles/1/sets/tag/4", None).await;
    assert_eq!(corps_recus(&faux).len(), 1);
    assert_eq!(pousseur.connus().len(), 1, "témoin : l'ensemble est connu");

    // Le cloud supprime l'ensemble (partage coupé puis rallumé, par exemple).
    faux.etat.lock().unwrap().ensembles.clear();
    // L'étiquette change ensuite en local.
    backend
        .execute_batch("INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (4, 'album', 10)")
        .unwrap();

    let b = pousseur.tour_des_definitions().await;
    assert_eq!(b.envoyes, 0, "{b:?}");
    let b = pousseur.tour_complet().await;
    assert_eq!(b.envoyes, 0, "{b:?}");
    assert_eq!(
        corps_recus(&faux).len(),
        1,
        "un ensemble absent de `GET /sets` a été repoussé de mémoire : il serait \
         recoché sans que personne l'ait coché"
    );
    assert!(faux.etat.lock().unwrap().ensembles.is_empty());
    assert!(pousseur.connus().is_empty(), "la mémoire suit `GET /sets`");
}

#[tokio::test]
async fn le_battement_ignore_les_ensembles_d_un_autre_serveur() {
    let (faux, _b, _h, _app, pousseur) = monde().await;
    faux.etat.lock().unwrap().ensembles.push(json!({
        "id": 1, "circle_id": 1, "kind": "tag", "source_id": 3,
        "server_id": AUTRE_SERVEUR_DU_COMPTE, "profile_id": 1, "digest": "v1:x", "count": 0
    }));
    let b = pousseur.tour_complet().await;
    assert_eq!(b, tune_circle::battement::Bilan::default());
    assert!(corps_recus(&faux).is_empty());
}

#[tokio::test]
async fn une_etiquette_supprimee_en_local_est_retiree_du_cloud() {
    let (faux, backend, _h, app, pousseur) = monde().await;
    appel(&app, "PUT", "/circles/1/sets/tag/3", None).await;
    backend
        .execute_batch("DELETE FROM item_tags WHERE tag_id = 3; DELETE FROM streaming_item_tags WHERE tag_id = 3; DELETE FROM tags WHERE id = 3")
        .unwrap();
    let b = pousseur.tour_complet().await;
    assert_eq!(b.retires, 1, "{b:?}");
    let f = faux.etat.lock().unwrap();
    assert_eq!(
        f.ensembles_retires,
        vec![("1".to_string(), "tag".to_string(), "3".to_string())]
    );
    assert!(f.ensembles.is_empty());
}

#[tokio::test]
async fn decocher_relaie_le_delete_et_la_liste_le_dit() {
    let (faux, _b, _h, app, pousseur) = monde().await;
    appel(&app, "PUT", "/circles/1/sets/tag/3", None).await;
    let v = appel(&app, "GET", "/circles/1/sets", None).await.json();
    assert_eq!(v["tags"][0]["shared"], true, "{v}");
    let r = appel(&app, "DELETE", "/circles/1/sets/tag/3", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));
    let v = appel(&app, "GET", "/circles/1/sets", None).await.json();
    assert_eq!(v["tags"][0]["shared"], false, "{v}");
    assert!(pousseur.connus().is_empty());
    // Plus rien à pousser pour lui.
    assert_eq!(pousseur.tour_complet().await.examines, 0);
    assert_eq!(corps_recus(&faux).len(), 1);
}

// 4. Contact : relais fidèle, révocation --------------------------------------

#[tokio::test]
async fn un_contact_lit_les_rayons_puis_la_revocation_coupe_au_prochain_appel() {
    let (faux, _b, _h, app, _p) = monde().await;
    let chemins = [
        "/contacts/7/sets",
        "/contacts/7/sets/900",
        "/contacts/7/sets/900/albums?page=2",
        "/contacts/7/sets/900/tracks",
        "/contacts/7/sets/900/artists",
        "/contacts/7/sets/900/streaming",
    ];
    for c in chemins {
        let r = appel(&app, "GET", c, None).await;
        assert_eq!(r.statut, StatusCode::OK, "{c}");
    }
    // La requête de la lecture part telle quelle.
    appel(
        &app,
        "GET",
        "/contacts/7/sets/900/albums?page=2&sort=title",
        None,
    )
    .await;
    assert_eq!(
        faux.etat.lock().unwrap().derniere_requete.as_deref(),
        Some("page=2&sort=title")
    );

    // Révocation côté cloud : chaque route rend le 404 du cloud, le MÊME
    // qu'un rayon inconnu, et rien n'est servi de mémoire.
    faux.etat.lock().unwrap().contacts_qui_partagent.clear();
    let inconnu = appel(&app, "GET", "/contacts/8/sets/901", None).await;
    for c in chemins {
        let avant = faux.etat.lock().unwrap().appels;
        let r = appel(&app, "GET", c, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{c}");
        assert_eq!(r.octets, inconnu.octets, "{c}");
        assert_eq!(
            faux.etat.lock().unwrap().appels,
            avant + 1,
            "{c} : servi de mémoire"
        );
    }
}

#[tokio::test]
async fn une_route_de_rayon_inconnue_rend_404_sans_appel() {
    let (faux, _b, _h, app, _p) = monde().await;
    let avant = faux.etat.lock().unwrap().appels;
    let r = appel(&app, "GET", "/contacts/7/sets/900/playlists", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(faux.etat.lock().unwrap().appels, avant);
}

#[tokio::test]
async fn sans_session_les_rayons_rendent_412_et_le_battement_ne_fait_rien() {
    let faux = demarrer().await;
    let backend = base(&faux.base, None);
    let (app, pousseur) = app_avec_rayons(backend, Arc::new(FauxHote::default()));
    for (m, c) in [
        ("GET", "/circles/1/sets"),
        ("DELETE", "/circles/1/sets/tag/3"),
        ("GET", "/contacts/7/sets"),
    ] {
        let r = appel(&app, m, c, None).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {c}");
    }
    assert_eq!(
        pousseur.tour_complet().await,
        tune_circle::battement::Bilan::default()
    );
    assert_eq!(faux.etat.lock().unwrap().appels, 0);
}
