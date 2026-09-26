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
//! 2. **Le plugin est au catalogue, en module Premium.** L'écran existe
//!    (tune-web-client#695) et le nuage a des dates : il est proposé à
//!    l'installation, avec le cadenas avant le clic (`premium`,
//!    `required_feature`), et il refuse LUI-MÊME ses routes à un compte gratuit
//!    — 402, forme de `ModuleRefusal`. Voir la section « Premium » plus bas.
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

/// Construit l'app avec le plugin chargé, sur un serveur PREMIUM.
///
/// Sur un compte gratuit, les routes du greffon répondent 402 avant tout
/// traitement. Les tests qui portent sur le contrat d'erreur du greffon partent
/// donc d'un serveur qui a le droit de s'en servir ; le cas gratuit a ses
/// propres tests.
async fn app_avec_concerts_premium(state: &AppState) -> axum::Router {
    state.license.set_account_premium(true, None).await;
    app_avec_concerts(state).await
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
    let app = app_avec_concerts_premium(&state).await;

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
    let app = app_avec_concerts_premium(&state).await;

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
async fn le_greffon_est_au_catalogue_et_declare_son_module() {
    use tune_core::license::Feature;
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
        greffon.catalogued(),
        "l'écran existe (tune-web-client#695) et le nuage a des dates : le \
         greffon doit être PROPOSÉ à l'installation"
    );
    assert_eq!(
        greffon.required_feature(),
        Some(Feature::Concerts),
        "le payant est une propriété du greffon : il nomme son module"
    );
}

/// ⭐ Ce que l'écran Extensions reçoit réellement : le greffon dormant (pas
/// encore installé) figure dans `/api/v1/plugins`, installable, avec le
/// cadenas AVANT le clic. `stores/concerts.ts` (tune-web-client) lit
/// `required_feature` ; sans lui, l'utilisateur installe, redémarre, et
/// n'obtient qu'un 402.
#[tokio::test]
async fn l_ecran_extensions_propose_le_greffon_avec_son_cadenas() {
    use_scratch_plugin_data_dir();
    let state = new_state();
    // Surtout PAS `plugin_concerts_installed` : c'est le cas d'un compte qui
    // découvre la fonction.
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;

    let app = tune_server::routes::router(state.clone());
    let (statut, liste) = get_json(&app, "/api/v1/plugins").await;
    assert_eq!(statut, StatusCode::OK);
    let fiche = liste
        .as_array()
        .expect("une liste")
        .iter()
        .find(|p| p["name"] == "concerts")
        .unwrap_or_else(|| panic!("concerts doit figurer au catalogue, dormant : {liste:#}"))
        .clone();

    assert_eq!(
        fiche["installed"], false,
        "pas installé → bouton « Installer »"
    );
    assert_eq!(fiche["compatible"], true);
    assert_eq!(fiche["premium"], true, "cadenas avant le clic");
    assert_eq!(fiche["required_feature"], "Concerts");
    assert_eq!(fiche["url"], "/api/v1/ext/concerts");

    // Contre-épreuve : un greffon libre du même binaire ne porte ni cadenas
    // ni module — le champ vient bien de la déclaration du greffon, pas d'une
    // valeur posée sur toutes les fiches.
    if let Some(libre) = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "bandcamp")
    {
        assert_eq!(libre["premium"], false, "{libre:#}");
        assert!(libre["required_feature"].is_null(), "{libre:#}");
    }
}

// ---------------------------------------------------------------------------
// Premium — le refus appartient au greffon (#2363, décision du 25/09/2026)
//
// Un compte gratuit peut INSTALLER le greffon (le refus porte sur les routes,
// jamais sur le chargement) ; chaque route refuse alors par un 402 dans la
// forme de `ModuleRefusal` (`error: "module_required"`), l'idiome des refus
// de module, que le client web reconnaît comme un refus d'offre et non comme
// une panne.
// ---------------------------------------------------------------------------

async fn requete(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if corps.is_some() {
        req = req.header("content-type", "application/json");
    }
    let reponse = app
        .clone()
        .oneshot(
            req.body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
                .unwrap(),
        )
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

/// ⭐ Compte gratuit : TOUTES les routes du greffon refusent, et par le même
/// corps. Une route ajoutée sans son portillon est l'erreur classique — la
/// lecture refuse, l'écriture passe, et un compte gratuit pose sa commune sur
/// un service qu'il n'a pas.
#[tokio::test]
async fn un_compte_gratuit_recoit_un_refus_d_offre_sur_toutes_les_routes() {
    let state = new_state();
    // Même un serveur qui a son identité : le refus passe AVANT tout.
    SettingsRepo::with_backend(state.backend.clone())
        .set("instance_id", "3f1c7a52-5e0b-4f5e-9d7c-2b8f3f6f9a10")
        .unwrap();
    let app = app_avec_concerts(&state).await;
    assert!(!state.license.is_premium().await, "compte neuf = gratuit");

    for (methode, chemin, corps) in [
        ("GET", "/api/v1/ext/concerts/upcoming", None),
        ("GET", "/api/v1/ext/concerts/location", None),
        (
            "POST",
            "/api/v1/ext/concerts/location",
            Some(r#"{"city":"Dijon","country":"FR","scope":"country"}"#),
        ),
        // Un corps illisible passe d'abord le portillon.
        ("POST", "/api/v1/ext/concerts/location", Some("pas du json")),
    ] {
        let (statut, corps) = requete(&app, methode, chemin, corps).await;
        assert_eq!(
            statut,
            StatusCode::PAYMENT_REQUIRED,
            "{methode} {chemin} : un compte gratuit doit recevoir 402, pas une \
             liste vide qui se lirait « il n'y a aucun concert » ({corps})"
        );
        assert_eq!(corps["error"], "module_required", "{methode} {chemin}");
        assert_eq!(
            corps["code"], "module_account_not_linked",
            "aucun compte lié : le code nomme la raison, que le client traduit"
        );
        assert_eq!(corps["action"], "link_account");
        assert_eq!(corps["module"], "concerts");
        assert_eq!(corps["feature"], "concerts");
        assert!(
            corps.get("concerts").is_none(),
            "un refus d'offre ne se déguise pas en liste vide"
        );
    }

    // Compte lié, mais sans Premium : l'autre raison, l'autre geste. Annoncer
    // « liez votre compte » à qui l'a déjà lié le renverrait tourner en rond
    // (#2392).
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_access_token", "jeton-de-test")
        .unwrap();
    let (statut, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert_eq!(statut, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(corps["error"], "module_required");
    assert_eq!(corps["code"], "module_not_owned");
    assert_eq!(corps["action"], "purchase_module");
}

/// Contre-épreuve du précédent : le MÊME serveur, passé Premium, sert.
/// Si le portillon refusait tout le monde, le test gratuit resterait vert.
#[tokio::test]
async fn contre_epreuve_le_meme_serveur_premium_est_servi() {
    let state = new_state();
    let app = app_avec_concerts_premium(&state).await;

    let (statut, corps) = get_json(&app, "/api/v1/ext/concerts/location").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["code"], "concerts.no_location");
    assert_eq!(
        corps["scope"], "world",
        "rien d'enregistré : le nuage ne filtre pas, et on le dit"
    );

    // Un Premium qui s'en va referme le module, sans redémarrage : la licence
    // est relue à chaque requête.
    state.license.set_account_premium(false, None).await;
    let (statut, _) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert_eq!(statut, StatusCode::PAYMENT_REQUIRED);
}

/// `acces()`, de l'autre côté de la frontière de crate, sur la licence que
/// l'hôte construit vraiment (`AppState::license`).
#[tokio::test]
async fn acces_suit_la_licence_de_l_hote() {
    let state = new_state();
    assert_eq!(
        tune_concerts::acces(Some(state.license.as_ref())).await,
        tune_concerts::Acces::Refuse
    );
    assert_eq!(
        tune_concerts::acces(None).await,
        tune_concerts::Acces::Refuse,
        "une licence absente ne vaut pas une autorisation"
    );
    state.license.set_account_premium(true, None).await;
    assert_eq!(
        tune_concerts::acces(Some(state.license.as_ref())).await,
        tune_concerts::Acces::Complet
    );
}

// ---------------------------------------------------------------------------
// La localisation et le périmètre, de bout en bout, contre un nuage simulé.
//
// Le routeur est celui du greffon (`router_vers`), pointé sur un banc local :
// c'est le même code que `router` en production, seule la racine change. Le
// banc répond comme `site-mozaiklabs` (`routes/api.php`, #186) et garde ce
// qu'il reçoit.
// ---------------------------------------------------------------------------

const INSTANCE: &str = "3f1c7a52-5e0b-4f5e-9d7c-2b8f3f6f9a10";

struct NuageSimule {
    racine: String,
    recues: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for NuageSimule {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

/// Un nuage minimal : `POST /location` rend la localisation retenue,
/// `GET /upcoming` rend une date et le périmètre. Lit la requête ENTIÈRE avant
/// de répondre (un banc qui répond sans lire fait émettre un RST, #1358).
async fn nuage_simule() -> NuageSimule {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let racine = format!("http://{}", ecoute.local_addr().unwrap());
    let recues = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let journal = recues.clone();

    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let mut brut = Vec::new();
            let mut tampon = [0u8; 4096];
            let (ligne, corps) = loop {
                let Ok(lu) = flux.read(&mut tampon).await else {
                    break (String::new(), Value::Null);
                };
                if lu == 0 {
                    break (String::new(), Value::Null);
                }
                brut.extend_from_slice(&tampon[..lu]);
                let Some(fin) = brut.windows(4).position(|f| f == b"\r\n\r\n") else {
                    continue;
                };
                let tete = String::from_utf8_lossy(&brut[..fin]).to_string();
                let taille = tete
                    .to_lowercase()
                    .split("content-length:")
                    .nth(1)
                    .and_then(|s| s.split("\r\n").next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if brut.len() >= fin + 4 + taille {
                    let ligne = tete.lines().next().unwrap_or_default().to_string();
                    let corps = serde_json::from_slice(&brut[fin + 4..fin + 4 + taille])
                        .unwrap_or(Value::Null);
                    break (ligne, corps);
                }
            };
            if ligne.is_empty() {
                continue;
            }
            journal.lock().unwrap().push((ligne.clone(), corps.clone()));

            let reponse = if ligne.starts_with("POST /location ") {
                serde_json::json!({
                    "scope": corps["scope"],
                    "city": corps["city"],
                    "country": corps["country"],
                    "radius_km": corps["radius_km"],
                    "located": true,
                })
            } else {
                serde_json::json!({
                    "concerts": [{
                        "artist_name": "Superbus", "event_date": "2026-11-02",
                        "venue": "La Vapeur", "city": "Dijon", "country": "FR",
                    }],
                    "scope": "radius", "radius_km": 50,
                    "city": "Dijon", "country": "FR",
                })
            }
            .to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reponse}",
                reponse.len()
            );
            let _ = flux.write_all(http.as_bytes()).await;
            let _ = flux.flush().await;
            let _ = flux.shutdown().await;
        }
    });

    NuageSimule {
        racine,
        recues,
        tache,
    }
}

/// ⭐ L'aller-retour complet : l'écran pose sa commune et son cran, le
/// greffon les relaie au nuage SOUS L'IDENTITÉ DU SERVEUR, rend ce que le
/// nuage a retenu, le garde pour `GET /location` ; puis `upcoming` rend le
/// périmètre appliqué avec les dates.
#[tokio::test]
async fn la_localisation_fait_l_aller_retour_et_upcoming_rend_le_perimetre() {
    let state = new_state();
    state.license.set_account_premium(true, None).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set("instance_id", INSTANCE)
        .unwrap();
    let nuage = nuage_simule().await;
    let app = tune_concerts::router_vers(
        &nuage.racine,
        state.backend.clone(),
        Some(state.license.clone()),
    );

    // Le corps EXACT de `setLocalisationConcerts` (tune-web-client), avec une
    // identité que le client prétendrait — elle ne doit jamais partir.
    let (statut, rendu) = requete(
        &app,
        "POST",
        "/location",
        Some(
            r#"{"city":"Dijon","postal_code":"21000","country":"fr","scope":"radius",
               "radius_km":50,"instance_id":"00000000-0000-4000-8000-000000000000"}"#,
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rendu}");
    // Les champs obligatoires du contrat `LocalisationConcerts`
    // (docs/contrat-web.json) : scope, city, country, radius_km.
    assert_eq!(rendu["scope"], "radius");
    assert_eq!(rendu["city"], "Dijon");
    assert_eq!(rendu["country"], "FR", "le pays part en majuscules");
    assert_eq!(rendu["radius_km"], 50);
    assert_eq!(rendu["located"], true);

    {
        let recues = nuage.recues.lock().unwrap();
        let (ligne, corps) = &recues[0];
        assert!(ligne.starts_with("POST /location "), "{ligne}");
        assert_eq!(
            corps["instance_id"], INSTANCE,
            "l'identité est celle du serveur, jamais celle du client"
        );
        assert_eq!(corps["postal_code"], "21000");
        assert!(
            corps.get("latitude").is_none() && corps.get("longitude").is_none(),
            "aucune position déduite ne part : la commune est SAISIE"
        );
    }

    // Gardée pour la lecture — le nuage n'a pas de route de lecture.
    let (statut, lue) = requete(&app, "GET", "/location", None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(lue["scope"], "radius");
    assert_eq!(lue["radius_km"], 50);
    assert_eq!(lue["postal_code"], "21000");
    assert!(lue.get("code").is_none(), "{lue}");

    let (statut, a_venir) = requete(&app, "GET", "/upcoming", None).await;
    assert_eq!(statut, StatusCode::OK, "{a_venir}");
    assert_eq!(a_venir["concerts"][0]["artist_name"], "Superbus");
    assert_eq!(a_venir["scope"], "radius");
    assert_eq!(a_venir["radius_km"], 50);
    assert_eq!(a_venir["city"], "Dijon");
    assert_eq!(a_venir["country"], "FR");

    let recues = nuage.recues.lock().unwrap();
    let (ligne, _) = &recues[1];
    assert!(
        ligne.starts_with("GET /upcoming?") && ligne.contains(INSTANCE),
        "la lecture porte l'identité de l'instance : {ligne}"
    );
}

/// Une demande hors des règles du nuage est refusée ICI, en 422 et en nommant
/// le champ — et le nuage n'est pas appelé. Contre-épreuve de l'aller-retour :
/// la même route, un rayon hors liste.
#[tokio::test]
async fn contre_epreuve_une_demande_invalide_ne_part_pas_au_nuage() {
    let state = new_state();
    state.license.set_account_premium(true, None).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set("instance_id", INSTANCE)
        .unwrap();
    let nuage = nuage_simule().await;
    let app = tune_concerts::router_vers(
        &nuage.racine,
        state.backend.clone(),
        Some(state.license.clone()),
    );

    let (statut, corps) = requete(
        &app,
        "POST",
        "/location",
        Some(r#"{"city":"Dijon","country":"FR","scope":"radius","radius_km":75}"#),
    )
    .await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(corps["code"], "concerts.invalid_location");
    assert_eq!(corps["field"], "radius_km");
    assert!(
        nuage.recues.lock().unwrap().is_empty(),
        "rien ne part au nuage"
    );

    let (_, lue) = requete(&app, "GET", "/location", None).await;
    assert_eq!(
        lue["code"], "concerts.no_location",
        "un refus n'enregistre rien"
    );
}

/// Sans identité d'instance, la localisation ne peut pas être posée : un
/// statut d'erreur, pas un 200 — l'écran ne regarde que les exceptions sur
/// cette route.
#[tokio::test]
async fn sans_identite_la_localisation_est_un_echec_visible() {
    let state = new_state();
    state.license.set_account_premium(true, None).await;
    let app = tune_concerts::router_vers(
        "http://127.0.0.1:9",
        state.backend.clone(),
        Some(state.license.clone()),
    );

    let (statut, corps) = requete(
        &app,
        "POST",
        "/location",
        Some(r#"{"city":"Dijon","country":"FR"}"#),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT);
    assert_eq!(corps["code"], "concerts.no_instance_id");
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
