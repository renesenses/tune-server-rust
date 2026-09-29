//! #5319 (Marco Polo, fil 2009, v0.9.167) — un coffret composé À LA MAIN
//! (`POST /library/albums/coffret`) redevenait deux albums après un scan qui
//! relit les fichiers.
//!
//! La route réunissait les pistes et posait le marqueur `coffret = manuel`,
//! mais n'écrivait aucune TENUE (`edition_pistes`). Or le scan reconstruit
//! chaque ligne piste depuis ses balises (ou sa feuille CUE) et résout
//! l'album par le DOSSIER : chaque disque retournait à l'album de son
//! dossier, disque 1. Seules les tenues l'en empêchent — par chemin pour un
//! fichier, par identité `(cue_media_path, cue_start_ms)` pour une tranche
//! d'image, qui n'a pas de chemin.
//!
//! Les disques s'appellent *Disc A* / *Disc B* : la passe automatique ne lit
//! que des CHIFFRES (`coffrets::marqueur_final`), elle ne les réunit donc pas
//! d'elle-même — ce sont précisément les coffrets qui ne peuvent QUE être
//! composés à la main. Nommés *CD1* / *CD2*, la passe les aurait regroupés au
//! premier scan et l'épreuve ne verrait plus la route.
//!
//! Ces épreuves exécutent le VRAI scan forcé (`spawn_library_scan`, le bouton
//! « Analyse complète ») et la VRAIE route, sur une base SQLite de fichier.
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use crate::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tower::ServiceExt;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

struct Bibliotheque {
    _base: tune_core::test_scratch::ScratchDir,
    racine: tune_core::test_scratch::ScratchDir,
    etat: AppState,
    db: Arc<dyn DbBackend>,
}

fn bibliotheque(epreuve: &str) -> Bibliotheque {
    let base = tune_core::test_scratch::scratch_dir(&format!("coffret-5319-base-{epreuve}"));
    // Sous le dossier courant : `is_tune_temp_file` écarte le dossier
    // temporaire du système.
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("coffret-5319-{epreuve}"),
    );
    let etat = AppState::new(
        &base.join("tune-epreuve.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db = etat.backend.clone();
    SettingsRepo::with_backend(db.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    Bibliotheque {
        _base: base,
        racine,
        etat,
        db,
    }
}

/// Le scan FORCÉ (« Analyse complète ») jusqu'à sa fin annoncée : chaque
/// fichier relu, chaque ligne réécrite. Le verrou de sérialisation est tenu
/// à travers les `.await` à dessein : le droit de scanner est global au
/// processus (même motif que `scan_realigne_tests_4896::scan_manuel`).
#[allow(clippy::await_holding_lock)]
async fn scan_force(etat: &AppState) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), true, None).await {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan forcé n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => {
                crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
                return;
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus d'événements fermé : {e}"),
        }
    }
}

/// Un appel au routeur de production, avec un corps JSON.
async fn appel(etat: &AppState, methode: &str, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&corps).unwrap()))
        .unwrap();
    let reponse = crate::routes::router(etat.clone())
        .oneshot(requete)
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// `POST /api/v1/library/albums/coffret`.
async fn composer(etat: &AppState, ids: &[i64]) -> Value {
    let (statut, corps) = appel(
        etat,
        "POST",
        "/api/v1/library/albums/coffret",
        json!({ "album_ids": ids }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "composition refusée : {corps}");
    corps
}

/// `POST /api/v1/library/coffrets/{id}/defaire-manuel`.
async fn defaire(etat: &AppState, id: i64) -> (StatusCode, Value) {
    appel(
        etat,
        "POST",
        &format!("/api/v1/library/coffrets/{id}/defaire-manuel"),
        Value::Null,
    )
    .await
}

/// La vue d'édition (`GET …/edition`) : ce que montre l'écran « Modifier ».
async fn vue_edition(etat: &AppState, id: i64) -> Value {
    let (statut, corps) = appel(
        etat,
        "GET",
        &format!("/api/v1/library/albums/{id}/edition"),
        Value::Null,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "vue d'édition : {corps}");
    corps
}

/// Les clés de `album_metadata` d'un album.
fn cles(db: &Arc<dyn DbBackend>, id: i64) -> Vec<String> {
    let mut v: Vec<String> = db
        .query_many(
            "SELECT key FROM album_metadata WHERE album_id = ?",
            &[&id as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_string())
        .collect();
    v.sort();
    v
}

/// `(identité, album_id, disc_number)` de chaque piste, triées par identité
/// — le chemin du fichier, ou `image@début` pour une tranche CUE.
fn disposition(db: &Arc<dyn DbBackend>) -> Vec<(String, i64, i64)> {
    let mut v: Vec<(String, i64, i64)> = db
        .query_many(
            "SELECT COALESCE(file_path, cue_media_path || '@' || cue_start_ms), \
             album_id, disc_number FROM tracks",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap_or_default(),
                r[1].as_i64().unwrap_or(-1),
                r[2].as_i64().unwrap_or(-1),
            )
        })
        .collect();
    v.sort();
    v
}

/// `(id, titre)` des albums, triés par id.
fn albums(db: &Arc<dyn DbBackend>) -> Vec<(i64, String)> {
    db.query_many("SELECT id, title FROM albums ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].as_string().unwrap_or_default()))
        .collect()
}

/// L'album de la piste dont l'identité commence par `prefixe`.
fn album_de(db: &Arc<dyn DbBackend>, prefixe: &str) -> i64 {
    disposition(db)
        .into_iter()
        .find(|(c, _, _)| c.starts_with(prefixe))
        .unwrap_or_else(|| panic!("aucune piste sous {prefixe}"))
        .1
}

/// Composer, puis relire tous les fichiers : le coffret doit rester entier.
async fn composer_puis_rescanner(b: &Bibliotheque, cd1: &Path, cd2: &Path) {
    scan_force(&b.etat).await;
    let d1 = album_de(&b.db, &cd1.to_string_lossy());
    let d2 = album_de(&b.db, &cd2.to_string_lossy());
    assert_ne!(d1, d2, "montage : un album par dossier de disque");

    let corps = composer(&b.etat, &[d1, d2]).await;
    assert_eq!(corps["cible"].as_i64(), Some(d1));
    let compose = disposition(&b.db);
    assert!(
        compose.iter().all(|(_, a, _)| *a == d1),
        "montage : la route a réuni les pistes : {compose:?}"
    );
    let disques: Vec<i64> = compose.iter().map(|(_, _, d)| *d).collect();
    assert_eq!(disques, vec![1, 1, 2, 2], "montage : {compose:?}");
    let titre = albums(&b.db);
    let vue = vue_edition(&b.etat, d1).await;
    assert!(
        vue["album"]["champs_edites"]
            .as_array()
            .is_some_and(|c| c.iter().any(|v| v == "title")),
        "le titre composé est tenu comme modifié à la main : {}",
        vue["album"]
    );
    assert_eq!(
        vue["defaire_coffret_manuel"], true,
        "la sonde du bouton web"
    );

    scan_force(&b.etat).await;

    assert_eq!(
        disposition(&b.db),
        compose,
        "le scan forcé a défait le coffret composé à la main"
    );
    assert_eq!(
        albums(&b.db),
        titre,
        "un seul album, le coffret, sous le titre que la composition lui a donné"
    );
}

/// LE TÉMOIN — deux disques en pistes séparées, un dossier par disque.
#[tokio::test]
async fn un_coffret_compose_survit_au_scan_force_5319() {
    let b = bibliotheque("pistes");
    let d = disques_flac(&b);
    composer_puis_rescanner(&b, &d[0], &d[1]).await;
}

/// Le cas du testeur — chaque disque est UNE image APE et sa feuille CUE :
/// des tranches sans `file_path`, désignées par `(image, début)`.
#[tokio::test]
async fn un_coffret_compose_d_images_cue_survit_au_scan_force_5319() {
    let b = bibliotheque("cue");
    let i = images_cue(&b);
    scan_force(&b.etat).await;
    let montage = disposition(&b.db);
    assert!(
        montage.len() == 4 && montage.iter().all(|(c, _, _)| c.contains(".ape@")),
        "montage : quatre tranches CUE, sans chemin : {montage:?}"
    );
    composer_puis_rescanner(&b, &i[0], &i[1]).await;
}

/// `(identité, titre)` de chaque piste, triées par identité.
fn titres_des_pistes(db: &Arc<dyn DbBackend>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = db
        .query_many(
            "SELECT COALESCE(file_path, cue_media_path || '@' || cue_start_ms), title FROM tracks",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap_or_default(),
                r[1].as_string().unwrap_or_default(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Composer, renommer une piste à la main, DÉFAIRE, puis relire tous les
/// fichiers : chaque disque est revenu à l'album de son dossier, sous son
/// titre, et le renommage de piste a survécu.
async fn composer_defaire_puis_rescanner(b: &Bibliotheque, cd1: &Path, cd2: &Path) {
    scan_force(&b.etat).await;
    let d1 = album_de(&b.db, &cd1.to_string_lossy());
    let d2 = album_de(&b.db, &cd2.to_string_lossy());
    let avant = albums(&b.db);
    let titres_avant: Vec<String> = avant.iter().map(|(_, t)| t.clone()).collect();
    composer(&b.etat, &[d1, d2]).await;

    // Une modification à la main SUR UNE PISTE, que « défaire » doit garder.
    let piste_b: i64 =
        b.db.query_many(
            "SELECT id FROM tracks WHERE COALESCE(file_path, cue_media_path) LIKE ? \
             ORDER BY COALESCE(cue_start_ms, 0), file_path LIMIT 1",
            &[&format!("{}%", cd2.to_string_lossy()) as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap()[0][0]
            .as_i64()
            .unwrap();
    let (statut, corps) = appel(
        &b.etat,
        "PUT",
        &format!("/api/v1/library/albums/{d1}/edition"),
        json!({ "tracks": [ { "id": piste_b, "title": "Renommée à la main" } ] }),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "montage : renommage refusé : {corps}"
    );

    let (statut, corps) = defaire(&b.etat, d1).await;
    assert_eq!(statut, StatusCode::OK, "défaire refusé : {corps}");
    assert_eq!(corps["cible"].as_i64(), Some(d1));
    let recrees = corps["albums_recrees"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        recrees.len(),
        1,
        "un album recréé pour le disque 2 : {corps}"
    );
    let d2_neuf = recrees[0].as_i64().unwrap();

    let defait = disposition(&b.db);
    for (chemin, album, disque) in &defait {
        let attendu = if chemin.starts_with(&*cd1.to_string_lossy()) {
            d1
        } else {
            d2_neuf
        };
        assert_eq!(
            (*album, *disque),
            (attendu, 1),
            "{chemin} : chaque disque redevient son album, disque 1 : {defait:?}"
        );
    }
    let titres: Vec<String> = albums(&b.db).into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        titres, titres_avant,
        "chaque album reprend son titre d'origine"
    );
    assert_eq!(
        cles(&b.db, d1),
        Vec::<String>::new(),
        "ni marqueur de coffret, ni titre tenu, ni disposition sur le premier disque"
    );
    assert_eq!(
        cles(&b.db, d2_neuf),
        vec!["edition_pistes".to_string()],
        "le disque 2 ne garde que le renommage de sa piste"
    );
    let tenue: String =
        b.db.query_many(
            "SELECT value FROM album_metadata WHERE album_id = ? AND key = 'edition_pistes'",
            &[&d2_neuf as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap()[0][0]
            .as_string()
            .unwrap();
    assert!(
        tenue.contains("\"disposition\":false") && tenue.contains("Renommée à la main"),
        "renommage gardé, disposition retirée : {tenue}"
    );
    let renommees = titres_des_pistes(&b.db);

    scan_force(&b.etat).await;

    assert_eq!(
        disposition(&b.db),
        defait,
        "après le scan forcé, chaque disque est resté l'album de son dossier"
    );
    let titres: Vec<String> = albums(&b.db).into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        titres, titres_avant,
        "deux albums, sous leurs titres d'origine"
    );
    assert_eq!(
        titres_des_pistes(&b.db),
        renommees,
        "le renommage de piste fait à la main a survécu à « défaire » et au scan"
    );
    assert!(
        renommees.iter().any(|(_, t)| t == "Renommée à la main"),
        "montage : la piste renommée : {renommees:?}"
    );
}

fn disques_flac(b: &Bibliotheque) -> Vec<std::path::PathBuf> {
    let parent = b.racine.join("Handel - Messiah, Gardiner (Philips 2CD)");
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    let mut disques = Vec::new();
    for cd in ["Disc A", "Disc B"] {
        let dossier = parent.join(cd);
        std::fs::create_dir_all(&dossier).unwrap();
        for n in 1..=2 {
            let piste = dossier.join(format!("0{n}.flac"));
            std::fs::write(&piste, flac_8_canaux()).unwrap();
            baliser(
                &piste,
                &[
                    ("TITLE", &format!("{cd} piste {n}")),
                    ("ARTIST", "G. F. Handel"),
                    ("ALBUMARTIST", "G. F. Handel"),
                    ("ALBUM", &format!("Messiah - Gardiner - {cd}")),
                    ("TRACKNUMBER", &n.to_string()),
                ],
                hier,
            );
        }
        disques.push(dossier);
    }
    disques
}

fn images_cue(b: &Bibliotheque) -> Vec<std::path::PathBuf> {
    let parent = b.racine.join("Handel - Messiah, Gardiner (Philips 2CD)");
    let mut images = Vec::new();
    for (n, cd) in [(1, "Disc A"), (2, "Disc B")] {
        let dossier = parent.join(cd);
        std::fs::create_dir_all(&dossier).unwrap();
        let image = dossier.join(format!("CDImage{n}.ape"));
        // L'extension suffit au plan (`native_decoder_supports`) : le contenu
        // n'est jamais décodé par le scan.
        std::fs::write(&image, flac_8_canaux()).unwrap();
        std::fs::write(
            dossier.join(format!("CDImage{n}.cue")),
            format!(
                "PERFORMER \"G. F. Handel\"\r\n\
                 TITLE \"Messiah - Gardiner - {cd}\"\r\n\
                 FILE \"CDImage{n}.ape\" WAVE\r\n\
                 \x20 TRACK 01 AUDIO\r\n\
                 \x20   TITLE \"{cd} Sinfony\"\r\n\
                 \x20   INDEX 01 00:00:00\r\n\
                 \x20 TRACK 02 AUDIO\r\n\
                 \x20   TITLE \"{cd} Comfort ye\"\r\n\
                 \x20   INDEX 01 00:00:03\r\n"
            ),
        )
        .unwrap();
        images.push(image);
    }
    images
}

/// Défaire, en pistes séparées (décision de Bertrand du 29/09/2026).
#[tokio::test]
async fn defaire_un_coffret_compose_rend_chaque_disque_a_son_dossier_5319() {
    let b = bibliotheque("defaire-pistes");
    let d = disques_flac(&b);
    composer_defaire_puis_rescanner(&b, &d[0], &d[1]).await;
}

/// Défaire, en images APE + CUE.
#[tokio::test]
async fn defaire_un_coffret_compose_d_images_cue_rend_chaque_disque_5319() {
    let b = bibliotheque("defaire-cue");
    let i = images_cue(&b);
    composer_defaire_puis_rescanner(&b, &i[0], &i[1]).await;
}

/// Un album qui n'est pas un coffret MANUEL : refus clair, rien d'écrit.
#[tokio::test]
async fn defaire_refuse_ce_qui_n_est_pas_un_coffret_manuel_5319() {
    let b = bibliotheque("defaire-refus");
    let d = disques_flac(&b);
    scan_force(&b.etat).await;
    let d1 = album_de(&b.db, &d[0].to_string_lossy());
    let avant = (disposition(&b.db), albums(&b.db));

    let (statut, corps) = defaire(&b.etat, d1).await;
    assert_eq!(statut, StatusCode::CONFLICT, "album ordinaire : {corps}");
    assert_eq!(corps["error"], "pas_un_coffret_manuel", "{corps}");

    // Un coffret AUTOMATIQUE non plus : il a sa propre route.
    b.db.execute(
        "INSERT INTO album_metadata (album_id, key, value) VALUES (?, 'coffret', ?)",
        &[
            &d1 as &dyn tune_core::db::backend::ToSqlValue,
            &r#"{"origine":"auto","cle":"x","disques":[]}"#.to_string(),
        ],
    )
    .unwrap();
    let (statut, corps) = defaire(&b.etat, d1).await;
    assert_eq!(
        statut,
        StatusCode::CONFLICT,
        "coffret automatique : {corps}"
    );

    let (statut, _) = defaire(&b.etat, 999_999).await;
    assert_eq!(statut, StatusCode::NOT_FOUND, "album inconnu");
    assert_eq!((disposition(&b.db), albums(&b.db)), avant, "rien n'a bougé");
}
