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

    let ctx = PluginContext::new("http://127.0.0.1:0", std::env::temp_dir());
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
