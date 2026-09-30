//! #5214 — la vignette d'un podcast est mise en cache à l'abonnement et servie
//! par Tune lui-même : liste des abonnements, épisode en lecture (que Lecture
//! en cours et l'Historique du client reprennent).
//!
//! Éprouvé SUR LES ROUTES (routeur complet). Rien ne sort sur Internet : un
//! faux serveur d'images écoute sur la boucle locale, et le relais du banc
//! résout `images.podcast.test` vers lui. Cet hôte n'est PAS dans la liste du
//! relais `/library/artwork/proxy` — c'est exactement la situation d'un
//! hébergeur de flux RSS.
//!
//! Cible propre (`[[test]]` dans le manifeste) : `TUNE_ARTWORK_DIR` est une
//! variable du PROCESSUS, posée une fois pour tout ce binaire.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::library::artwork_proxy::{
    Relais, Resolution, Resolveur, adresse_interdite, hote_autorise,
};
use tune_core::streaming::vignette_podcast;
use tune_server::state::AppState;

fn jpeg(marque: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.resize(1024, marque);
    v
}

/// Fait supprimer `chemin` quand le processus se termine — même geste que
/// `plugin_contracts.rs` : un `TempDir` rangé dans un `static` n'est jamais
/// détruit, `atexit` est la seule fin de vie qui reste.
#[cfg(unix)]
fn menage_a_la_sortie_du_processus(chemin: std::path::PathBuf) {
    static CHEMIN: OnceLock<std::path::PathBuf> = OnceLock::new();

    extern "C" fn balayer() {
        if let Some(chemin) = CHEMIN.get() {
            let _ = std::fs::remove_dir_all(chemin);
        }
    }

    if CHEMIN.set(chemin).is_ok() {
        // Safety: `atexit` n'est appelé qu'une fois — `OnceLock::set` ne rend
        // `Ok` qu'au premier passage — et `balayer` ne lit que `CHEMIN`.
        unsafe {
            libc::atexit(balayer);
        }
    }
}

#[cfg(not(unix))]
fn menage_a_la_sortie_du_processus(_chemin: std::path::PathBuf) {}

// Le dossier doit survivre à tous les tests du binaire, donc à toute portée.
// tmp-autorise: repris par `menage_a_la_sortie_du_processus`, pas abandonné.
static DOSSIER: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Le dossier de cache de pochettes du binaire, posé une fois.
// tmp-autorise: rend le chemin du `static` ci-dessus, repris à la sortie.
fn dossier_cache() -> &'static std::path::Path {
    DOSSIER
        .get_or_init(|| {
            let d = tempfile::tempdir().unwrap();
            menage_a_la_sortie_du_processus(d.path().to_path_buf());
            // SAFETY : posé une seule fois, sous le `OnceLock`, avant toute
            // lecture par un test de ce binaire (chacun passe d'abord par ici).
            unsafe { std::env::set_var("TUNE_ARTWORK_DIR", d.path()) };
            d
        })
        .path()
}

async fn faux_serveur() -> u16 {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let flux = format!(
        r#"<rss><channel><title>L'After</title><itunes:image href="http://images.podcast.test:{port}/nouvelle.jpg"/><item><title>Ep 1</title><enclosure url="http://audio.test/ep1.mp3" type="audio/mpeg"/></item></channel></rss>"#
    );
    let app = Router::new()
        .route(
            "/vignette.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg(0x11)) }),
        )
        .route(
            "/nouvelle.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg(0x22)) }),
        )
        .route(
            "/ancienne.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg(0x33)) }),
        )
        .route(
            "/page.html",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<html></html>") }),
        )
        .route(
            "/flux.xml",
            get(move || {
                let flux = flux.clone();
                async move { ([(header::CONTENT_TYPE, "application/rss+xml")], flux) }
            }),
        );
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    port
}

struct Table;

impl Resolveur for Table {
    fn resoudre(&self, nom: &str) -> Resolution {
        let connu = nom.eq_ignore_ascii_case("images.podcast.test");
        Box::pin(async move {
            if connu {
                Ok(vec![IpAddr::from([127, 0, 0, 1])])
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "inconnu"))
            }
        })
    }
}

fn etat() -> AppState {
    dossier_cache();
    let mut state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.relais_pochettes = Arc::new(Relais::avec(Arc::new(Table), |ip| {
        ip == IpAddr::from([127, 0, 0, 1]) || !adresse_interdite(ip)
    }));
    state
}

fn app(state: &AppState) -> Router {
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    tune_server::routes::router(state.clone()).layer(axum::middleware::from_fn(
        move |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(ConnectInfo(peer));
            next.run(req).await
        },
    ))
}

async fn appel(
    app: &Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (statut, octets)
}

fn json_de(octets: &[u8]) -> Value {
    serde_json::from_slice(octets).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(octets)))
}

fn image_de(port: u16, chemin: &str) -> String {
    format!("http://images.podcast.test:{port}{chemin}")
}

async fn abonner(app: &Router, feed: &str, titre: &str, image: &str) -> Value {
    let (statut, corps) = appel(
        app,
        "POST",
        "/api/v1/podcasts/subscriptions",
        Some(json!({"feed_url": feed, "title": titre, "image_url": image})),
    )
    .await;
    assert!(statut.is_success(), "abonnement : {statut}");
    json_de(&corps)
}

async fn abonnements(app: &Router) -> Vec<Value> {
    let (statut, corps) = appel(app, "GET", "/api/v1/podcasts/subscriptions", None).await;
    assert_eq!(statut, StatusCode::OK);
    json_de(&corps).as_array().cloned().unwrap()
}

/// LE témoin de bout en bout : abonnement → la vignette est en cache, la liste
/// la sert en `cover_url`, la route des pochettes locales la rend, et
/// l'épisode joué la porte en `cover_path`.
#[tokio::test]
async fn l_abonnement_met_la_vignette_en_cache_et_la_lecture_la_sert() {
    let port = faux_serveur().await;
    let state = etat();
    let app = app(&state);
    let image = image_de(port, "/vignette.jpg");
    assert!(
        !hote_autorise("images.podcast.test", &[]),
        "l'hôte du banc doit rester hors de la liste du relais"
    );

    let rendu = abonner(&app, "http://flux.test/after.xml", "L'After Foot", &image).await;
    let adresse = vignette_podcast::adresse(&image);
    assert_eq!(
        rendu["cover_url"],
        json!(adresse),
        "réponse d'abonnement : {rendu}"
    );

    let liste = abonnements(&app).await;
    let ligne = liste
        .iter()
        .find(|l| l["title"] == "L'After Foot")
        .expect("abonnement listé");
    assert_eq!(ligne["cover_url"], json!(adresse));
    assert_eq!(
        ligne["image_url"],
        json!(image),
        "l'URL source reste en base"
    );

    let (statut, octets) = appel(
        &app,
        "GET",
        &format!("/api/v1/library/artwork/{adresse}"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(octets, jpeg(0x11));

    // L'épisode joué, avec l'image distante que le client envoie.
    let (statut, corps) = appel(
        &app,
        "POST",
        "/api/v1/podcasts/play/1",
        Some(json!({
            "audio_url": "http://audio.test/ep1.mp3",
            "title": "C'était pas dans le plan",
            "podcast_name": "L'After Foot",
            "cover_url": image,
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let rendu = json_de(&corps);
    assert_eq!(
        rendu["state"]["now_playing"]["cover_path"],
        json!(adresse),
        "la pochette en cours doit être la vignette en cache : {rendu}"
    );
}

/// L'épisode porte sa PROPRE image, distante et hors liste, jamais mise en
/// cache : la vignette de l'abonnement du même titre la remplace.
#[tokio::test]
async fn un_episode_a_image_propre_hors_liste_prend_la_vignette_de_l_abonnement() {
    let port = faux_serveur().await;
    let state = etat();
    let app = app(&state);
    let image = image_de(port, "/vignette.jpg");
    abonner(
        &app,
        "http://flux.test/propre.xml",
        "Podcast Propre",
        &image,
    )
    .await;
    let (_, corps) = appel(
        &app,
        "POST",
        "/api/v1/podcasts/play/2",
        Some(json!({
            "audio_url": "http://audio.test/ep2.mp3",
            "podcast_name": "Podcast Propre",
            "cover_url": "https://cdn.hebergeur.test/episode-2.jpg",
        })),
    )
    .await;
    assert_eq!(
        json_de(&corps)["state"]["now_playing"]["cover_path"],
        json!(vignette_podcast::adresse(&image))
    );
}

/// Une image que le relais admet déjà reste telle quelle (elle s'affiche, et
/// c'est peut-être l'image propre de l'épisode) ; sans abonnement, rien ne
/// change non plus.
#[tokio::test]
async fn sans_vignette_en_cache_la_lecture_garde_l_image_envoyee() {
    let state = etat();
    let app = app(&state);
    for (zone, cover) in [
        (3, "https://is1-ssl.mzstatic.com/image/thumb/a.jpg"),
        (4, "https://cdn.inconnu.test/b.jpg"),
    ] {
        let (_, corps) = appel(
            &app,
            "POST",
            &format!("/api/v1/podcasts/play/{zone}"),
            Some(json!({
                "audio_url": "http://audio.test/x.mp3",
                "podcast_name": "Jamais abonné",
                "cover_url": cover,
            })),
        )
        .await;
        assert_eq!(
            json_de(&corps)["state"]["now_playing"]["cover_path"],
            json!(cover)
        );
    }
}

/// Repli propre : un téléchargement qui échoue n'empêche pas l'abonnement, et
/// la liste retombe sur l'URL distante.
#[tokio::test]
async fn un_echec_de_telechargement_laisse_l_abonnement_et_l_url_distante() {
    let port = faux_serveur().await;
    let state = etat();
    let app = app(&state);
    let image = image_de(port, "/page.html");
    let rendu = abonner(&app, "http://flux.test/echec.xml", "Podcast Échec", &image).await;
    assert_eq!(rendu["created"], json!(true));
    assert_eq!(rendu["cover_url"], Value::Null);
    let liste = abonnements(&app).await;
    let ligne = liste
        .iter()
        .find(|l| l["title"] == "Podcast Échec")
        .unwrap();
    assert_eq!(ligne["cover_url"], Value::Null);
    assert_eq!(ligne["image_url"], json!(image));
}

/// Rattrapage du démarrage : un abonnement d'AVANT le correctif (écrit
/// directement en base, sans vignette en cache) reçoit la sienne.
#[tokio::test]
async fn le_rattrapage_met_en_cache_les_abonnements_existants() {
    let port = faux_serveur().await;
    let state = etat();
    let app = app(&state);
    let image = image_de(port, "/ancienne.jpg");
    let (feed, titre) = (
        "http://flux.test/existant.xml".to_string(),
        "Existant".to_string(),
    );
    state
        .backend
        .execute(
            "INSERT INTO podcast_subscriptions (feed_url, title, image_url) VALUES (?, ?, ?)",
            &[
                &feed as &dyn ToSqlValue,
                &titre as &dyn ToSqlValue,
                &image as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    assert_eq!(vignette_podcast::en_cache(dossier_cache(), &image), None);
    let (faites, echecs) = tune_server::routes::podcasts::rattraper_vignettes(&state).await;
    assert_eq!((faites, echecs), (1, 0));
    let liste = abonnements(&app).await;
    let ligne = liste.iter().find(|l| l["title"] == "Existant").unwrap();
    assert_eq!(ligne["cover_url"], json!(vignette_podcast::adresse(&image)));
    // Deuxième passe : rien à refaire.
    assert_eq!(
        tune_server::routes::podcasts::rattraper_vignettes(&state).await,
        (0, 0)
    );
}

/// Le flux CHANGE son image : le prochain rafraîchissement l'enregistre et la
/// met en cache.
#[tokio::test]
async fn un_flux_qui_change_d_image_est_suivi_au_rafraichissement() {
    let port = faux_serveur().await;
    let state = etat();
    let app = app(&state);
    let feed = format!("http://127.0.0.1:{port}/flux.xml");
    let ancienne = image_de(port, "/ancienne.jpg");
    let nouvelle = image_de(port, "/nouvelle.jpg");
    abonner(&app, &feed, "Flux changeant", &ancienne).await;

    let (statut, _) = appel(
        &app,
        "GET",
        &format!(
            "/api/v1/podcasts/episodes?feed_url={}",
            urlencoding::encode(&feed)
        ),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);

    // Le téléchargement part en tâche de fond : on l'attend, borné.
    let adresse = vignette_podcast::adresse(&nouvelle);
    let mut ligne = Value::Null;
    for _ in 0..100 {
        let liste = abonnements(&app).await;
        ligne = liste
            .into_iter()
            .find(|l| l["title"] == "Flux changeant")
            .unwrap();
        if ligne["cover_url"] == json!(adresse) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(ligne["image_url"], json!(nouvelle), "{ligne}");
    assert_eq!(ligne["cover_url"], json!(adresse), "{ligne}");
}
