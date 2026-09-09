//! Bandcamp est un SERVICE du registre, pas seulement un greffon (#2702, #2778).
//!
//! # Ce qui n'allait pas
//!
//! Bandcamp montait des routes sous `/api/v1/ext/bandcamp/…` et n'existait
//! nulle part dans `AppState::services`. Or les deux SEULES routes qui savent
//! construire une file complète — `POST /zones/{id}/play` avec
//! `streaming_album_id` ou `streaming_playlist_id` — commencent par
//! `registry.get(source)`. Pour `source = "bandcamp"` elles répondaient donc
//! `400 unknown service: bandcamp`, et il ne restait au client que le chemin
//! « piste distante seule », qui termine par `update_queue_info(zone, 0, 1)` :
//! une file d'EXACTEMENT une piste.
//!
//! C'est le défaut de Sevy Tabroc — « les morceaux Bandcamp ne s'enchaînent
//! pas » (#2702). Il n'y avait jamais de piste suivante : le poller trouvait
//! une file de longueur 1 et s'arrêtait.
//!
//! Et côté FabienM (#2778), l'état de liaison de « Ma collection » n'était
//! lisible par AUCUNE route : le greffon écrit `bandcamp_username` et
//! `bandcamp_fan_id` sans jamais les rendre.
//!
//! # Ce que ce fichier cloue
//!
//! 1. Bandcamp EST dans le registre — c'est la condition que les routes de
//!    file interrogent, et le seul geste qui les débloque.
//! 2. Une demande d'album Bandcamp ne rend plus `unknown service` : elle
//!    atteint l'adaptateur, qui NOMME son échec.
//! 3. Une playlist Bandcamp — qui n'existe pas chez Bandcamp — se refuse en le
//!    disant, au lieu de se confondre avec un service inconnu.
//! 4. Le TÉMOIN : les cinq services déjà inscrits sont toujours là, et
//!    répondent comme avant.
//! 5. L'état de liaison se lit par une route (#2778).
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.
//!
//! # Pourquoi ce fichier porte `#![cfg(feature = "bandcamp")]`
//!
//! Le service n'existe QUE sous cette fonctionnalité, et ce n'est pas un
//! choix de ce lot : `bandcamp = ["dep:tune-bandcamp"]` dans
//! `tune-server/Cargo.toml`, et `tune-bandcamp` y est déclarée
//! `optional = true`. Sans la fonctionnalité, la caisse du greffon n'est pas
//! compilée du tout : `BandcampService` n'existe pas, et l'inscription de
//! `state.rs` — elle-même sous `#[cfg(feature = "bandcamp")]` — disparaît.
//! Exiger ici un registre à six services reviendrait alors à exiger ce que le
//! binaire ne contient pas.
//!
//! C'est exactement ce qui a mis le run **33702848850** en rouge : le job
//! `Test` lançait `--no-default-features --features oaat,cloud-relay`, la
//! fonctionnalité était absente, et les cinq essais rougissaient sur un
//! registre à cinq services alors que rien n'était cassé. Le job
//! `Test (PostgreSQL)` (`--features postgres,oaat`) tombait pour la même
//! raison.
//!
//! ⚠️ Un `cfg` qui rend un test invisible est un faux vert de plus s'il reste
//! seul. Il ne reste pas seul : le job `Test` de `ci.yml` NOMME désormais
//! `bandcamp` dans son `--features`, donc ce fichier s'exécute sur chaque PR
//! Rust, et le garde `le_job_test_de_la_ci_active_bandcamp`
//! (`workflows_bornes.rs`) refuse qu'on l'en retire. Sans cette ligne de CI,
//! les cinq essais ne tourneraient que dans `test-shipped-features`, différé
//! jusqu'à `full` — donc jamais sur une PR vers `batch/*`, celle-ci comprise.
//!
//! Même idiome que `karaoke_plugin.rs`, module voisin du même agrégateur.
#![cfg(feature = "bandcamp")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn poster_texte(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn obtenir(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// 🔴 #2702 — le geste qui débloque tout : Bandcamp est dans `state.services`.
///
/// Sabotage : retirer le `services.register(BandcampService…)` de `state.rs`
/// fait tomber ce test, et avec lui les deux suivants.
#[tokio::test]
async fn bandcamp_est_inscrit_au_registre_des_services() {
    let etat = etat();
    let registre = etat.services.lock().await;
    assert!(
        registre.get("bandcamp").is_some(),
        "sans entrée « bandcamp » dans le registre, les routes de file \
         répondent 400 unknown service et la file reste à une piste (#2702) — \
         services inscrits : {:?}",
        registre.list()
    );
}

/// LE TÉMOIN : les cinq services déjà inscrits ne bougent pas.
#[tokio::test]
async fn les_services_deja_inscrits_ne_changent_pas() {
    let etat = etat();
    let registre = etat.services.lock().await;
    for nom in ["tidal", "qobuz", "spotify", "deezer", "youtube"] {
        assert!(
            registre.get(nom).is_some(),
            "{nom} doit rester inscrit — inscrits : {:?}",
            registre.list()
        );
    }
    assert_eq!(
        registre.list().len(),
        6,
        "cinq services d'origine plus Bandcamp, et rien d'autre : {:?}",
        registre.list()
    );
}

/// 🔴 #2702 — la route de file ATTEINT Bandcamp au lieu de le déclarer inconnu.
///
/// L'adresse d'album est volontairement invalide : l'épreuve ne doit dépendre
/// d'aucun accès réseau. Ce qui est mesuré est que le refus vient de
/// l'adaptateur Bandcamp — qui parle d'adresse — et non du registre, qui
/// parlait de service inconnu.
#[tokio::test]
async fn un_album_bandcamp_n_est_plus_un_service_inconnu() {
    let app = tune_server::routes::router(etat());
    let (status, corps) = poster_texte(
        &app,
        "/api/v1/zones/1/play",
        json!({ "source": "bandcamp", "streaming_album_id": "pas-une-adresse" }),
    )
    .await;
    assert!(
        !corps.contains("unknown service"),
        "la route de file ne doit plus ignorer Bandcamp (#2702) — {status} : {corps}"
    );
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "l'échec vient désormais de l'adaptateur : {corps}"
    );
    assert!(
        corps.contains("bandcamp.com"),
        "l'échec doit NOMMER ce qu'il attendait : {corps}"
    );
}

/// Bandcamp n'a pas de playlists. Le refus le DIT, au lieu de se confondre
/// avec un service absent du registre.
#[tokio::test]
async fn une_playlist_bandcamp_se_refuse_en_le_nommant() {
    let app = tune_server::routes::router(etat());
    let (status, corps) = poster_texte(
        &app,
        "/api/v1/zones/1/play",
        json!({ "source": "bandcamp", "streaming_playlist_id": "peu-importe" }),
    )
    .await;
    assert!(!corps.contains("unknown service"), "{status} : {corps}");
    assert!(
        corps.contains("playlists"),
        "le refus doit nommer ce qui manque : {status} — {corps}"
    );
}

/// 🔴 #2778 — l'état de liaison Bandcamp se LIT.
///
/// Aucune route ne le rendait : le greffon écrivait `bandcamp_username` et
/// `bandcamp_fan_id` et ne les relisait que pour `GET /collection`, qui répond
/// « aucun compte lié » sans distinguer « jamais lié » de « écriture perdue ».
/// L'inscription au registre donne `GET /streaming/bandcamp/status`.
#[tokio::test]
async fn l_etat_de_liaison_bandcamp_se_lit_par_une_route() {
    let etat = etat();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone());
    let app = tune_server::routes::router(etat);

    let (status, corps) = obtenir(&app, "/api/v1/streaming/bandcamp/status").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "la route d'état doit exister pour Bandcamp (#2778) : {corps}"
    );
    assert_eq!(corps["authenticated"], json!(false));

    reglages.set("bandcamp_username", "fabienm").unwrap();
    reglages.set("bandcamp_fan_id", "897100").unwrap();

    let (status, corps) = obtenir(&app, "/api/v1/streaming/bandcamp/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["authenticated"],
        json!(true),
        "un compte mémorisé doit se voir : {corps}"
    );
    assert_eq!(
        corps["username"],
        json!("fabienm"),
        "le pseudo lié doit être rendu — c'est l'« identifiant perdu » de \
         FabienM qui devient visible : {corps}"
    );
}

// ---------------------------------------------------------------------------
// « Ma collection » sert-elle ce que Bandcamp lui donne ? (Yves, 09/09/2026)
// ---------------------------------------------------------------------------

/// Une page de collection telle que Bandcamp la rend RÉELLEMENT.
///
/// Mesurée le 09/09/2026 sur `POST
/// https://bandcamp.com/api/fancollection/1/collection_items`, sans aucun
/// cookie de session : 200, huit clefs de premier niveau, dont un bloc
/// `tracklists` que Tune jetait, et un `redownload_urls` VIDE. Les noms et la
/// forme sont recopiés de cette réponse, pas devinés d'après le code.
///
/// Deux articles ACHETÉS — c'est le sujet : ce que Tune rend à quelqu'un qui a
/// payé. Le second n'a volontairement pas de tracklist, pour que l'absence
/// d'extrait reste un `null` franc et non une panne.
fn page_de_collection_reelle() -> Value {
    json!({
        "items": [
            {
                "band_name": "Andrew Huang",
                "item_title": "CXM 1978",
                "item_type": "album",
                "item_url": "https://andrewhuang.bandcamp.com/album/cxm-1978",
                "item_art_id": 3903246145i64,
                "tralbum_type": "a",
                "tralbum_id": 787856765i64,
                "purchased": "10 Jun 2026 13:15:40 GMT",
                "download_available": true,
                "num_streamable_tracks": 2
            },
            {
                "band_name": "Sans Tracklist",
                "item_title": "Précommande",
                "item_type": "album",
                "item_url": "https://exemple.bandcamp.com/album/precommande",
                "item_art_id": 1i64,
                "tralbum_type": "a",
                "tralbum_id": 999i64,
                "purchased": "01 Jan 2026 00:00:00 GMT",
                "download_available": false
            }
        ],
        "tracklists": {
            "a787856765": [
                {
                    "id": 3603313194i64,
                    "title": "CXM 1978",
                    "artist": "Andrew Huang",
                    "track_number": 1,
                    "duration": 138.772,
                    "file": {
                        "mp3-128": "https://bandcamp.com/stream_redirect?enc=mp3-128&track_id=3603313194"
                    }
                }
            ]
        },
        // Mesuré VIDE sans session d'achat : c'est ce champ qui porterait les
        // fichiers sans perte de l'acheteur, et Tune n'a aucune session à
        // présenter — `lier_compte` ne lit qu'une page de profil PUBLIQUE.
        "redownload_urls": {},
        "purchase_infos": {},
        "more_available": false,
        "last_token": "1781097340:787856765:a::"
    })
}

/// 🔴 « Ma collection » ANNONCE le mp3-128, comme toutes les autres surfaces.
///
/// C'est le signalement d'Yves, qui a ACHETÉ ses albums : Tune lui joue le
/// flux de découverte à 128 kbit/s. Mesuré le 09/09/2026, Bandcamp ne sert
/// rien d'autre sans session d'achat — `file` ne porte QUE la clef `mp3-128`
/// (3 pistes sur 3 dans la collection, 2 sur 2 sur la page d'album), tout
/// autre `enc=` répond 404, et le flux mesure bien 128 kbit/s / 44 100 Hz.
/// Il n'y a donc rien de mieux à jouer, et la seule réparation honnête est de
/// le DIRE là où l'acheteur regarde.
///
/// Or c'était la seule surface Bandcamp muette : `/discover` porte `qualite`
/// et `lossless` par article ET sur l'enveloppe, `/search` sur l'enveloppe,
/// `/album` sur l'album et sur chaque piste — et `/collection` sur rien. La
/// règle de #2074 (« un flux à 128 kbit/s doit être annoncé PARTOUT où il
/// apparaît ») s'arrêtait juste avant l'écran de l'acheteur.
///
/// Sabotage : retirer `"qualite": BC_STREAM_QUALITY` de l'article dans
/// `collection_mise_en_forme` fait tomber ce test.
#[test]
fn ma_collection_annonce_le_mp3_128_comme_les_autres_surfaces_bandcamp() {
    let vue = tune_bandcamp::collection_mise_en_forme(&page_de_collection_reelle(), 897100);

    assert_eq!(
        vue["qualite"],
        json!("mp3-128"),
        "l'enveloppe de « Ma collection » doit annoncer sa qualité comme \
         /search et /discover : {vue}"
    );
    assert_eq!(
        vue["lossless"],
        json!(false),
        "« Ma collection » ne sert PAS de sans perte, et doit le dire : {vue}"
    );
    let note = vue["quality_note"].as_str().unwrap_or_default();
    assert!(
        note.contains("128") && note.contains("télécharger"),
        "la note doit nommer le débit ET le seul chemin qui rend à l'acheteur \
         ce qu'il a payé — le téléchargement : {note:?}"
    );

    for (rang, article) in vue["items"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            article["qualite"],
            json!("mp3-128"),
            "article {rang} : la qualité doit être annoncée sur l'ARTICLE \
             aussi, comme /discover le fait — un écran qui n'affiche que la \
             grille doit pouvoir le dire : {article}"
        );
        assert_eq!(
            article["lossless"],
            json!(false),
            "article {rang} : {article}"
        );
    }
}

/// 🔴 Un article de « Ma collection » porte de quoi s'afficher et de quoi jouer.
///
/// Second symptôme du même écran, rapporté le même jour : « le clic sur la
/// pochette ne lance pas la lecture ». Une seule cause côté serveur — la
/// réponse de Bandcamp porte DÉJÀ la pochette et une URL de flux par article,
/// et `collection_mise_en_forme` les jetait toutes les deux. L'article arrivait
/// au client avec cinq champs de texte : rien à afficher, rien à jouer.
///
/// Le préfixe `a` de la pochette n'est pas décoratif — `.../img/3903246145_2.jpg`
/// répond 404, `.../img/a3903246145_2.jpg` répond 200, mesuré. C'est pour cela
/// que le client ne recompose AUCUNE URL bcbits lui-même et attend une adresse
/// résolue, comme sur les trois autres surfaces.
///
/// Sabotage : retirer `"extrait": extrait_de_collection(brut, it)` de
/// `collection_mise_en_forme` fait tomber ce test.
#[test]
fn un_article_de_ma_collection_porte_une_pochette_et_un_extrait_jouables() {
    let vue = tune_bandcamp::collection_mise_en_forme(&page_de_collection_reelle(), 897100);
    let articles = vue["items"].as_array().unwrap();
    assert_eq!(
        articles.len(),
        2,
        "les deux achats doivent survivre : {vue}"
    );

    let achat = &articles[0];
    assert_eq!(
        achat["pochette"],
        json!("https://f4.bcbits.com/img/a3903246145_2.jpg"),
        "la pochette doit être RÉSOLUE, préfixe `a` compris : sans elle la \
         vignette de l'acheteur reste vide sur l'écran de ses propres \
         achats : {achat}"
    );
    assert_eq!(
        achat["extrait"],
        json!("https://bandcamp.com/stream_redirect?enc=mp3-128&track_id=3603313194"),
        "l'URL de flux vient du bloc `tracklists` que Bandcamp rend déjà dans \
         la MÊME réponse — sans elle, le geste de lecture n'a rien à jouer : \
         {achat}"
    );
    assert_eq!(
        achat["source"],
        json!("bandcamp"),
        "la source nomme le service, comme partout ailleurs : {achat}"
    );

    // Un article sans tracklist rend un `null` franc, pas une panne ni une
    // URL inventée : le client saura qu'il n'y a rien à jouer.
    assert_eq!(
        articles[1]["extrait"],
        Value::Null,
        "sans tracklist, l'extrait doit être null : {}",
        articles[1]
    );

    // TÉMOIN — les cinq champs d'origine sont intacts. Le client les lit tous
    // (`BandcampItem`), et le rapprochement avec la bibliothèque locale en
    // dépend : les ajouter ne doit rien remplacer.
    assert_eq!(achat["artist"], json!("Andrew Huang"));
    assert_eq!(achat["title"], json!("CXM 1978"));
    assert_eq!(achat["type"], json!("album"));
    assert_eq!(
        achat["url"],
        json!("https://andrewhuang.bandcamp.com/album/cxm-1978")
    );
    assert_eq!(achat["art_id"], json!(3903246145i64));
    assert_eq!(vue["fan_id"], json!(897100));
    assert_eq!(vue["count"], json!(2));
    assert_eq!(vue["more_available"], json!(false));
}
