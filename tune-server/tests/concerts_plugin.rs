//! Les concerts, de l'extérieur, maintenant qu'ils sont un plugin natif (#2363).
//!
//! Compilé seulement avec `--features concerts`. Ces tests exercent le vrai
//! câblage : l'arm de `plugins::register_builtin_plugins` construit
//! `tune_concerts::ConcertsPlugin`, `plugins::init` l'installe, et le routeur
//! qu'il contribue est monté sous `/api/v1/ext/concerts` — le préfixe vient de
//! `name()`, le plugin ne le choisit pas.
//!
//! Trois choses sont gardées ici, et chacune correspond à un piège déjà payé
//! ailleurs dans ce dépôt :
//!
//! 1. **La route de l'ancien cœur a bien disparu.** `GET /system/concerts`
//!    répondait dans tous les serveurs. Si l'extraction l'avait laissée en
//!    place, on aurait deux portes pour la même donnée — exactement le défaut
//!    corrigé côté cloud dans le même chantier.
//! 2. **Le plugin reste hors catalogue.** Aucun écran ne consomme ses routes ;
//!    l'offrir à l'installation vendrait une fonction que rien n'expose
//!    (#2090). Ce test échouera le jour où quelqu'un rebranchera
//!    `catalogued()` — et c'est le but : il faudra alors que l'écran existe.
//! 3. **Le corps d'erreur ne contient pas de phrase anglaise.** L'ancien
//!    handler rendait `{"error": "concerts: HTTP 500"}`, qu'une interface
//!    traduite en 11 langues aurait affichée telle quelle.
//!
//! # Et une quatrième, ajoutée à la fusion : l'apport de #2892
//!
//! Ce greffon a été écrit en portant `concert_alerts.rs` tel qu'il était le
//! 29/08. Le 30/08, #2892 (`40f9342c`) a réécrit ce même fichier dans la ligne
//! de release : l'abonnement porte sur TOUTE la bibliothèque, plus seulement
//! sur les artistes identifiés par un MusicBrainz ID (+275 / −38).
//!
//! La fusion rendait un `modify/delete` : prendre la suppression aurait annulé
//! #2892 en silence — le greffon compilait, la suite passait au vert, et une
//! fonction livrée avait simplement disparu. Les tests ci-dessous gardent le
//! **fait de base** (« un artiste sans MBID est abonné »), pas un code HTTP ni
//! un décompte de lignes : c'est le seul énoncé qu'une réécriture du greffon ne
//! peut pas satisfaire par accident.
#![cfg(feature = "concerts")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Construit l'app avec le plugin chargé par le vrai chemin d'enregistrement.
async fn app_avec_concerts(state: &AppState) -> axum::Router {
    use_scratch_plugin_data_dir();

    // Opt-in comme dj, karaoke et bandcamp : `default_enabled()` rend false et
    // `setup_all` le laisse dormant tant que `plugin_concerts_installed` n'est
    // pas posé.
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_concerts_installed", "true")
        .expect("marquer concerts installé");

    let routers = tune_server::plugins::init(state, "http://127.0.0.1:0", vec![]).await;

    assert!(
        routers.iter().any(|(name, _)| name == "concerts"),
        "le greffon concerts doit contribuer un routeur monté sous son name()"
    );

    tune_server::routes::router_with_plugins(state.clone(), routers)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

#[tokio::test]
async fn la_route_du_coeur_a_disparu() {
    let state = new_state();
    let app = app_avec_concerts(&state).await;

    let (statut, _) = get_json(&app, "/api/v1/system/concerts").await;
    assert_eq!(
        statut,
        StatusCode::NOT_FOUND,
        "GET /system/concerts doit avoir disparu du cœur : la lecture vit \
         désormais sous /api/v1/ext/concerts/upcoming"
    );
}

#[tokio::test]
async fn le_routeur_est_monte_sous_son_nom() {
    let state = new_state();
    let app = app_avec_concerts(&state).await;

    // Sans `instance_id`, le plugin répond sans jamais appeler le cloud : le
    // test ne dépend d'aucun réseau.
    let (statut, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["concerts"], serde_json::json!([]));
    assert_eq!(
        corps["code"], "concerts.no_instance_id",
        "un serveur sans instance_id doit le dire par un code, pas par une \
         liste vide muette"
    );
}

#[tokio::test]
async fn le_corps_ne_porte_aucune_phrase_anglaise() {
    let state = new_state();
    let app = app_avec_concerts(&state).await;

    let (_, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert!(
        corps.get("error").is_none(),
        "le corps ne doit plus porter de champ `error` : l'ancien handler y \
         mettait une chaîne technique anglaise qu'une interface traduite en 11 \
         langues affichait telle quelle. Le contrat est un `code` stable."
    );
    let code = corps["code"].as_str().unwrap_or_default();
    assert!(
        code.starts_with("concerts."),
        "le code doit être préfixé par le domaine, trouvé : {code:?}"
    );
}

#[tokio::test]
async fn le_greffon_reste_hors_catalogue_tant_qu_aucun_ecran_ne_l_appelle() {
    use tune_core::plugin_sdk::TunePlugin;

    let state = new_state();
    let greffon = tune_concerts::ConcertsPlugin::new(tune_concerts::HostServices {
        backend: state.backend.clone(),
    });

    assert!(
        !greffon.default_enabled(),
        "le greffon doit être opt-in, comme dj, karaoke et bandcamp"
    );
    assert!(
        !greffon.catalogued(),
        "le greffon ne doit PAS être offert au catalogue tant qu'aucun écran \
         ne consomme ses routes : proposer « Installer » sur une fonction que \
         rien n'expose fait redémarrer l'utilisateur pour rien (#2090). \
         Rebrancher ce test le jour où l'écran existe."
    );
}

#[tokio::test]
async fn la_tache_periodique_s_arrete_avec_le_greffon() {
    use tune_core::plugin_sdk::{PluginContext, TunePlugin};

    use_scratch_plugin_data_dir();
    let state = new_state();
    let mut greffon = tune_concerts::ConcertsPlugin::new(tune_concerts::HostServices {
        backend: state.backend.clone(),
    });

    // Le dossier de données passe par `test_scratch` et non par `temp_dir()`
    // composé à la main : le garde-fou #3030, arrivé par ce lot, refuse le
    // second — un dossier compose à la main survit au test, et surtout au test
    // qui échoue. `dossier` se supprime par `Drop` à la fin de la fonction.
    let dossier = tune_core::test_scratch::scratch_dir("concerts-teardown");
    let ctx = PluginContext::new("http://127.0.0.1:0", dossier.path().to_path_buf());
    greffon.setup(&ctx).await.expect("setup");

    // Le cœur ne gardait aucune poignée sur cette tâche : `tokio::spawn` et
    // plus rien. Un plugin qu'on arrête doit emporter sa tâche, sinon elle
    // survit à son propriétaire et continue d'appeler le cloud.
    greffon.teardown().await.expect("teardown");
}

// ---------------------------------------------------------------------------
// L'apport de #2892 (40f9342c), reporté du cœur vers le greffon.
//
// Le cœur gardait ce comportement par un `#[cfg(test)] mod tests` interne au
// fichier ; ce fichier ayant été supprimé par l'extraction, ces tests-là sont
// partis avec lui. Ils revivent ici, de l'autre côté de la frontière de crate,
// sur `tune_concerts::artistes_de_la_bibliotheque` — la charge utile exacte
// que le greffon envoie au nuage, avant tout HTTP. Aucun réseau n'est touché.
// ---------------------------------------------------------------------------

/// Une base peuplée d'artistes, dont certains **sans** MusicBrainz ID.
fn base_avec_artistes(artistes: &[(&str, Option<&str>)]) -> AppState {
    let state = new_state();
    for (nom, mbid) in artistes {
        state
            .backend
            .execute(
                "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
                &[nom, mbid],
            )
            .expect("inserer l'artiste");
    }
    state
}

fn noms(artistes: &[Value]) -> Vec<String> {
    artistes
        .iter()
        .map(|a| a["artist_name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// LE FAIT DE BASE, et le test qui rougit sur la fusion naïve.
///
/// Avec l'ancien filtre `WHERE musicbrainz_id IS NOT NULL` — celui que le
/// greffon portait avant cette fusion — Melissa Laveaux n'était jamais abonnée.
/// C'est justement une artiste que Ticketmaster reconnaît par son seul nom,
/// avec des dates à La Ferté-Bernard et Grasse (mesuré le 30/08/2026).
///
/// Ce test échoue si le filtre revient, sous quelque forme que ce soit.
#[test]
fn un_artiste_sans_mbid_est_abonne() {
    let state = base_avec_artistes(&[
        ("Melissa Laveaux", None),
        (
            "Bernard Lavilliers",
            Some("8bef9bae-a250-4c4e-8e5e-b2f81607db2a"),
        ),
    ]);

    let artistes = tune_concerts::artistes_de_la_bibliotheque(&state.backend).unwrap();

    assert_eq!(
        noms(&artistes),
        vec!["Bernard Lavilliers", "Melissa Laveaux"],
        "un artiste sans MBID doit partir comme les autres : c'est tout \
         l'apport de #2892, et le greffon ne doit pas le reperdre"
    );
    assert!(
        artistes[1]["musicbrainz_artist_id"].is_null(),
        "l'absence de MBID s'envoie comme nulle, pas comme chaine vide"
    );
}

/// TÉMOIN. Vert des deux côtés de la fusion : l'ancien greffon envoyait déjà
/// les artistes identifiés, et le nouveau les envoie toujours. Si ce test
/// bougeait en même temps que le précédent, c'est que la contre-épreuve
/// mesurerait autre chose que le filtre MBID — le harnais lui-même, par
/// exemple.
#[test]
fn temoin_le_mbid_est_conserve_quand_on_l_a() {
    let state = base_avec_artistes(&[(
        "Fatoumata Diawara",
        Some("6f5064bb-7dbb-4a44-bac5-04c467394817"),
    )]);

    let artistes = tune_concerts::artistes_de_la_bibliotheque(&state.backend).unwrap();

    assert_eq!(
        artistes[0]["musicbrainz_artist_id"], "6f5064bb-7dbb-4a44-bac5-04c467394817",
        "le MBID reste la meilleure identite disponible : il cesse d'etre \
         obligatoire, il ne disparait pas"
    );
}

/// La même personne apparaît souvent deux fois : une ligne identifiée par un
/// scan enrichi, une autre nue. Le nuage classant par nom replié, envoyer les
/// deux ne ferait que gonfler la charge utile.
#[test]
fn un_artiste_present_deux_fois_ne_part_qu_une_fois_avec_son_mbid() {
    let state = base_avec_artistes(&[
        ("Yael Naim", None),
        ("Yael Naim", Some("11111111-1111-4111-8111-111111111111")),
    ]);

    let artistes = tune_concerts::artistes_de_la_bibliotheque(&state.backend).unwrap();

    assert_eq!(artistes.len(), 1, "un seul envoi pour un seul artiste");
    assert_eq!(
        artistes[0]["musicbrainz_artist_id"], "11111111-1111-4111-8111-111111111111",
        "entre une ligne identifiee et une ligne nue, on garde l'identite"
    );
}

#[test]
fn un_nom_vide_ne_part_pas() {
    let state = base_avec_artistes(&[("", None), ("Superbus", None)]);

    assert_eq!(
        noms(&tune_concerts::artistes_de_la_bibliotheque(&state.backend).unwrap()),
        vec!["Superbus"]
    );
}

/// Le nuage refuse plus de 200 artistes par appel. L'ancienne requête coupait à
/// 200 SANS LE DIRE : sur une bibliothèque de 1 747 artistes, 1 547 d'entre eux
/// n'étaient jamais abonnés et personne ne pouvait le savoir. Le découpage est
/// ce qui rend l'apport utile — sans lui, lever le filtre MBID ne ferait que
/// remplir les 200 places disponibles autrement.
#[test]
fn au_dela_de_200_artistes_le_decoupage_les_emmene_tous() {
    let noms_generes: Vec<String> = (0..450).map(|i| format!("Artiste {i:04}")).collect();
    let refs: Vec<(&str, Option<&str>)> = noms_generes.iter().map(|n| (n.as_str(), None)).collect();

    let state = base_avec_artistes(&refs);
    let tous = tune_concerts::artistes_de_la_bibliotheque(&state.backend).unwrap();

    assert_eq!(tous.len(), 450, "aucun artiste ne doit etre perdu en amont");

    // `LOT` est lu dans le greffon, pas réécrit ici : un test qui coderait
    // `200` en dur resterait vert si le code changeait de taille de lot.
    let lots: Vec<_> = tous.chunks(tune_concerts::LOT).collect();
    assert_eq!(lots.len(), 3, "450 artistes = 3 appels de 200 au plus");
    assert_eq!(lots[0].len(), tune_concerts::LOT);
    assert_eq!(lots[2].len(), 450 - 2 * tune_concerts::LOT);

    let envoyes: usize = lots.iter().map(|l| l.len()).sum();
    assert_eq!(
        envoyes, 450,
        "la somme des lots doit rendre la bibliotheque entiere"
    );
}

// ---------------------------------------------------------------------------
// L'apport de #2178 (64e8378f), reporté du cœur vers le greffon.
//
// Le lot apprend à tout le nuage à rendre un 429 entier : motif nommé, délai
// de réessai, en-tête `Retry-After`. Il câblait ce contrat sur six modules,
// dont `cloud::concert_alerts`, et le rendait au client par
// `routes::cloud_error::reponse` depuis `GET /system/concerts`.
//
// La ligne de release a supprimé ce fichier et cette route au profit de ce
// greffon. La fusion rendait donc un `modify/delete` doublé d'un conflit de
// contenu, et le réflexe — prendre la suppression — **perdait le traitement du
// 429 pour les concerts** sans qu'aucun test ne rougisse : le greffon compile
// parfaitement en rendant 200 sur tous les refus, exactement comme avant.
//
// Le greffon ne peut pas appeler `routes::cloud_error` : il dépend de
// `tune-core`, jamais de `tune-server`. Ce qui est partagé l'est donc au bon
// niveau — le type `CloudError` et la lecture du délai, tous deux dans
// `tune-core` — et seul le rendu est refait, sur la forme du greffon : un
// **code stable** plutôt qu'un message traduit côté serveur.
//
// Ces tests portent sur le fait de base — « un 429 du nuage arrive au client
// en 429, avec son délai » — pas sur une ligne de code. Aucun réseau n'est
// touché : `reponse_de_refus` est une fonction pure du refus.
// ---------------------------------------------------------------------------

use axum::http::header;
use axum::response::Response;

/// Lit une réponse rendue par le greffon : statut, en-tête `Retry-After`, corps.
async fn lire_reponse(resp: Response) -> (StatusCode, Option<String>, Value) {
    let statut = resp.status();
    let retry = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, retry, serde_json::from_slice(&octets).unwrap())
}

fn limite(retry_after: Option<u64>) -> tune_core::cloud::refusal::CloudError {
    tune_core::cloud::refusal::CloudError::RateLimited {
        message: "concerts: HTTP 429 Too Many Requests".into(),
        retry_after,
        upstream: "Too Many Attempts.".into(),
    }
}

/// Le fait de base du portage : le 429 survit à la fusion.
///
/// L'ancienne route rendait **200** sur un refus du nuage — c'est précisément
/// ce qui empêchait l'écran de reconnaître une limite atteinte. Le statut, le
/// motif, le délai et l'en-tête doivent tous arriver.
#[tokio::test]
async fn un_429_du_nuage_garde_son_statut_son_delai_et_son_entete() {
    let (statut, retry, corps) =
        lire_reponse(tune_concerts::reponse_de_refus(&limite(Some(30)))).await;

    assert_eq!(
        statut,
        StatusCode::TOO_MANY_REQUESTS,
        "une limite atteinte doit repartir en 429 : l'ancienne route la rendait \
         en 200, et l'ecran ne pouvait alors dire que « une erreur est survenue »"
    );
    assert_eq!(
        corps["code"], "concerts.rate_limited",
        "le motif doit etre nomme par un code stable — la forme du greffon, \
         qui ne traduit pas cote serveur"
    );
    assert_eq!(
        corps["retry_after"], 30,
        "le delai annonce par le distant doit arriver au client"
    );
    assert_eq!(
        retry.as_deref(),
        Some("30"),
        "l'en-tete Retry-After doit etre reemis, forme standard pour qui programme"
    );
    assert_eq!(
        corps["upstream_message"], "Too Many Attempts.",
        "le texte du distant est conserve pour le diagnostic"
    );
    assert_eq!(
        corps["concerts"],
        serde_json::json!([]),
        "l'enveloppe est conservee, pour l'ecran qui rend la liste avant de \
         regarder l'erreur"
    );
    assert!(
        corps.get("error").is_none(),
        "le greffon ne rend pas de champ `error` : son contrat est un `code`"
    );
}

/// Le délai n'est **jamais fabriqué**. Quand le distant ne l'annonce pas, le
/// motif arrive quand même, mais sans chiffre inventé.
#[tokio::test]
async fn un_429_sans_entete_ne_fabrique_aucun_delai() {
    let (statut, retry, corps) = lire_reponse(tune_concerts::reponse_de_refus(&limite(None))).await;

    assert_eq!(statut, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(corps["code"], "concerts.rate_limited");
    assert!(
        corps.get("retry_after").is_none(),
        "aucun delai ne doit etre invente quand le distant n'en annonce pas"
    );
    assert!(
        retry.is_none(),
        "pas d'en-tete Retry-After sans delai connu"
    );
}

/// **Témoin, vert des deux côtés de la contre-épreuve.**
///
/// Hors 429, rien ne bouge : statut 200 et `concerts.unavailable`, exactement
/// comme avant le portage. Ce témoin borne la contre-épreuve au **traitement du
/// 429** et non au harnais de test : si les deux tests ci-dessus rougissaient
/// parce que `reponse_de_refus` ne rendait plus rien du tout, celui-ci
/// rougirait aussi.
#[tokio::test]
async fn temoin_un_refus_ordinaire_repart_comme_avant() {
    let refus = tune_core::cloud::refusal::CloudError::Message("concerts: HTTP 500".into());
    let (statut, retry, corps) = lire_reponse(tune_concerts::reponse_de_refus(&refus)).await;

    assert_eq!(
        statut,
        StatusCode::OK,
        "hors 429, le statut de la route ne bouge pas"
    );
    assert_eq!(corps["code"], "concerts.unavailable");
    assert!(retry.is_none());
    assert_eq!(corps["concerts"], serde_json::json!([]));
    assert!(
        corps.get("error").is_none(),
        "la phrase technique anglaise ne doit pas reapparaitre dans le corps"
    );
}
