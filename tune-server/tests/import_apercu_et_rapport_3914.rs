//! L'aperçu n'écrit rien, et le suivi de tâche porte le rapport — #3914 (R4, R5).
//!
//! Deux faces du même sujet : **ce que l'utilisateur voit quand il importe**.
//!
//! **R4.** L'écran appelle `importRoon(file, true)` — donc `?preview=true`
//! (`SettingsView.svelte:2885`) — pour MONTRER ce qu'un import ferait avant de
//! le faire. Le paramètre n'était lu par personne : les points d'entrée
//! n'avaient aucun extracteur `Query`. Cliquer sur « aperçu » lançait un import
//! RÉEL, en tâche détachée. L'utilisateur croyait regarder ; Tune écrivait.
//!
//! **R5.** Et quand l'import aboutissait, l'écran affichait un rapport vide :
//! la route rend `202 {status, task_id}`, le suivi rendait
//! `{imported, skipped, errors}`, et l'écran lit une TROISIÈME forme —
//! `ImportReport` (`tune-web-client/src/lib/api.ts:4692`). Aucun de ses noms
//! n'était rendu nulle part : tout arrivait `undefined`.
//!
//! 🔴 **Le témoin décisif de R4 n'est pas le code de statut.** Une route qui
//! rendrait `200` et un joli rapport tout en lançant l'import derrière serait
//! verte sur le statut et fausse sur le fond. Ce qui tranche est
//! `SELECT COUNT(*) FROM tracks`, relevé AVANT et APRÈS l'aperçu — c'est ce que
//! `compter_les_pistes` fait, directement sur la base de l'état applicatif.
//!
//! 🔴 **Et ces témoins passent par la ROUTE.** Les cinq essais unitaires
//! d'`import.rs` appellent le gestionnaire en direct : c'est exactement ce qui
//! a laissé vivre le 415 de R3 entre le routeur et le gestionnaire, invisible.
//! Ici on monte `tune_server::routes::router(state)`, avec son préfixe
//! `/api/v1`, et on tape le chemin exact du client, chaîne de requête comprise.
//!
//! 🔴 **Rien n'est construit ici de ce qui est vérifié.** La bibliothèque de
//! départ n'est pas écrite par le témoin : elle est peuplée par un import RÉEL,
//! passé par la même route. Un témoin qui fabriquerait lui-même les pistes
//! resterait vert le jour où la production oublierait de les écrire.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

const ROON: &str = "/api/v1/system/import/roon";
const PLEX: &str = "/api/v1/system/import/plex";

/// Deux lignes, de forme Roon — mêmes colonnes que la fixture de R3.
///
/// ⚠️ La réserve de R3 tient : aucun export CSV réel de Roon n'a encore été
/// recoupé contre `ROON_*_HEADERS`. Ce que ces témoins gardent n'est pas la
/// correspondance des colonnes, c'est l'APERÇU et le RAPPORT.
const CSV_ROON: &str = "Title,Artist,Album,File Path,Play Count\r\n\
     Walking,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/01.flac,5\r\n\
     Liberte,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/02.flac,2\r\n";

/// Le même export, ÉLARGI de deux lignes que la bibliothèque ne connaît pas.
///
/// 🔴 C'est ce débordement qui donne sa force au témoin d'aperçu, et il a été
/// mesuré : avec le fichier de départ tel quel, un aperçu saboté relançait bien
/// un import réel — mais ses deux lignes étaient déjà en base, `ecrire_les_pistes`
/// les sautait toutes les deux par leur `file_path`, et `SELECT COUNT(*)` ne
/// bougeait pas d'un pouce. Le témoin restait VERT sur le comptage, c'est-à-dire
/// sur ce qu'il prétendait garder. Il faut que l'import saboté ait quelque chose
/// à écrire pour que ne rien écrire se voie.
const CSV_ROON_ELARGI: &str = "Title,Artist,Album,File Path,Play Count\r\n\
     Walking,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/01.flac,5\r\n\
     Liberte,Sokratis Sinopoulos,Eight Winds,/music/eight-winds/02.flac,2\r\n\
     Metamorphosis,Sokratis Sinopoulos,Metamodal,/music/metamodal/01.flac,1\r\n\
     Liquid,Sokratis Sinopoulos,Metamodal,/music/metamodal/02.flac,0\r\n";

const XML_PLEX: &str = r#"<MediaContainer><Track title="Time" grandparentTitle="Pink Floyd" parentTitle="The Dark Side of the Moon" viewCount="3" /></MediaContainer>"#;

/// Les huit champs que l'écran lit sur un `ImportReport` (`api.ts:4692`).
const CHAMPS_DU_RAPPORT: &[&str] = &[
    "total_rows",
    "matched",
    "unmatched",
    "play_counts_updated",
    "ratings_updated",
    "history_entries_added",
    "playlists_created",
    "details",
];

/// Le routeur RÉEL, et l'état qu'il partage — pour pouvoir compter les pistes.
fn app() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    (tune_server::routes::router(state.clone()), state)
}

/// `SELECT COUNT(*) FROM tracks`, sur la base que la route écrit vraiment.
fn compter_les_pistes(state: &AppState) -> i64 {
    TrackRepo::with_backend(state.backend.clone())
        .count()
        .expect("compter les pistes")
}

fn multipart(nom_du_fichier: &str, type_mime: &str, contenu: &str) -> (String, Vec<u8>) {
    let frontiere = "----tune3914ApercuRapport";
    let corps = format!(
        "--{frontiere}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{nom_du_fichier}\"\r\n\
         Content-Type: {type_mime}\r\n\
         \r\n\
         {contenu}\r\n\
         --{frontiere}--\r\n"
    );
    (
        format!("multipart/form-data; boundary={frontiere}"),
        corps.into_bytes(),
    )
}

async fn poster(
    app: &axum::Router,
    chemin: &str,
    type_de_contenu: &str,
    corps: Vec<u8>,
) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header(header::CONTENT_TYPE, type_de_contenu)
                .body(Body::from(corps))
                .unwrap(),
        )
        .await
        .unwrap();
    lire(reponse).await
}

async fn obtenir(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    lire(reponse).await
}

async fn lire(reponse: axum::response::Response) -> (StatusCode, Value) {
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    (
        statut,
        serde_json::from_str(&texte).unwrap_or(json!({ "_brut": texte })),
    )
}

/// Poster le CSV sur `/system/import/roon`, avec la chaîne de requête voulue.
async fn poster_un_csv(app: &axum::Router, csv: &str, suffixe: &str) -> (StatusCode, Value) {
    let (type_de_contenu, corps) = multipart("roon-export.csv", "text/csv", csv);
    poster(app, &format!("{ROON}{suffixe}"), &type_de_contenu, corps).await
}

async fn poster_le_csv(app: &axum::Router, suffixe: &str) -> (StatusCode, Value) {
    poster_un_csv(app, CSV_ROON, suffixe).await
}

/// Lancer un import RÉEL et attendre qu'il ait fini, par le suivi de tâche.
///
/// C'est la bibliothèque de départ des témoins d'aperçu : elle est peuplée par
/// la route, jamais par le témoin lui-même.
async fn importer_pour_de_vrai(app: &axum::Router) -> Value {
    let (statut, reponse) = poster_le_csv(app, "?preview=false").await;
    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "un import réel doit rendre 202 : {reponse}"
    );
    let task_id = reponse["task_id"]
        .as_str()
        .expect("un import accepté rend un task_id")
        .to_string();
    attendre_la_fin(app, &task_id).await
}

/// Interroger `/system/import/status/{id}` jusqu'à ce que la tâche ait fini.
///
/// L'import travaille dans une tâche détachée : le suivi est le SEUL signal de
/// fin observable depuis la route. Il est donc à la fois l'outil de ce fichier
/// et l'objet de la tranche R5.
async fn attendre_la_fin(app: &axum::Router, task_id: &str) -> Value {
    let chemin = format!("/api/v1/system/import/status/{task_id}");
    for _ in 0..200 {
        let (statut, suivi) = obtenir(app, &chemin).await;
        assert_eq!(statut, StatusCode::OK, "le suivi de tâche doit répondre");
        if suivi["status"] != "running" && suivi["status"] != "unknown" {
            return suivi;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("l'import n'a jamais fini");
}

// ===========================================================================
// R4 — « aperçu » ne doit RIEN écrire
// ===========================================================================

/// 🔴 LE témoin de R4 : le comptage des pistes, avant et après l'aperçu.
///
/// La bibliothèque est d'abord peuplée par un import réel — deux pistes, par la
/// route. Puis un fichier ÉLARGI (les deux mêmes lignes, plus deux inconnues)
/// repart en `?preview=true`. Si l'aperçu importe, le compte passe de 2 à 4 ;
/// s'il n'importe pas, il reste à 2. Le code de statut ne décide de rien ici.
///
/// 🔴 Le fichier élargi n'est pas un détail de confort : voir `CSV_ROON_ELARGI`.
/// Avec le fichier de départ, un aperçu saboté relançait un import réel dont
/// TOUTES les lignes étaient sautées comme déjà connues — le comptage ne bougeait
/// pas, et le témoin restait vert sur le défaut qu'il devait attraper.
#[tokio::test]
async fn l_apercu_n_ecrit_rien_dans_la_bibliotheque() {
    let (app, state) = app();
    importer_pour_de_vrai(&app).await;
    let avant = compter_les_pistes(&state);
    assert_eq!(
        avant, 2,
        "la bibliothèque de départ doit porter les 2 pistes du CSV, écrites par \
         la route elle-même — sinon ce témoin ne mesure rien"
    );

    let (statut, rapport) = poster_un_csv(&app, CSV_ROON_ELARGI, "?preview=true").await;

    // Laisser sa chance à une éventuelle tâche détachée : un aperçu qui
    // importerait en arrière-plan ne doit pas passer pour propre juste parce
    // qu'on a compté trop tôt.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let apres = compter_les_pistes(&state);

    assert_eq!(
        apres, avant,
        "`?preview=true` a ÉCRIT dans la bibliothèque : {avant} pistes avant, \
         {apres} après — les deux lignes inconnues du fichier sont entrées. \
         L'utilisateur croit regarder, Tune importe (#3914, R4). Réponse de la \
         route : {rapport}"
    );
    assert_eq!(
        statut,
        StatusCode::OK,
        "un aperçu doit rendre son rapport tout de suite, pas un 202 : {rapport}"
    );
    assert_eq!(
        rapport["total_rows"], 4,
        "l'aperçu doit porter `total_rows` — c'est le premier champ que l'écran \
         lit, et celui sur lequel il décide de continuer : {rapport}"
    );
    assert!(
        rapport["task_id"].is_null(),
        "un aperçu ne lance aucune tâche : il ne doit pas rendre de `task_id`, \
         sinon l'écran croira devoir en suivre une : {rapport}"
    );
}

/// Le jumeau Plex : même paramètre, même exigence.
#[tokio::test]
async fn l_apercu_plex_n_ecrit_rien_dans_la_bibliotheque() {
    let (app, state) = app();
    let avant = compter_les_pistes(&state);
    let (type_de_contenu, corps) = multipart("plex-export.xml", "text/xml", XML_PLEX);
    let (statut, rapport) = poster(
        &app,
        &format!("{PLEX}?preview=true"),
        &type_de_contenu,
        corps,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let apres = compter_les_pistes(&state);
    assert_eq!(
        apres, avant,
        "`?preview=true` a écrit dans la bibliothèque par la route Plex : \
         {avant} → {apres} (#3914, R4). Réponse : {rapport}"
    );
    assert_eq!(
        statut,
        StatusCode::OK,
        "l'aperçu Plex doit rendre 200 : {rapport}"
    );
    assert_eq!(
        rapport["total_rows"], 1,
        "l'aperçu Plex doit compter la piste de l'export : {rapport}"
    );
}

/// L'aperçu rend les HUIT champs que l'écran lit, pas seulement `total_rows`.
///
/// Et trois d'entre eux portent une valeur mesurable : sur les quatre lignes du
/// fichier élargi, la bibliothèque en connaît deux. Donc `total_rows` vaut 4,
/// `matched` 2 et `unmatched` 2. Un rapport calculé APRÈS l'écriture donnerait
/// `matched` = 4 ; un rapport calculé sur un index vide donnerait 0. Ni l'un ni
/// l'autre ne tombe sur 2 par hasard.
#[tokio::test]
async fn l_apercu_rend_les_champs_que_l_ecran_lit() {
    let (app, _state) = app();
    importer_pour_de_vrai(&app).await;

    let (statut, rapport) = poster_un_csv(&app, CSV_ROON_ELARGI, "?preview=true").await;
    assert_eq!(statut, StatusCode::OK, "réponse : {rapport}");

    for champ in CHAMPS_DU_RAPPORT {
        assert!(
            !rapport[champ].is_null(),
            "l'aperçu ne rend pas `{champ}` : l'écran l'affiche en `undefined` \
             (api.ts:4692). Réponse : {rapport}"
        );
    }
    assert_eq!(
        rapport["total_rows"], 4,
        "le fichier élargi porte quatre lignes : {rapport}"
    );
    assert_eq!(
        rapport["matched"], 2,
        "deux des quatre lignes sont DÉJÀ en bibliothèque : l'aperçu doit les \
         reconnaître, sinon l'écran laisse le bouton « importer » grisé \
         (`importReport.matched === 0`). Réponse : {rapport}"
    );
    assert_eq!(
        rapport["unmatched"], 2,
        "les deux autres lignes sont inconnues de la bibliothèque : {rapport}"
    );
    assert_eq!(
        rapport["details"].as_array().map(Vec::len),
        Some(4),
        "la table d'aperçu se dresse à partir de `details` : une entrée par \
         ligne du fichier (SettingsView.svelte:4358). Réponse : {rapport}"
    );
    assert_eq!(
        rapport["details"][0]["title"], "Walking",
        "chaque détail porte les noms que la table lit — `title`, et non \
         `imported_title` : {rapport}"
    );
    assert_eq!(
        rapport["details"][0]["matched"], true,
        "la première ligne est en bibliothèque : {rapport}"
    );
}

/// Non-régression : sans `?preview`, l'import écrit toujours.
///
/// Sans ce témoin, un correctif qui ferait de TOUT appel un aperçu serait vert
/// partout ailleurs — et l'import ne marcherait plus du tout.
#[tokio::test]
async fn sans_preview_l_import_ecrit_toujours() {
    let (app, state) = app();
    assert_eq!(compter_les_pistes(&state), 0);
    let (statut, reponse) = poster_le_csv(&app, "").await;
    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "sans `?preview`, la route doit toujours rendre 202 : {reponse}"
    );
    let task_id = reponse["task_id"].as_str().expect("task_id").to_string();
    attendre_la_fin(&app, &task_id).await;
    assert_eq!(
        compter_les_pistes(&state),
        2,
        "un import sans `?preview` doit ÉCRIRE les deux pistes du CSV"
    );
}

/// Un `?preview=` illisible est refusé — il ne retombe pas sur un import réel.
///
/// Le doute va vers le refus : une faute de frappe ne doit pas se transformer
/// en écriture silencieuse dans la bibliothèque.
#[tokio::test]
async fn un_preview_illisible_est_refuse_sans_rien_ecrire() {
    let (app, state) = app();
    let (statut, reponse) = poster_le_csv(&app, "?preview=peut-etre").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        compter_les_pistes(&state),
        0,
        "un `?preview=` illisible ne doit RIEN écrire : {reponse}"
    );
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "un `?preview=` illisible doit être refusé explicitement : {reponse}"
    );
    assert_eq!(
        reponse["error"], "preview_illisible",
        "le refus doit porter un nom stable : {reponse}"
    );
}

// ===========================================================================
// R5 — le suivi de tâche porte le rapport
// ===========================================================================

/// 🔴 LE témoin de R5 : les noms de champs rendus par le suivi de tâche.
///
/// L'import ne peut pas devenir synchrone — la réponse immédiate reste
/// `202 {task_id}`, et c'est le suivi qui doit porter le rapport. S'il ne le
/// porte pas, l'écran lit `undefined` sur les huit champs.
#[tokio::test]
async fn le_suivi_de_tache_rend_les_champs_que_l_ecran_lit() {
    let (app, _state) = app();
    // Un premier import peuple la bibliothèque…
    importer_pour_de_vrai(&app).await;
    // …le second rejoue le MÊME fichier : ses deux lignes sont désormais
    // connues, donc le rapport doit les compter comme reconnues.
    let suivi = importer_pour_de_vrai(&app).await;

    for champ in CHAMPS_DU_RAPPORT {
        assert!(
            !suivi[champ].is_null(),
            "`/system/import/status/{{id}}` ne rend pas `{champ}` : l'écran \
             l'affiche en `undefined` (#3914, R5). Suivi : {suivi}"
        );
    }
    assert_eq!(
        suivi["total_rows"], 2,
        "le rapport doit compter les lignes du fichier : {suivi}"
    );
    assert_eq!(
        suivi["matched"], 2,
        "les deux lignes étaient déjà en bibliothèque : le rapport se calcule \
         AVANT l'écriture, sinon ce nombre ne veut plus rien dire : {suivi}"
    );
    assert_eq!(
        suivi["unmatched"], 0,
        "aucune ligne inconnue au second passage : {suivi}"
    );
    assert_eq!(
        suivi["source"], "roon_import",
        "le rapport doit dire de quel import il parle : {suivi}"
    );
}

/// Non-régression : les quatre champs historiques du suivi restent rendus.
///
/// `imported`/`skipped` disent ce qui a été ÉCRIT ; `matched`/`unmatched`
/// disent ce qui a été RECONNU. Deux questions différentes — ajouter les
/// secondes ne doit pas faire disparaître les premières.
#[tokio::test]
async fn le_suivi_de_tache_garde_ses_champs_historiques() {
    let (app, _state) = app();
    let premier = importer_pour_de_vrai(&app).await;
    assert_eq!(
        premier["imported"], 2,
        "le premier import écrit les deux pistes : {premier}"
    );
    assert_eq!(premier["skipped"], 0, "rien à sauter : {premier}");
    assert_eq!(premier["errors"], 0, "aucune erreur attendue : {premier}");

    let second = importer_pour_de_vrai(&app).await;
    assert_eq!(
        second["imported"], 0,
        "au second passage les deux chemins sont déjà en base : {second}"
    );
    assert_eq!(
        second["skipped"], 2,
        "…et donc les deux lignes sont sautées : {second}"
    );
    assert!(
        !second["status"].is_null(),
        "le suivi doit toujours dire son état : {second}"
    );
}
