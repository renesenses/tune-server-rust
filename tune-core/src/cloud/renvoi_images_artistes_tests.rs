//! Renvoi unique des images d'artistes perdues (rc12), contre un faux site.

use super::*;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::Artist;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

const MBID_A: &str = "11111111-1111-4111-8111-111111111111";
const MBID_B: &str = "22222222-2222-4222-8222-222222222222";
const MBID_C: &str = "33333333-3333-4333-8333-333333333333";
const INSTANCE: &str = "c3c3c3c3-0000-4000-8000-000000000001";

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn image(graine: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend((0..2048u32).map(|i| (i as u8).wrapping_mul(graine)));
    v
}

/// Un artiste local, avec son image en cache si `graine` est donnée.
fn artiste(
    db: &Arc<dyn DbBackend>,
    cache: &std::path::Path,
    nom: &str,
    mbid: Option<&str>,
    graine: Option<u8>,
    source: &str,
) {
    let repo = ArtistRepo::with_backend(db.clone());
    let mut a = Artist::new(nom.into());
    a.musicbrainz_id = mbid.map(String::from);
    let id = repo.create(&a).unwrap();
    let adresse = match graine {
        Some(g) => crate::library::artwork::cache_fetched_image(&image(g), cache, "jpg").unwrap(),
        // Une image dont le fichier a disparu du cache.
        None => "ab".repeat(32),
    };
    repo.update_image(id, &adresse, source).unwrap();
}

/// Ce que le faux site a reçu.
#[derive(Default)]
struct Recu {
    questions: Vec<Vec<String>>,
    envois: Vec<String>,
}

struct FauxSite {
    base: String,
    recu: Arc<Mutex<Recu>>,
}

/// Le faux site : `manquants` dit quels MBID il n'a pas ; les
/// `refus_envoi` premiers envois répondent `statut_refus` ; la route
/// `missing` répond `statut_missing` si donné.
async fn faux_site(
    manquants: &'static [&'static str],
    refus_envoi: u32,
    statut_refus: u16,
    statut_missing: Option<u16>,
) -> FauxSite {
    let recu = Arc::new(Mutex::new(Recu::default()));
    let compteur = Arc::new(AtomicU32::new(0));
    let r1 = recu.clone();
    let r2 = recu.clone();
    let app = axum::Router::new()
        .route(
            "/api/v1/community/artist-images/missing",
            axum::routing::post(move |axum::Json(corps): axum::Json<serde_json::Value>| {
                let r = r1.clone();
                async move {
                    if let Some(s) = statut_missing {
                        return (
                            axum::http::StatusCode::from_u16(s).unwrap(),
                            axum::Json(serde_json::json!({})),
                        );
                    }
                    assert_eq!(corps["instance_id"], INSTANCE);
                    let mbids: Vec<String> = corps["mbids"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect();
                    let manque: Vec<String> = mbids
                        .iter()
                        .filter(|m| manquants.contains(&m.as_str()))
                        .cloned()
                        .collect();
                    r.lock().unwrap().questions.push(mbids);
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({ "missing": manque })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/community/artist-images",
            axum::routing::post(move |corps: axum::body::Bytes| {
                let r = r2.clone();
                let compteur = compteur.clone();
                async move {
                    if compteur.fetch_add(1, Ordering::SeqCst) < refus_envoi {
                        return axum::http::StatusCode::from_u16(statut_refus).unwrap();
                    }
                    r.lock()
                        .unwrap()
                        .envois
                        .push(String::from_utf8_lossy(&corps).into_owned());
                    axum::http::StatusCode::CREATED
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FauxSite {
        base: format!("http://127.0.0.1:{port}"),
        recu,
    }
}

fn reglages(base: &str, cache: &std::path::Path) -> Reglages {
    Reglages {
        base_url: base.to_string(),
        instance_id: INSTANCE.to_string(),
        version: "1.0.0-rc12".to_string(),
        cache_dir: cache.to_path_buf(),
        cadence: Duration::ZERO,
        attente_transitoire: Duration::from_millis(10),
        essais_max: 3,
    }
}

fn champ(corps: &str, nom: &str) -> Option<String> {
    let debut = corps.find(&format!("name=\"{nom}\"\r\n\r\n"))? + nom.len() + 10;
    let fin = corps[debut..].find("\r\n")?;
    Some(corps[debut..debut + fin].to_string())
}

#[test]
fn les_candidats_sont_les_images_externes_en_cache_avec_mbid() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-candidats");
    artiste(&db, &cache, "Auto", Some(MBID_A), Some(1), "auto");
    // Même MBID, deuxième fiche : un seul envoi.
    artiste(&db, &cache, "Auto bis", Some(MBID_A), Some(2), "auto");
    artiste(
        &db,
        &cache,
        "Discogs",
        Some(&MBID_B.to_uppercase()),
        Some(3),
        "discogs",
    );
    artiste(
        &db,
        &cache,
        "Communauté",
        Some(MBID_C),
        Some(4),
        "community",
    );
    artiste(&db, &cache, "Roon", Some(MBID_C), Some(5), "roon");
    artiste(&db, &cache, "Sans MBID", None, Some(6), "auto");
    artiste(
        &db,
        &cache,
        "MBID abîmé",
        Some("pas-un-mbid"),
        Some(7),
        "auto",
    );
    artiste(&db, &cache, "Fichier perdu", Some(MBID_C), None, "lastfm");

    let c = candidats(&db, &cache);
    let vus: Vec<(&str, &str, &str)> = c
        .iter()
        .map(|c| (c.nom.as_str(), c.mbid.as_str(), c.source.as_str()))
        .collect();
    assert_eq!(
        vus,
        vec![("Auto", MBID_A, "auto"), ("Discogs", MBID_B, "discogs")],
        "seules les images externes en cache, avec un MBID valide, une fois par MBID"
    );
}

#[tokio::test]
async fn ne_renvoie_que_ce_que_le_site_n_a_plus_puis_pose_le_marqueur() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-manquantes");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    artiste(&db, &cache, "Björk", Some(MBID_B), Some(2), "discogs");
    let site = faux_site(&[MBID_B], 0, 503, None).await;
    let r = reglages(&site.base, &cache);

    let mut frontieres = 0u32;
    let mut dernier = (0, 0);
    let bilan = executer(
        db.clone(),
        &r,
        || true,
        || {
            frontieres += 1;
            async {}
        },
        |n, t| dernier = (n, t),
    )
    .await;

    let recu = site.recu.lock().unwrap();
    assert_eq!(
        recu.questions,
        vec![vec![MBID_A.to_string(), MBID_B.to_string()]]
    );
    assert_eq!(recu.envois.len(), 1, "seule l'image absente du site repart");
    assert_eq!(champ(&recu.envois[0], "mbid").as_deref(), Some(MBID_B));
    assert_eq!(champ(&recu.envois[0], "source").as_deref(), Some("discogs"));
    assert_eq!(
        champ(&recu.envois[0], "artist_name").as_deref(),
        Some("Björk")
    );
    assert!(recu.envois[0].len() > 2048, "les octets de l'image partent");
    assert_eq!(
        frontieres, 1,
        "chaque envoi attend son tour (pause, lecture)"
    );
    assert_eq!(dernier, (2, 2));
    assert_eq!(bilan.renvoyees, 1);
    assert!(bilan.terminee);
    assert!(deja_fait(
        &SettingsRepo::with_backend(db.clone()),
        "1.0.0-rc12"
    ));
    assert!(!deja_fait(&SettingsRepo::with_backend(db), "1.0.0-rc13"));
}

#[tokio::test]
async fn sans_consentement_rien_ne_part_et_rien_n_est_marque() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-consentement");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    let site = faux_site(&[MBID_A], 0, 503, None).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || false,
        || async {},
        |_, _| {},
    )
    .await;

    let recu = site.recu.lock().unwrap();
    assert!(recu.questions.is_empty() && recu.envois.is_empty());
    assert!(!bilan.terminee);
    assert!(!deja_fait(&SettingsRepo::with_backend(db), "1.0.0-rc12"));
}

#[tokio::test]
async fn un_503_du_site_se_retente_plus_tard_sans_perdre_l_image() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-503");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    // Deux 503 (stockage indisponible), puis le site accepte.
    let site = faux_site(&[MBID_A], 2, 503, None).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || true,
        || async {},
        |_, _| {},
    )
    .await;

    assert_eq!(site.recu.lock().unwrap().envois.len(), 1);
    assert_eq!(bilan.renvoyees, 1);
    assert!(bilan.terminee);
}

#[tokio::test]
async fn un_site_qui_reste_en_503_remet_la_passe_au_prochain_demarrage() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-503-durable");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    let site = faux_site(&[MBID_A], u32::MAX, 503, None).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || true,
        || async {},
        |_, _| {},
    )
    .await;

    assert!(!bilan.terminee);
    assert!(bilan.reportee.is_some());
    assert!(
        !deja_fait(&SettingsRepo::with_backend(db), "1.0.0-rc12"),
        "sans marqueur, le prochain démarrage reprend"
    );
}

#[tokio::test]
async fn une_route_pas_encore_deployee_ne_marque_rien() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-404");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    let site = faux_site(&[MBID_A], 0, 503, Some(404)).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || true,
        || async {},
        |_, _| {},
    )
    .await;

    assert!(!bilan.terminee);
    assert!(site.recu.lock().unwrap().envois.is_empty());
    assert!(!deja_fait(&SettingsRepo::with_backend(db), "1.0.0-rc12"));
}

#[tokio::test]
async fn un_refus_definitif_ecarte_l_image_et_la_passe_continue() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-422");
    artiste(&db, &cache, "Queen", Some(MBID_A), Some(1), "auto");
    artiste(&db, &cache, "Björk", Some(MBID_B), Some(2), "auto");
    // Le premier envoi est refusé (422), le second passe.
    let site = faux_site(&[MBID_A, MBID_B], 1, 422, None).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || true,
        || async {},
        |_, _| {},
    )
    .await;

    assert_eq!(bilan.ecartees, 1);
    assert_eq!(bilan.renvoyees, 1);
    assert!(bilan.terminee);
}

#[tokio::test]
async fn la_question_part_par_lots_de_500() {
    let db = base();
    let cache = crate::test_scratch::scratch_dir("renvoi-images-lots");
    for i in 0..1_001u32 {
        let mbid = format!("{i:08x}-0000-4000-8000-000000000000");
        artiste(&db, &cache, &format!("A{i}"), Some(&mbid), Some(1), "auto");
    }
    let site = faux_site(&[], 0, 503, None).await;

    let bilan = executer(
        db.clone(),
        &reglages(&site.base, &cache),
        || true,
        || async {},
        |_, _| {},
    )
    .await;

    let tailles: Vec<usize> = site
        .recu
        .lock()
        .unwrap()
        .questions
        .iter()
        .map(Vec::len)
        .collect();
    assert_eq!(tailles, vec![500, 500, 1]);
    assert_eq!(bilan.examines, 1_001);
    assert!(bilan.terminee);
}
