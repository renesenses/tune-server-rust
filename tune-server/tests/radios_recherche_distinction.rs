//! `GET /radios/search` : « aucune station de ce nom » n'est plus la même
//! réponse que « la recherche a échoué » (#2119).
//!
//! Le défaut d'origine tient en une ligne, `routes/radios.rs` :
//!
//! ```ignore
//! let items = repo.search(&q.q).unwrap_or_default();
//! Json(json!(items))
//! ```
//!
//! Le `unwrap_or_default()` transformait toute erreur du dépôt en `[]` — le
//! corps EXACT que rend un catalogue qui ne connaît pas la station. Un client
//! ne pouvait donc pas écrire la bonne phrase, faute de savoir laquelle des
//! deux s'était produite.
//!
//! Ce n'est pas une inquiétude de principe : le 21/08/2026 (fil forum 1506),
//! Belkadi Yacine cherche « radio paradise », voit une liste vide et ouvre un
//! ticket « radio paradise ne fonctionne pas » ; Bilou, qui a la station dans
//! SON catalogue pour l'y avoir ajoutée, répond « fonctionne parfaitement chez
//! moi ». Deux verdicts opposés le même jour, aucun des deux faux.
//!
//! Les essais tiennent cinq propriétés, dans cet ordre :
//!
//! 1. une station présente se trouve, et le corps le DIT (pas seulement par la
//!    longueur de `items`) ;
//! 2. une station ABSENTE rend « aucun résultat », avec le geste de secours à
//!    l'écran — c'est la voie 3 de l'issue, « au minimum, le dire » ;
//! 2 bis. et la requête du ticket, elle, TROUVE désormais : le catalogue livré
//!    a cessé d'être français-seulement (migration 90, voie 1 de l'issue,
//!    tranchée le 29/08 — peupler le semis depuis notre annuaire) ;
//! 3. une recherche qui n'aboutit pas rend une PANNE, et non un catalogue
//!    vide ;
//! 4. **la contre-épreuve** : les deux issues précédentes ne partagent NI le
//!    statut HTTP, NI le code, NI le message. Sans cette assertion-là, rien ne
//!    prouve que la distinction est observable.
//!
//! **Section 7 — le titre même du ticket.** « La recherche n'interroge aucun
//! annuaire » restait vrai après la migration 90 : celle-ci a gelé un relevé du
//! 30/08 dans le semis, ce qui règle le catalogue livré du jour et non le
//! mécanisme. La recherche consulte désormais le relevé de l'annuaire conservé
//! en mémoire, et distingue « nulle part » de « pas encore chez vous ». Aucun
//! de ces essais ne touche au réseau : l'annuaire y est déposé à la main, là où
//! `refresh_radio_logos` le pose au démarrage.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn chercher(app: &axum::Router, requete: &str) -> (StatusCode, Value) {
    chercher_dans_la_langue(app, requete, "fr-FR,fr;q=0.9").await
}

async fn chercher_dans_la_langue(
    app: &axum::Router,
    requete: &str,
    accept_language: &str,
) -> (StatusCode, Value) {
    let chemin = format!("/api/v1/radios/search?q={}", urlencoding_minimal(requete));
    let reponse = app
        .clone()
        .oneshot(
            Request::get(&chemin)
                .header("Accept-Language", accept_language)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

/// Assez pour les requêtes de ces essais : seul l'espace a besoin d'être
/// encodé. Une dépendance de plus pour trois caractères ne se justifierait pas.
fn urlencoding_minimal(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20")
}

/// Une requête qui ne peut RIEN rendre, aujourd'hui ni demain.
///
/// Ces essais tenaient cette place avec « radio paradise », et la PR qui les a
/// écrits l'avait annoncé : *« si le catalogue livré gagne un jour Radio
/// Paradise, cet essai deviendra rouge : c'est voulu. Il faudra alors changer
/// la requête, pas l'assertion »*. C'est fait — le semis de l'annuaire
/// (migration 90, #2119) livre désormais deux Radio Paradise, et `q=paradise`
/// rend trois lignes sur un serveur neuf.
///
/// La propriété tenue ici n'a jamais été « Radio Paradise est absente » mais
/// « une station absente rend `aucun_resultat` ». On la tient donc avec un
/// jeton qu'aucun enrichissement futur du catalogue ne peut faire exister,
/// pour que le prochain semis ne repasse pas ces essais au rouge.
const REQUETE_SANS_REPONSE: &str = "zzz-aucune-station-de-ce-nom";

/// Le catalogue livré n'est PAS vide.
///
/// Sans ce garde-fou, « aucun résultat » serait vrai pour une raison qui n'a
/// rien à voir — un semis cassé, des migrations non jouées — et les essais
/// ci-dessous seraient verts sans rien prouver.
fn le_catalogue_est_peuple(state: &tune_server::state::AppState) {
    let stations = RadioRepo::with_backend(state.backend.clone())
        .list()
        .expect("le catalogue doit être lisible");
    assert!(
        stations.len() >= 49,
        "catalogue livré à {} stations : « aucun résultat » ne prouverait rien",
        stations.len()
    );
}

fn semer_station(state: &tune_server::state::AppState, nom: &str, url: &str) -> i64 {
    RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: nom.into(),
            url: url.into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .expect("l'insertion directe en base doit réussir")
}

/// Met le dépôt hors d'état de répondre.
///
/// C'est la seule façon d'atteindre la branche d'erreur sans mentir sur le
/// chemin : `repo.search` rend un `Err` parce que la table n'existe plus,
/// exactement comme il le ferait sur un schéma incomplet ou une base fermée.
fn casser_le_catalogue(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch("DROP TABLE radio_stations")
        .expect("le catalogue doit pouvoir être retiré pour l'essai");
}

// ---------------------------------------------------------------------------
// 1. Une station présente se trouve — et le corps le dit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_station_du_catalogue_se_trouve_et_le_statut_le_dit() {
    let (app, state) = app_et_etat();
    // Une station que le semis ne peut pas fournir : depuis la migration 90,
    // chercher « paradise » sur un serveur neuf rend les Radio Paradise du
    // catalogue livré, et le `count == 1` d'ici ne mesurerait plus rien.
    semer_station(
        &state,
        "Radio Temoin Contre-Epreuve",
        "http://exemple.invalid/temoin",
    );

    let (status, corps) = chercher(&app, "temoin").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "resultats", "corps = {corps}");
    assert_eq!(
        corps["code"], "radio_recherche_resultats",
        "corps = {corps}"
    );
    assert_eq!(corps["count"], 1, "corps = {corps}");
    assert_eq!(
        corps["items"].as_array().map(Vec::len),
        Some(1),
        "corps = {corps}"
    );
    assert_eq!(
        corps["items"][0]["name"], "Radio Temoin Contre-Epreuve",
        "corps = {corps}"
    );
    // Rien à dire quand la liste parle d'elle-même : un message ici ferait
    // écrire au client une phrase par-dessus des résultats.
    assert!(corps["message"].is_null(), "corps = {corps}");
}

// ---------------------------------------------------------------------------
// 2. La requête du ticket : « aucun résultat », dit comme tel
// ---------------------------------------------------------------------------

/// Une station absente du catalogue rend « aucun résultat », dit comme tel.
///
/// L'essai portait la requête du ticket, « radio paradise ». Le catalogue
/// livré la contient depuis la migration 90 (#2119) — voie 1 de l'issue,
/// tranchée le 29/08 : peupler le semis depuis notre annuaire. On tient donc
/// la même propriété avec [`REQUETE_SANS_REPONSE`], et la requête du ticket
/// sert désormais de contre-épreuve juste en dessous.
#[tokio::test]
async fn la_requete_du_ticket_rend_aucun_resultat_et_le_geste_de_secours() {
    let (app, state) = app_et_etat();
    le_catalogue_est_peuple(&state);

    let (status, corps) = chercher(&app, REQUETE_SANS_REPONSE).await;

    // Une recherche qui aboutit sur zéro station a RÉUSSI.
    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "aucun_resultat", "corps = {corps}");
    assert_eq!(
        corps["code"], "radio_recherche_aucun_resultat",
        "corps = {corps}"
    );
    assert_eq!(corps["count"], 0, "corps = {corps}");
    assert_eq!(corps["items"], json!([]), "corps = {corps}");

    // Voie 3 de l'issue : « au minimum, le dire ». Le message doit nommer le
    // catalogue ET le geste de secours, sinon il ne fait pas gagner la minute
    // que Yacine a perdue.
    let message = corps["message"].as_str().expect("message absent");
    assert!(message.contains("catalogue Tune"), "message = {message}");
    assert!(message.contains("adresse"), "message = {message}");

    // Et la réponse qualifie sa portée : « absente de CE catalogue » n'est pas
    // « inexistante ».
    assert_eq!(corps["portee"], "catalogue_local", "corps = {corps}");
}

/// CONTRE-ÉPREUVE de #2119 : la requête exacte du ticket rend maintenant des
/// résultats sur un serveur neuf.
///
/// Le 21/08/2026, Belkadi Yacine tape « radio paradise » sur .18 et voit une
/// liste vide (fil forum 1506). Rien dans le produit ne pouvait la lui rendre :
/// le catalogue livré tenait 24 stations, toutes françaises, alors que NOTRE
/// annuaire en servait 51 — téléchargé à chaque démarrage, et jeté sauf les
/// logos. Cet essai mesure la même requête au même endroit, par HTTP, sur une
/// base neuve.
///
/// Il est le pendant exact de l'essai ci-dessus : si le semis de l'annuaire
/// disparaissait, celui-ci deviendrait rouge et l'autre resterait vert.
#[tokio::test]
async fn la_requete_du_ticket_trouve_desormais_radio_paradise() {
    let (app, state) = app_et_etat();
    le_catalogue_est_peuple(&state);

    let (status, corps) = chercher(&app, "radio paradise").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "resultats", "corps = {corps}");
    assert!(
        corps["count"].as_u64().unwrap_or(0) >= 2,
        "le catalogue livré ne rend que {} Radio Paradise : {corps}",
        corps["count"]
    );
    // Les deux canaux que Bilou citait au fil 1506, en FLAC avec métadonnées.
    let urls: Vec<&str> = corps["items"]
        .as_array()
        .expect("items")
        .iter()
        // `stream_url` : c'est le nom SÉRIALISÉ du champ (`RadioStation`,
        // `#[serde(rename = "stream_url")]`) — lire `url` rendrait toujours
        // une liste vide, donc une preuve fabriquée.
        .filter_map(|i| i["stream_url"].as_str())
        .collect();
    assert!(
        urls.contains(&"http://stream.radioparadise.com/flacm"),
        "le mix principal manque : {urls:?}"
    );
    assert!(
        urls.contains(&"http://stream.radioparadise.com/rock-flacm"),
        "le fil rock manque : {urls:?}"
    );
}

/// Et le catalogue livré n'est plus français-seulement — le SECOND symptôme du
/// ticket, mesuré lui aussi par la recherche : `q=Royaume-Uni` ne pouvait rien
/// rendre sur un catalogue dont les 24 stations portaient toutes `France`.
#[tokio::test]
async fn le_catalogue_livre_repond_sur_un_pays_etranger() {
    let (app, state) = app_et_etat();
    le_catalogue_est_peuple(&state);

    for pays in ["Royaume-Uni", "Suisse", "Japon"] {
        let (status, corps) = chercher(&app, pays).await;
        assert_eq!(status, StatusCode::OK, "corps = {corps}");
        assert_eq!(corps["statut"], "resultats", "{pays} : corps = {corps}");
        assert!(
            corps["count"].as_u64().unwrap_or(0) >= 1,
            "{pays} : aucune station, corps = {corps}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Une recherche qui n'aboutit pas est une panne, pas un catalogue vide
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_recherche_qui_echoue_ne_se_lit_plus_comme_un_catalogue_vide() {
    let (app, state) = app_et_etat();
    casser_le_catalogue(&state);

    let (status, corps) = chercher(&app, "paradise").await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une panne doit se lire au code de retour, corps = {corps}"
    );
    assert_eq!(corps["statut"], "echec", "corps = {corps}");
    assert_eq!(corps["code"], "radio_recherche_echec", "corps = {corps}");

    // La forme ne change pas : `items` est là, vide, pour qu'un client n'ait
    // pas à deviner la structure avant de savoir ce qui s'est passé.
    assert_eq!(corps["items"], json!([]), "corps = {corps}");
    assert_eq!(corps["count"], 0, "corps = {corps}");

    // Le message dit que ce n'est PAS une station manquante — c'est toute la
    // confusion du fil 1506.
    let message = corps["message"].as_str().expect("message absent");
    assert!(
        message.contains("n'est pas une station manquante")
            || message.contains("pas une station manquante"),
        "message = {message}"
    );

    // La cause technique reste disponible pour le rapport de bogue.
    let detail = corps["detail"].as_str().expect("detail absent");
    assert!(!detail.trim().is_empty(), "detail = {detail}");
}

// ---------------------------------------------------------------------------
// 4. CONTRE-ÉPREUVE — les deux issues sont bien discernables
// ---------------------------------------------------------------------------

/// L'assertion qui donne son sens aux trois précédentes.
///
/// Chacune prise seule pourrait passer sur une implémentation qui rendrait le
/// même corps dans les deux cas. Ici, on met les deux réponses côte à côte et
/// on exige qu'elles diffèrent sur les TROIS canaux qu'un client peut lire :
/// le statut HTTP, le code stable, le message montré.
#[tokio::test]
async fn aucun_resultat_et_panne_different_sur_les_trois_canaux_lisibles() {
    let (app_sain, _etat_sain) = app_et_etat();
    let (app_casse, etat_casse) = app_et_etat();
    casser_le_catalogue(&etat_casse);

    let (statut_aucun, corps_aucun) = chercher(&app_sain, REQUETE_SANS_REPONSE).await;
    let (statut_panne, corps_panne) = chercher(&app_casse, REQUETE_SANS_REPONSE).await;

    assert_ne!(
        statut_aucun, statut_panne,
        "statut identique : {statut_aucun} pour les deux"
    );
    assert_ne!(
        corps_aucun["code"], corps_panne["code"],
        "code identique : {}",
        corps_aucun["code"]
    );
    assert_ne!(
        corps_aucun["message"], corps_panne["message"],
        "message identique : {}",
        corps_aucun["message"]
    );

    // Et la régression exacte d'avant ce correctif : les deux corps ne peuvent
    // plus être le même tableau vide.
    assert_ne!(corps_aucun, json!([]), "corps = {corps_aucun}");
    assert_ne!(corps_panne, json!([]), "corps = {corps_panne}");
    assert_ne!(corps_aucun, corps_panne);
}

// ---------------------------------------------------------------------------
// 5. Le message suit la langue de l'interface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_message_daucun_resultat_suit_la_langue_demandee() {
    let (app, _state) = app_et_etat();

    let (_, en_fr) = chercher_dans_la_langue(&app, REQUETE_SANS_REPONSE, "fr-FR,fr;q=0.9").await;
    let (_, en_en) = chercher_dans_la_langue(&app, REQUETE_SANS_REPONSE, "en-GB,en;q=0.9").await;

    let fr = en_fr["message"].as_str().expect("message fr absent");
    let en = en_en["message"].as_str().expect("message en absent");
    assert!(fr.contains("catalogue Tune"), "fr = {fr}");
    assert!(en.contains("Tune catalogue"), "en = {en}");
    assert_ne!(fr, en, "la traduction n'a pas été appliquée");

    // Le code, lui, ne bouge pas d'une langue à l'autre : c'est ce contre quoi
    // un client programme.
    assert_eq!(en_fr["code"], en_en["code"]);
}

// ---------------------------------------------------------------------------
// 6. Le catalogue livré reste le seul interrogé — et la réponse l'annonce
// ---------------------------------------------------------------------------

/// La portée est un relevé, pas une promesse.
///
/// Sur un serveur qui n'a PAS relevé l'annuaire — hors ligne au démarrage,
/// mozaiklabs.fr indisponible, ou un essai comme celui-ci qui ne touche jamais
/// au réseau — la recherche n'a bel et bien que le catalogue local, et la
/// réponse doit continuer de le dire. C'est aussi le témoin de non-régression
/// du branchement de l'annuaire : la valeur `"catalogue_local"` ne bouge pas
/// pour un serveur dans cette situation.
#[tokio::test]
async fn la_portee_est_annoncee_sur_les_trois_issues() {
    let (app_sain, etat_sain) = app_et_etat();
    semer_station(
        &etat_sain,
        "FIP Rock",
        "https://icecast.radiofrance.fr/fiprock.mp3",
    );
    let (app_casse, etat_casse) = app_et_etat();
    casser_le_catalogue(&etat_casse);

    for (nom, corps) in [
        ("resultats", chercher(&app_sain, "fip rock").await.1),
        ("aucun", chercher(&app_sain, REQUETE_SANS_REPONSE).await.1),
        ("panne", chercher(&app_casse, REQUETE_SANS_REPONSE).await.1),
    ] {
        assert_eq!(
            corps["portee"], "catalogue_local",
            "portée absente pour {nom} : {corps}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. LE TITRE DU TICKET — « la recherche n'interroge aucun annuaire »
// ---------------------------------------------------------------------------
//
// La migration 90 (PR #2878) a gelé un relevé de l'annuaire au 30/08 dans le
// semis. Elle règle le catalogue livré du jour ; elle ne règle pas le
// MÉCANISME. L'annuaire servait 46 stations le 22/08 et 51 le 30/08, et tout
// ce qui y entre après le gel n'atteint plus personne — c'est mot pour mot ce
// que Bilou a vécu : « je les avais fait ajouter » était vrai côté annuaire
// (fil 626, 14/06) et faux côté produit pendant deux mois.
//
// Ces essais ne touchent JAMAIS au réseau : l'annuaire est déposé directement
// dans `state.annuaire_radios`, exactement là où `refresh_radio_logos` le pose
// au démarrage. Le chemin de la recherche est le même, à la source du relevé
// près.

use tune_server::routes::radios::StationAnnuaire;

fn station_annuaire(nom: &str, url: &str, pays: &str, genre: &str) -> StationAnnuaire {
    StationAnnuaire {
        name: nom.into(),
        stream_url: url.into(),
        logo_url: Some("https://mozaiklabs.fr/storage/radios/logo.png".into()),
        country: Some(pays.into()),
        genre: Some(genre.into()),
        quality: Some("flac".into()),
        website_url: None,
    }
}

/// Dépose un relevé d'annuaire, comme le ferait un démarrage en ligne.
fn relever_lannuaire(state: &tune_server::state::AppState, stations: Vec<StationAnnuaire>) {
    *state
        .annuaire_radios
        .write()
        .expect("le relevé de l'annuaire doit être accessible") = stations;
}

/// Une station ajoutée à l'annuaire APRÈS le gel du semis : le cas que la
/// migration 90 ne peut structurellement pas couvrir.
fn station_posterieure_au_semis() -> StationAnnuaire {
    station_annuaire(
        "Radio Ajoutee Apres Le Gel",
        "https://exemple.invalid/apres-le-gel.flac",
        "Islande",
        "Ambient",
    )
}

/// LE défaut du titre : une station présente à l'annuaire et absente du
/// catalogue était introuvable, et rien ne la distinguait d'une station qui
/// n'existe pas.
#[tokio::test]
async fn une_station_de_lannuaire_absente_du_catalogue_est_desormais_proposee() {
    let (app, state) = app_et_etat();
    le_catalogue_est_peuple(&state);
    relever_lannuaire(&state, vec![station_posterieure_au_semis()]);

    let (status, corps) = chercher(&app, "Ajoutee Apres Le Gel").await;

    // La recherche a ABOUTI, et sur autre chose que le vide.
    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "annuaire_seul", "corps = {corps}");
    assert_eq!(
        corps["code"], "radio_recherche_annuaire_seul",
        "corps = {corps}"
    );

    // `items` garde son sens : le catalogue LOCAL. Il ne la contient pas.
    assert_eq!(corps["count"], 0, "corps = {corps}");
    assert_eq!(corps["items"], json!([]), "corps = {corps}");

    // Et la station est là, avec son adresse — l'ajout tient en un geste.
    assert_eq!(corps["annuaire_count"], 1, "corps = {corps}");
    assert_eq!(
        corps["annuaire"][0]["stream_url"], "https://exemple.invalid/apres-le-gel.flac",
        "corps = {corps}"
    );
    assert_eq!(
        corps["annuaire"][0]["name"], "Radio Ajoutee Apres Le Gel",
        "corps = {corps}"
    );

    // La portée a changé, et le dit.
    assert_eq!(
        corps["portee"], "catalogue_local_et_annuaire",
        "corps = {corps}"
    );

    // Le message n'est PLUS celui de « aucun résultat » : il ne demande pas de
    // saisir une adresse qu'on a déjà.
    let message = corps["message"].as_str().expect("message absent");
    assert!(
        !message.contains("Aucune station de ce nom"),
        "message d'absence servi alors que l'annuaire connaît la station : {message}"
    );
    assert!(message.contains("annuaire"), "message = {message}");
}

/// CONTRE-ÉPREUVE de l'essai précédent : la MÊME requête, sur un serveur qui
/// n'a pas relevé l'annuaire, rend « aucun résultat ».
///
/// Sans elle, `annuaire_seul` pourrait être rendu pour n'importe quelle raison.
#[tokio::test]
async fn sans_releve_dannuaire_la_meme_requete_rend_aucun_resultat() {
    let (app, state) = app_et_etat();
    le_catalogue_est_peuple(&state);
    // Aucun relevé : c'est l'état exact d'un serveur hors ligne, et l'état
    // d'avant ce correctif pour tout le monde.

    let (status, corps) = chercher(&app, "Ajoutee Apres Le Gel").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["statut"], "aucun_resultat", "corps = {corps}");
    assert_eq!(corps["annuaire_count"], 0, "corps = {corps}");
    assert_eq!(corps["portee"], "catalogue_local", "corps = {corps}");
}

/// Une station de l'annuaire DÉJÀ au catalogue n'est jamais proposée deux fois
/// — ni par son adresse, ni par son nom.
///
/// L'appariement porte sur les deux, comme la garde de la migration 90 : une
/// station repointée vers un relais local garde son nom, une station renommée
/// garde son adresse, et aucune des deux ne doit revenir en suggestion.
#[tokio::test]
async fn une_station_deja_au_catalogue_nest_pas_proposee_en_double() {
    let (app, state) = app_et_etat();
    semer_station(
        &state,
        "Radio Temoin Doublon",
        "https://exemple.invalid/temoin-doublon.mp3",
    );
    semer_station(
        &state,
        "Radio Temoin Repointee",
        "https://mon-relais.local/temoin.mp3",
    );

    relever_lannuaire(
        &state,
        vec![
            // Même adresse, nom différent (station renommée localement) —
            // et le nom local ne répond même pas à la requête, ce que seule
            // l'exclusion sur le CATALOGUE ENTIER permet de voir.
            station_annuaire(
                "Radio Temoin Doublon (annuaire)",
                "http://exemple.invalid/temoin-doublon.mp3/",
                "Temoinie",
                "Temoin",
            ),
            // Même nom, adresse différente (station repointée localement).
            station_annuaire(
                "Radio Temoin Repointee",
                "https://exemple.invalid/officiel.mp3",
                "Temoinie",
                "Temoin",
            ),
        ],
    );

    let (status, corps) = chercher(&app, "Temoinie").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    assert_eq!(
        corps["annuaire_count"], 0,
        "des stations déjà au catalogue ont été proposées : {corps}"
    );
    // Rien à suggérer ⇒ on ne bascule pas en `annuaire_seul` : le pays
    // « Temoinie » n'est porté par aucune station locale.
    assert_eq!(corps["statut"], "aucun_resultat", "corps = {corps}");
}

/// Une station de l'annuaire ne se glisse JAMAIS dans `items`.
///
/// C'est la garantie de compatibilité : `items` porte des `RadioStation` avec
/// un `id` en base. Un client qui y trouverait une suggestion tenterait de
/// jouer un identifiant qui n'existe pas.
#[tokio::test]
async fn les_suggestions_de_lannuaire_ne_polluent_pas_items() {
    let (app, state) = app_et_etat();
    semer_station(
        &state,
        "Radio Temoin Melange",
        "https://exemple.invalid/melange.mp3",
    );
    relever_lannuaire(
        &state,
        vec![station_annuaire(
            "Radio Temoin Melange Bis",
            "https://exemple.invalid/melange-bis.mp3",
            "France",
            "Temoin",
        )],
    );

    let (status, corps) = chercher(&app, "Temoin Melange").await;

    assert_eq!(status, StatusCode::OK, "corps = {corps}");
    // Le catalogue local a répondu : le statut reste `resultats`.
    assert_eq!(corps["statut"], "resultats", "corps = {corps}");
    assert_eq!(corps["count"], 1, "corps = {corps}");
    assert_eq!(corps["annuaire_count"], 1, "corps = {corps}");

    let items = corps["items"].as_array().expect("items");
    assert!(
        items.iter().all(|i| i["id"].is_i64()),
        "une entrée sans id en base s'est glissée dans items : {corps}"
    );
    assert!(
        items
            .iter()
            .all(|i| i["name"] != "Radio Temoin Melange Bis"),
        "une suggestion d'annuaire est passée dans items : {corps}"
    );
}

/// Une PANNE du catalogue ne fait pas proposer l'annuaire.
///
/// On ne sait alors rien du catalogue local, et surtout pas qu'il ne contient
/// pas ces stations : suggérer un ajout mènerait droit au doublon.
#[tokio::test]
async fn une_panne_du_catalogue_ne_propose_rien_de_lannuaire() {
    let (app, state) = app_et_etat();
    relever_lannuaire(&state, vec![station_posterieure_au_semis()]);
    casser_le_catalogue(&state);

    let (status, corps) = chercher(&app, "Ajoutee Apres Le Gel").await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "corps = {corps}");
    assert_eq!(corps["statut"], "echec", "corps = {corps}");
    assert_eq!(
        corps["annuaire_count"], 0,
        "l'annuaire a été proposé sur un catalogue en panne : {corps}"
    );
}

/// TÉMOIN ANTI-RÉGRESSION : les radios françaises déjà servies aujourd'hui le
/// restent, que l'annuaire ait été relevé ou non.
///
/// Le correctif ne touche ni au semis, ni à `repo.search`, ni au sens de
/// `items` — cet essai le MESURE au lieu de l'affirmer, dans les deux
/// configurations, et compare les deux `items` champ à champ.
#[tokio::test]
async fn les_radios_francaises_restent_servies_avec_ou_sans_annuaire() {
    let (app_hors_ligne, etat_hors_ligne) = app_et_etat();
    le_catalogue_est_peuple(&etat_hors_ligne);

    let (app_en_ligne, etat_en_ligne) = app_et_etat();
    le_catalogue_est_peuple(&etat_en_ligne);
    relever_lannuaire(
        &etat_en_ligne,
        vec![
            station_posterieure_au_semis(),
            // Une entrée qui DOUBLE une station livrée : elle ne doit rien
            // changer aux résultats français.
            station_annuaire(
                "FIP",
                "https://icecast.radiofrance.fr/fip-hifi.aac",
                "FR",
                "eclectic",
            ),
        ],
    );

    for requete in ["fip", "France Musique", "Radio Classique", "France"] {
        let (statut_hl, corps_hl) = chercher(&app_hors_ligne, requete).await;
        let (statut_el, corps_el) = chercher(&app_en_ligne, requete).await;

        assert_eq!(statut_hl, StatusCode::OK, "{requete} : {corps_hl}");
        assert_eq!(statut_el, StatusCode::OK, "{requete} : {corps_el}");
        assert_eq!(corps_hl["statut"], "resultats", "{requete} : {corps_hl}");
        assert_eq!(corps_el["statut"], "resultats", "{requete} : {corps_el}");
        assert!(
            corps_hl["count"].as_u64().unwrap_or(0) >= 1,
            "{requete} : plus rien hors ligne, {corps_hl}"
        );
        // L'assertion qui compte : le catalogue rendu est IDENTIQUE.
        assert_eq!(
            corps_hl["items"], corps_el["items"],
            "{requete} : le branchement de l'annuaire a changé les résultats locaux"
        );
        assert_eq!(
            corps_hl["count"], corps_el["count"],
            "{requete} : {corps_el}"
        );
    }
}

/// Le message d'« annuaire seul » suit la langue, comme les deux autres.
#[tokio::test]
async fn le_message_dannuaire_seul_suit_la_langue_demandee() {
    let (app, state) = app_et_etat();
    relever_lannuaire(&state, vec![station_posterieure_au_semis()]);

    let (_, en_fr) = chercher_dans_la_langue(&app, "Ajoutee Apres Le Gel", "fr-FR,fr;q=0.9").await;
    let (_, en_en) = chercher_dans_la_langue(&app, "Ajoutee Apres Le Gel", "en-GB,en;q=0.9").await;

    let fr = en_fr["message"].as_str().expect("message fr absent");
    let en = en_en["message"].as_str().expect("message en absent");
    assert!(fr.contains("annuaire Tune"), "fr = {fr}");
    assert!(en.contains("Tune directory"), "en = {en}");
    assert_ne!(fr, en, "la traduction n'a pas été appliquée");
    assert_eq!(en_fr["code"], en_en["code"], "le code doit être stable");
}

/// CONTRE-ÉPREUVE des trois issues, portée à quatre : « annuaire seul » ne
/// partage NI le code, NI le message, NI la portée avec « aucun résultat ».
///
/// C'est l'assertion qui donne son sens aux précédentes — sans elle, rien ne
/// prouve qu'un client puisse distinguer « nulle part » de « pas chez vous ».
#[tokio::test]
async fn annuaire_seul_et_aucun_resultat_sont_discernables() {
    let (app_avec, etat_avec) = app_et_etat();
    relever_lannuaire(&etat_avec, vec![station_posterieure_au_semis()]);
    let (app_sans, _etat_sans) = app_et_etat();

    let (statut_avec, corps_avec) = chercher(&app_avec, "Ajoutee Apres Le Gel").await;
    let (statut_sans, corps_sans) = chercher(&app_sans, "Ajoutee Apres Le Gel").await;

    // Les deux ABOUTISSENT — ce n'est pas la distinction panne/vide.
    assert_eq!(statut_avec, StatusCode::OK);
    assert_eq!(statut_sans, StatusCode::OK);

    assert_ne!(
        corps_avec["code"], corps_sans["code"],
        "code identique : {}",
        corps_avec["code"]
    );
    assert_ne!(
        corps_avec["message"], corps_sans["message"],
        "message identique : {}",
        corps_avec["message"]
    );
    assert_ne!(
        corps_avec["portee"], corps_sans["portee"],
        "portée identique : {}",
        corps_avec["portee"]
    );
    assert_ne!(
        corps_avec["annuaire_count"], corps_sans["annuaire_count"],
        "aucune station proposée d'un côté comme de l'autre : {corps_avec}"
    );
}
