//! Tune Circle, étape T2 (#5325) : le catalogue d'un contact, en lecture.
//!
//! Le relais des routes de partage et de lecture contre le faux mozaiklabs
//! qui implémente le contrat de l'issue, et la copie en ligne que pousse
//! `library_sync` : qui pousse (un compte gratuit qui partage), et ce qui ne
//! part jamais (un chemin de fichier).

mod commun;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tune_core::cloud::library_sync;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use commun::*;

/// Une base connectée au faux cloud, avec le `server_id` de CE serveur.
fn base_du_serveur(url: &str, server_id: Option<&str>) -> Arc<dyn DbBackend> {
    let backend = base(url, Some(JETON));
    if let Some(id) = server_id {
        SettingsRepo::with_backend(backend.clone())
            .set("server_id", id)
            .unwrap();
    }
    backend
}

fn partage_note(backend: &Arc<dyn DbBackend>) -> bool {
    library_sync::partage_de_cercle_actif(&SettingsRepo::with_backend(backend.clone()))
}

/// Toutes les routes T2 relayées vers le cloud.
const ROUTES_T2: [(&str, &str); 8] = [
    ("PUT", "/circles/1/sharing/library"),
    ("DELETE", "/circles/1/sharing/library"),
    ("GET", "/shared-with-me"),
    ("GET", "/contacts/7/library/stats"),
    ("GET", "/contacts/7/library/artists"),
    ("GET", "/contacts/7/library/albums"),
    ("GET", "/contacts/7/library/albums/10/tracks"),
    ("GET", "/contacts/7/library/tracks"),
];

// 1. Non connecté ----------------------------------------------------------

#[tokio::test]
async fn sans_session_les_routes_t2_rendent_412_et_rien_ne_part() {
    let faux = demarrer().await;
    let backend = base(&faux.base, None);
    SettingsRepo::with_backend(backend.clone())
        .set("server_id", SERVEUR_DU_COMPTE)
        .unwrap();
    let app = commun::app(backend);
    for (m, chemin) in ROUTES_T2 {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" }),
            "{m} {chemin}"
        );
    }
    assert_eq!(faux.etat.lock().unwrap().appels, 0);
}

/// Session mozaiklabs refusée (401, rafraîchissement refusé) : l'état « non
/// connecté », jamais un 401 qui déconnecterait l'utilisateur de Tune.
#[tokio::test]
async fn un_401_du_cloud_sur_une_lecture_rend_non_connecte_jamais_401() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().rafraichissement_valide = "autre".into();
    let backend = base(&faux.base, Some(JETON_PERIME));
    let app = commun::app(backend);
    for (m, chemin) in ROUTES_T2 {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" })
        );
    }
}

// 2. Partager : le server_id est celui du serveur, jamais celui du client ---

#[tokio::test]
async fn partager_joint_le_server_id_du_reglage_jamais_celui_du_client() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().t2 = true;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    let app = commun::app(backend.clone());

    // Le client tente d'imposer un autre serveur, du compte ou non.
    for impose in [AUTRE_SERVEUR_DU_COMPTE, SERVEUR_D_UN_AUTRE] {
        let r = appel(
            &app,
            "PUT",
            "/circles/1/sharing/library",
            Some(json!({ "server_id": impose, "circle_id": 2 })),
        )
        .await;
        assert_eq!(r.statut, StatusCode::OK);
        assert_eq!(
            faux.etat.lock().unwrap().dernier_corps_partage,
            Some(json!({ "server_id": SERVEUR_DU_COMPTE })),
            "seul le server_id du réglage part, rien de ce que le client envoie"
        );
    }
    assert_eq!(
        r_sharing(&faux, 1),
        json!({ "library": true, "server_id": SERVEUR_DU_COMPTE })
    );
    assert!(partage_note(&backend), "le partage de CE serveur est noté");
}

fn r_sharing(faux: &Serveur, cercle: i64) -> Value {
    faux.etat.lock().unwrap().partage_du_cercle(&json!(cercle))
}

#[tokio::test]
async fn le_refus_du_cloud_est_relaye_et_ne_note_aucun_partage() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().t2 = true;

    // Un server_id local qui n'est pas un serveur du compte : 404 du cloud.
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_D_UN_AUTRE));
    let app = commun::app(backend.clone());
    let r = appel(&app, "PUT", "/circles/1/sharing/library", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));
    assert!(!partage_note(&backend));

    // Le cercle d'un autre : 404.
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    let app = commun::app(backend.clone());
    let chemin = format!("/circles/{CERCLE_D_UN_AUTRE}/sharing/library");
    let r = appel(&app, "PUT", &chemin, None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert!(!partage_note(&backend));

    // Aucun server_id local : `null` part, le cloud juge (422).
    let backend = base_du_serveur(&faux.base, None);
    let app = commun::app(backend.clone());
    let r = appel(&app, "PUT", "/circles/1/sharing/library", None).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json(),
        corps_de_validation("server_id", MESSAGE_SERVER_ID)
    );
    assert_eq!(
        faux.etat.lock().unwrap().dernier_corps_partage,
        Some(json!({ "server_id": null }))
    );
    assert!(!partage_note(&backend));
    assert!(faux.etat.lock().unwrap().partages.is_empty());
}

// 3. Couper : effet immédiat, et le booléen suit la vérité du cloud ----------

#[tokio::test]
async fn couper_le_dernier_partage_eteint_la_poussee_un_autre_la_garde() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().t2 = true;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    let app = commun::app(backend.clone());

    for c in [1, 2] {
        let r = appel(&app, "PUT", &format!("/circles/{c}/sharing/library"), None).await;
        assert_eq!(r.statut, StatusCode::OK);
    }
    let r = appel(&app, "DELETE", "/circles/1/sharing/library", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));
    assert!(
        partage_note(&backend),
        "le cercle 2 partage encore ce serveur"
    );

    // Supprimer le cercle 2 supprime son partage (cascade côté cloud).
    faux.etat.lock().unwrap().partages.retain(|(c, _)| *c != 2);
    let r = appel(&app, "DELETE", "/circles/2", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert!(!partage_note(&backend), "plus aucun cercle ne partage");
}

/// Un partage allumé ou coupé ailleurs (autre appareil, cascade) : le relais
/// suivant de `GET /` remet le booléen à la vérité du cloud. La liste, elle,
/// part à l'octet près.
#[tokio::test]
async fn le_get_de_la_liste_remet_le_partage_a_la_verite_du_cloud() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().t2 = true;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    let app = commun::app(backend.clone());

    faux.etat
        .lock()
        .unwrap()
        .partages
        .push((2, SERVEUR_DU_COMPTE.into()));
    let r = appel(&app, "GET", "/", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    let attendu = serde_json::to_vec(&faux.etat.lock().unwrap().cercle()).unwrap();
    assert_eq!(r.octets, attendu, "à l'octet près, `sharing` compris");
    assert_eq!(
        r.json()["circles"][1]["sharing"],
        json!({ "library": true, "server_id": SERVEUR_DU_COMPTE })
    );
    assert!(partage_note(&backend));

    // Le partage d'un AUTRE serveur du compte ne fait pas pousser celui-ci.
    faux.etat.lock().unwrap().partages = vec![(2, AUTRE_SERVEUR_DU_COMPTE.into())];
    appel(&app, "GET", "/", None).await;
    assert!(!partage_note(&backend));
}

// 4. Lectures : relais fidèle, requête comprise ------------------------------

#[tokio::test]
async fn les_lectures_d_un_contact_sont_relayees_a_l_octet_requete_comprise() {
    let faux = demarrer().await;
    let app = commun::app(base(&faux.base, Some(JETON)));

    let r = appel(&app, "GET", "/shared-with-me", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!([{ "user_id": 7, "name": "Alice", "library": true }])
    );

    for (chemin, quoi) in [
        ("/contacts/7/library/stats", "stats"),
        ("/contacts/7/library/artists", "artists"),
        ("/contacts/7/library/albums", "albums"),
        ("/contacts/7/library/albums/10/tracks", "tracks"),
        ("/contacts/7/library/tracks", "tracks"),
    ] {
        let r = appel(&app, "GET", chemin, None).await;
        assert_eq!(r.statut, StatusCode::OK, "{chemin}");
        assert_eq!(
            r.octets,
            serde_json::to_vec(&catalogue(quoi)).unwrap(),
            "{chemin}"
        );
        assert_eq!(faux.etat.lock().unwrap().derniere_requete, None, "{chemin}");
    }

    for requete in [
        "page=2&search=Miles%20Davis&sort=title",
        "artist=Miles%20Davis&page=1",
        "search=caf%C3%A9&sort=-year",
    ] {
        let r = appel(
            &app,
            "GET",
            &format!("/contacts/7/library/albums?{requete}"),
            None,
        )
        .await;
        assert_eq!(r.statut, StatusCode::OK);
        assert_eq!(
            faux.etat.lock().unwrap().derniere_requete.as_deref(),
            Some(requete),
            "la requête part telle quelle"
        );
    }
}

#[tokio::test]
async fn le_429_d_une_lecture_est_relaye_avec_son_delai() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().lectures_permises = 0;
    let app = commun::app(base(&faux.base, Some(JETON)));
    let r = appel(&app, "GET", "/contacts/7/library/tracks?page=3", None).await;
    assert_eq!(r.statut, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json(), json!({ "message": "Too Many Attempts." }));
    assert_eq!(r.entetes.get("retry-after").unwrap(), "17");
}

#[tokio::test]
async fn un_identifiant_qui_n_est_pas_un_contact_qui_partage_rend_le_404_du_cloud() {
    let faux = demarrer().await;
    let app = commun::app(base(&faux.base, Some(JETON)));
    for chemin in [
        "/contacts/9/library/albums",
        "/contacts/12345/library/stats",
        // Un identifiant qui tente de sortir de son segment reste UN segment.
        "/contacts/x%2F..%2F..%2Fmembers%2F7/library/stats",
        "/contacts/7%3Fpage=1/library/tracks",
    ] {
        let r = appel(&app, "GET", chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{chemin}");
        assert_eq!(r.json(), json!({ "error": "not_found" }), "{chemin}");
    }
    assert_eq!(faux.etat.lock().unwrap().members.len(), 2);
}

// 5. Révocation relayée, sans mémoire -----------------------------------------

/// Le cas qui compte : le cloud passe à 404 entre deux appels (révocation,
/// retrait du cercle, partage coupé). Le second appel rend ce 404 tel quel —
/// rien n'est servi de mémoire, chaque lecture repart vers le cloud.
#[tokio::test]
async fn la_revocation_vue_par_le_cloud_coupe_la_lecture_suivante() {
    let faux = demarrer().await;
    let app = commun::app(base(&faux.base, Some(JETON)));
    let chemins = [
        "/contacts/7/library/stats",
        "/contacts/7/library/artists",
        "/contacts/7/library/albums",
        "/contacts/7/library/albums/10/tracks",
        "/contacts/7/library/tracks",
    ];
    for chemin in chemins {
        assert_eq!(
            appel(&app, "GET", chemin, None).await.statut,
            StatusCode::OK,
            "{chemin}"
        );
    }

    // Alice révoque, de son côté : seul le cloud le sait.
    {
        let mut f = faux.etat.lock().unwrap();
        f.contacts_qui_partagent.clear();
        f.partagent_avec_moi.clear();
    }
    let appels_avant = faux.etat.lock().unwrap().appels;
    for chemin in chemins {
        let r = appel(&app, "GET", chemin, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{chemin}");
        assert_eq!(r.octets, br#"{"error":"not_found"}"#.to_vec(), "{chemin}");
    }
    assert_eq!(
        faux.etat.lock().unwrap().appels,
        appels_avant + chemins.len(),
        "chaque lecture doit repartir vers le cloud"
    );
    let r = appel(&app, "GET", "/shared-with-me", None).await;
    assert_eq!(r.json(), json!([]));
}

#[tokio::test]
async fn un_cloud_en_panne_rend_503_sur_une_lecture() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().panne = true;
    let app = commun::app(base(&faux.base, Some(JETON)));
    let r = appel(&app, "GET", "/contacts/7/library/albums?page=1", None).await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.json(),
        json!({ "connected": true, "code": "circle.cloud_unavailable", "upstream_status": 500 })
    );
}

// 6. La copie en ligne : qui pousse, et ce qui ne part jamais -----------------

const CHEMIN_DE_LA_PISTE: &str =
    "/Users/secret-5325/Musique/Miles Davis/Kind of Blue/01 So What.flac";
const DOSSIER_DE_L_ALBUM: &str = "/Users/secret-5325/Musique/Miles Davis/Kind of Blue";

/// Un artiste, un album et une piste LOCALE, dont chaque colonne de chemin
/// porte un chemin connu — `source_id` compris, le pire cas.
fn semer_une_bibliotheque(backend: &Arc<dyn DbBackend>) -> (i64, i64) {
    backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis');\
             INSERT INTO albums (id, title, artist_id, year, genre, cover_path, folder_path, \
                                 cover_source_path) \
             VALUES (10, 'Kind of Blue', 20, 1959, 'Jazz', '{d}/cover.jpg', '{d}', '{d}/folder.jpg');\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, sample_rate, \
                                 bit_depth, duration_ms, genre, track_number, disc_number, source, \
                                 source_id, isrc, cover_path, cue_media_path) \
             VALUES (1, 'So What', 10, 20, '{p}', 'flac', 96000, 24, 562000, 'Jazz', 1, 1, \
                     'local', '{p}', 'USSM15900113', '{d}/so-what.jpg', '{p}');\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source, source_id) \
             VALUES (2, 'Freddie Freeloader', 10, 20, 'C:\\Musique\\02.flac', 'flac', 'local', \
                     'C:\\Musique\\02.flac');",
            d = DOSSIER_DE_L_ALBUM,
            p = CHEMIN_DE_LA_PISTE,
        ))
        .unwrap();
    library_sync::populate_changelog_after_scan(backend);
    (10, 1)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn api(faux: &Serveur) -> String {
    format!("{}/api/v1/cloud-library", faux.base)
}

/// Décision de Bertrand du 28/09 : un compte GRATUIT qui partage sa
/// bibliothèque avec un cercle pousse son catalogue. Sans partage, rien ne
/// part ; Premium pousse comme avant.
#[tokio::test]
async fn un_compte_gratuit_qui_partage_pousse_son_catalogue() {
    let faux = demarrer().await;
    faux.etat.lock().unwrap().t2 = true;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    semer_une_bibliotheque(&backend);

    // Gratuit, sans partage : rien ne part.
    let issue = library_sync::cycle(&backend, &client(), &api(&faux), false).await;
    assert!(issue.is_none(), "gratuit sans partage : aucune poussée");
    assert!(faux.etat.lock().unwrap().synchros.is_empty());

    // Le propriétaire allume le partage d'un cercle, par le greffon.
    let app = commun::app(backend.clone());
    let r = appel(&app, "PUT", "/circles/1/sharing/library", None).await;
    assert_eq!(r.statut, StatusCode::OK);

    // Gratuit, qui partage : le catalogue part, sous le server_id du serveur.
    let issue = library_sync::cycle(&backend, &client(), &api(&faux), false).await;
    let rapport = issue
        .expect("gratuit qui partage : la poussée doit partir")
        .unwrap();
    assert!(rapport.errors.is_empty(), "{:?}", rapport.errors);
    assert_eq!(rapport.tracks_synced, 2, "les deux pistes sont parties");
    assert!(rapport.albums_synced >= 1 && rapport.artists_synced >= 1);
    assert_eq!(library_sync::pending_count(&backend), 0);
    {
        let f = faux.etat.lock().unwrap();
        assert!(!f.synchros.is_empty());
        assert!(f.synchros.iter().all(|(s, _)| s == SERVEUR_DU_COMPTE));
    }
    assert!(
        SettingsRepo::with_backend(backend.clone())
            .get("cloud_library_last_sync")
            .unwrap()
            .is_some()
    );

    // Coupé : plus rien ne part, même avec des changements en attente.
    let r = appel(&app, "DELETE", "/circles/1/sharing/library", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    library_sync::record_change(&backend, "track", 1, "upsert");
    let avant = faux.etat.lock().unwrap().synchros.len();
    assert!(
        library_sync::cycle(&backend, &client(), &api(&faux), false)
            .await
            .is_none()
    );
    assert_eq!(faux.etat.lock().unwrap().synchros.len(), avant);

    // Premium pousse, partage ou non.
    assert!(
        library_sync::cycle(&backend, &client(), &api(&faux), true)
            .await
            .is_some()
    );
    assert_eq!(faux.etat.lock().unwrap().synchros.len(), avant + 1);
}

/// La garde de #5325 : pour une piste locale dont `file_path` (et chaque
/// colonne de chemin, `source_id` compris) vaut un chemin connu, la charge
/// POUSSÉE — sérialisation complète, reçue par le faux cloud — ne contient ce
/// chemin nulle part, ni son dossier, ni le nom du fichier.
#[tokio::test]
async fn aucun_chemin_de_fichier_ne_part_dans_la_copie_en_ligne() {
    let faux = demarrer().await;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    semer_une_bibliotheque(&backend);

    library_sync::push_changes_vers(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let corps: String = faux
        .etat
        .lock()
        .unwrap()
        .synchros
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!corps.is_empty(), "la charge doit avoir été reçue");
    // Sérialisée par serde_json, une barre oblique reste `/` : la chaîne
    // brute se compare telle quelle ; l'antislash, lui, est doublé.
    for interdit in [
        CHEMIN_DE_LA_PISTE,
        DOSSIER_DE_L_ALBUM,
        "/Users/",
        "secret-5325",
        "01 So What.flac",
        "cover.jpg",
        "C:\\\\Musique",
        "02.flac",
        "file_path",
        "cover_path",
        "folder_path",
        "cue_media_path",
    ] {
        assert!(
            !corps.contains(interdit),
            "`{interdit}` est parti dans la copie en ligne : {corps}"
        );
    }
}

/// #5325 : une piste porte l'identifiant de son album — l'`id` de l'album
/// poussé — et les champs décidés le 28/09 (année, nombre de pistes, ISRC).
#[tokio::test]
async fn la_piste_porte_son_album_id_et_les_champs_decides() {
    let faux = demarrer().await;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    let (album_id, piste_id) = semer_une_bibliotheque(&backend);

    library_sync::push_changes_vers(&backend, &client(), &api(&faux), SERVEUR_DU_COMPTE, JETON)
        .await
        .unwrap();
    let changements: Vec<Value> = faux
        .etat
        .lock()
        .unwrap()
        .synchros
        .iter()
        .flat_map(|(_, c)| {
            serde_json::from_str::<Value>(c).unwrap()["changes"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    let trouver = |t: &str, id: i64| {
        changements
            .iter()
            .find(|c| c["type"] == t && c["id"] == id)
            .unwrap_or_else(|| panic!("{t} {id} absent : {changements:?}"))
            .clone()
    };
    let piste = trouver("track", piste_id);
    let album = trouver("album", album_id);
    assert_eq!(piste["data"]["album_id"], json!(album_id));
    assert_eq!(album["id"], json!(album_id));
    assert_eq!(piste["data"]["isrc"], json!("USSM15900113"));
    assert_eq!(album["data"]["year"], json!(1959));
    assert_eq!(album["data"]["track_count"], json!(2));
    // Le chemin rangé dans `source_id` est tu, la clé reste (le cloud l'attend).
    assert_eq!(piste["data"]["source_id"], Value::Null);
}

// 7. GET /library-sync : l'état LOCAL de la copie en ligne ---------------------

#[tokio::test]
async fn l_etat_de_la_copie_en_ligne_est_local() {
    let faux = demarrer().await;
    let backend = base_du_serveur(&faux.base, Some(SERVEUR_DU_COMPTE));
    semer_une_bibliotheque(&backend);
    let app = commun::app(backend.clone());

    let pending = library_sync::pending_count(&backend);
    assert!(pending >= 4, "artiste, album et deux pistes en attente");
    let r = appel(&app, "GET", "/library-sync", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "server_id": SERVEUR_DU_COMPTE, "premium": false, "active": false,
                "last_sync": null, "pending": pending })
    );
    assert_eq!(faux.etat.lock().unwrap().appels, 0, "aucun appel au cloud");

    let s = SettingsRepo::with_backend(backend.clone());
    s.set(library_sync::CLE_PARTAGE_DE_CERCLE, "true").unwrap();
    // La route de synchro manuelle écrit des secondes : une seule forme sort.
    s.set("cloud_library_last_sync", "1790000000").unwrap();
    let r = appel(&app, "GET", "/library-sync", None).await;
    assert_eq!(
        r.json(),
        json!({ "server_id": SERVEUR_DU_COMPTE, "premium": false, "active": true,
                "last_sync": "2026-09-21T14:13:20+00:00", "pending": pending })
    );

    // Sans session SSO, rien ne peut partir : inactive.
    s.set("mozaik_access_token", "").unwrap();
    assert_eq!(
        appel(&app, "GET", "/library-sync", None).await.json()["active"],
        json!(false)
    );

    // Sans `server_id` local : `null`, et l'écran sait qu'aucun partage ne
    // peut venir de ce serveur.
    s.set("server_id", "").unwrap();
    assert_eq!(
        appel(&app, "GET", "/library-sync", None).await.json()["server_id"],
        Value::Null
    );
}
