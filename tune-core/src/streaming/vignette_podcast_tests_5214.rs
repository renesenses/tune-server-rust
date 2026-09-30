//! #5214 — la vignette d'un podcast mise en cache, contre un faux serveur
//! d'images.
//!
//! Le faux serveur écoute sur la boucle locale ; le relais du banc résout
//! `images.podcast.test` vers lui et admet la boucle locale — et RIEN d'autre
//! parmi les adresses que la production refuse. `interne.podcast.test` résout
//! en 10.0.0.5, une adresse de réseau local.

use super::*;
use crate::library::artwork_proxy::{
    EchecTelechargement, Refus, Resolution, Resolveur, adresse_interdite,
};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Un JPEG pour le détecteur d'octets : en-tête SOI + APP0, puis du remplissage.
fn jpeg(taille: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.resize(taille.max(4), 0x42);
    v
}

struct Banc {
    port: u16,
    requetes: Arc<AtomicUsize>,
}

async fn faux_serveur() -> Banc {
    let requetes = Arc::new(AtomicUsize::new(0));
    let r = requetes.clone();
    let app = axum::Router::new()
        .route(
            "/vignette.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg(2048)) }),
        )
        .route(
            "/sans-type.jpg",
            get(|| async { axum::body::Body::from(jpeg(512)).into_response() }),
        )
        .route(
            "/page.html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<html>pas une image</html>",
                )
            }),
        )
        .route(
            "/menteuse.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], "<html>non</html>") }),
        )
        .route(
            "/enorme.jpg",
            get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg(64 * 1024)) }),
        )
        .route(
            "/redirige-meme-hote",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/vignette.jpg")]) }),
        )
        .route(
            "/redirige-lan-litteral",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://192.168.1.20/vignette.jpg")],
                )
            }),
        )
        .route("/absente.jpg", get(|| async { StatusCode::NOT_FOUND }))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    next.run(req).await
                }
            },
        ));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    Banc { port, requetes }
}

struct Table;

impl Resolveur for Table {
    fn resoudre(&self, nom: &str) -> Resolution {
        let r: Option<IpAddr> = match nom {
            "images.podcast.test" => Some([127, 0, 0, 1].into()),
            "interne.podcast.test" => Some([10, 0, 0, 5].into()),
            _ => None,
        };
        Box::pin(async move {
            r.map(|ip| vec![ip])
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "inconnu"))
        })
    }
}

fn relais_du_banc() -> Relais {
    Relais::avec(Arc::new(Table), |ip| {
        ip == IpAddr::from([127, 0, 0, 1]) || !adresse_interdite(ip)
    })
}

fn url(b: &Banc, chemin: &str) -> String {
    format!("http://images.podcast.test:{}{chemin}", b.port)
}

/// LE témoin : un hôte HORS de la liste du relais (`images.podcast.test`
/// n'y est pas) est téléchargé, rangé sous l'adresse de son URL, et retrouvé
/// par `en_cache`. Sans le correctif, rien de tout cela n'existe.
#[tokio::test]
async fn la_vignette_d_un_hote_hors_liste_est_mise_en_cache() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    let u = url(&b, "/vignette.jpg");
    assert!(
        !crate::library::artwork_proxy::hote_autorise("images.podcast.test", &[]),
        "l'hôte du banc doit être hors liste, sinon le témoin ne prouve rien"
    );
    assert_eq!(en_cache(cache.path(), &u), None);

    let a = mettre_en_cache(&relais_du_banc(), cache.path(), &u, TAILLE_MAX)
        .await
        .expect("mise en cache");
    assert_eq!(a, adresse(&u));
    let (chemin, mime) = find_cached(cache.path(), &a).expect("fichier en cache");
    assert_eq!(mime, "image/jpeg");
    assert_eq!(std::fs::read(chemin).unwrap(), jpeg(2048));
    assert_eq!(en_cache(cache.path(), &u), Some(a.clone()));

    // Déjà en cache : aucun nouveau téléchargement.
    let avant = b.requetes.load(Ordering::SeqCst);
    let encore = mettre_en_cache(&relais_du_banc(), cache.path(), &u, TAILLE_MAX)
        .await
        .unwrap();
    assert_eq!(encore, a);
    assert_eq!(b.requetes.load(Ordering::SeqCst), avant);
}

#[tokio::test]
async fn une_autre_url_donne_une_autre_adresse() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    let a1 = mettre_en_cache(
        &relais_du_banc(),
        cache.path(),
        &url(&b, "/vignette.jpg"),
        TAILLE_MAX,
    )
    .await
    .unwrap();
    let a2 = mettre_en_cache(
        &relais_du_banc(),
        cache.path(),
        &url(&b, "/sans-type.jpg"),
        TAILLE_MAX,
    )
    .await
    .expect("une réponse sans Content-Type est jugée sur ses octets");
    assert_ne!(a1, a2);
}

#[tokio::test]
async fn une_redirection_vers_le_meme_hote_est_suivie() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    let u = url(&b, "/redirige-meme-hote");
    let a = mettre_en_cache(&relais_du_banc(), cache.path(), &u, TAILLE_MAX)
        .await
        .expect("redirection suivie");
    assert_eq!(
        a,
        adresse(&u),
        "rangée sous l'URL du FLUX, pas celle d'arrivée"
    );
}

#[tokio::test]
async fn une_redirection_vers_le_reseau_local_est_refusee() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    match mettre_en_cache(
        &relais_du_banc(),
        cache.path(),
        &url(&b, "/redirige-lan-litteral"),
        TAILLE_MAX,
    )
    .await
    {
        Err(EchecVignette::Telechargement(EchecTelechargement::Refus(
            Refus::AdresseInterdite { ip, .. },
        ))) => assert_eq!(ip, IpAddr::from([192, 168, 1, 20])),
        autre => panic!("attendu un refus d'adresse, obtenu {autre:?}"),
    }
    assert!(
        std::fs::read_dir(cache.path())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0
    );
}

#[tokio::test]
async fn un_nom_qui_resout_en_reseau_local_est_refuse() {
    let cache = tempfile::tempdir().unwrap();
    match mettre_en_cache(
        &relais_du_banc(),
        cache.path(),
        "http://interne.podcast.test/vignette.jpg",
        TAILLE_MAX,
    )
    .await
    {
        Err(EchecVignette::Telechargement(EchecTelechargement::Refus(
            Refus::AdresseInterdite { ip, .. },
        ))) => assert_eq!(ip, IpAddr::from([10, 0, 0, 5])),
        autre => panic!("attendu un refus d'adresse, obtenu {autre:?}"),
    }
}

#[tokio::test]
async fn une_adresse_litterale_interne_est_refusee() {
    let cache = tempfile::tempdir().unwrap();
    for u in [
        "http://127.0.0.2/a.jpg",
        "http://169.254.169.254/a.jpg",
        "http://[::1]/a.jpg",
    ] {
        assert!(
            matches!(
                mettre_en_cache(&relais_du_banc(), cache.path(), u, TAILLE_MAX).await,
                Err(EchecVignette::Telechargement(EchecTelechargement::Refus(
                    Refus::AdresseInterdite { .. }
                )))
            ),
            "{u}"
        );
    }
}

#[tokio::test]
async fn un_type_qui_n_est_pas_une_image_est_refuse() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    assert!(matches!(
        mettre_en_cache(&relais_du_banc(), cache.path(), &url(&b, "/page.html"), TAILLE_MAX).await,
        Err(EchecVignette::Telechargement(EchecTelechargement::TypeRefuse(t))) if t == "text/html"
    ));
    // Le type annoncé ne suffit pas : les octets doivent être une image.
    assert!(matches!(
        mettre_en_cache(
            &relais_du_banc(),
            cache.path(),
            &url(&b, "/menteuse.jpg"),
            TAILLE_MAX
        )
        .await,
        Err(EchecVignette::FormatInconnu)
    ));
    assert_eq!(
        std::fs::read_dir(cache.path())
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
}

#[tokio::test]
async fn une_image_trop_lourde_est_refusee() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    assert!(matches!(
        mettre_en_cache(
            &relais_du_banc(),
            cache.path(),
            &url(&b, "/enorme.jpg"),
            16 * 1024
        )
        .await,
        Err(EchecVignette::Telechargement(
            EchecTelechargement::TropVolumineuse(_)
        ))
    ));
    assert_eq!(
        std::fs::read_dir(cache.path())
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
    // La même sous la borne de production passe.
    assert!(
        mettre_en_cache(
            &relais_du_banc(),
            cache.path(),
            &url(&b, "/enorme.jpg"),
            TAILLE_MAX
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn une_image_absente_ou_une_url_locale_ne_laisse_rien() {
    let b = faux_serveur().await;
    let cache = tempfile::tempdir().unwrap();
    assert!(matches!(
        mettre_en_cache(
            &relais_du_banc(),
            cache.path(),
            &url(&b, "/absente.jpg"),
            TAILLE_MAX
        )
        .await,
        Err(EchecVignette::Telechargement(EchecTelechargement::Amont(_)))
    ));
    assert!(matches!(
        mettre_en_cache(&relais_du_banc(), cache.path(), "0123abcd", TAILLE_MAX).await,
        Err(EchecVignette::PasDistante)
    ));
    assert_eq!(en_cache(cache.path(), "0123abcd"), None);
}
