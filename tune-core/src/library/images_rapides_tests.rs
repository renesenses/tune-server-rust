//! Témoins des passes d'images rapides (01/11/2026). Aucun appel réel à un
//! service : les sources sont simulées (latence, limiteur), et le seul
//! serveur HTTP est local.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

/// Une source simulée : elle attend son créneau sur `limiteur` (s'il y en a
/// un), note l'instant où elle part, puis répond après `latence`.
async fn source_simulee(
    limiteur: Option<&RateLimiter>,
    departs: &std::sync::Mutex<Vec<Instant>>,
    latence: Duration,
) -> Option<u32> {
    if let Some(l) = limiteur {
        l.acquire("simule").await;
    }
    departs.lock().unwrap().push(Instant::now());
    tokio::time::sleep(latence).await;
    Some(1)
}

async fn passe_simulee(
    n: usize,
    largeur: usize,
    limiteur: Option<&RateLimiter>,
    latence: Duration,
) -> (Duration, Vec<Instant>) {
    let departs = std::sync::Mutex::new(Vec::new());
    let debut = Instant::now();
    let total: u32 = en_parallele_borne(
        0..n,
        largeur,
        || async {},
        |_| source_simulee(limiteur, &departs, latence),
    )
    .map(|r| r.unwrap_or(0))
    .fold(0, |a, b| async move { a + b })
    .await;
    assert_eq!(
        total as usize, n,
        "chaque élément doit être traité une fois"
    );
    (debut.elapsed(), departs.into_inner().unwrap())
}

/// Gain mesuré : 40 éléments, 80 ms de latence chacun. Un à la fois : ≥ 3,2 s.
/// Quatre à la fois : ~0,8 s. Le témoin exige au moins un facteur 3.
#[tokio::test]
async fn quatre_elements_a_la_fois_vont_au_moins_trois_fois_plus_vite() {
    let latence = Duration::from_millis(80);
    let (sequentiel, _) = passe_simulee(40, 1, None, latence).await;
    let (parallele, _) = passe_simulee(40, ELEMENTS_EN_PARALLELE, None, latence).await;
    eprintln!(
        "40 éléments à 80 ms : séquentiel {sequentiel:?}, {ELEMENTS_EN_PARALLELE} en parallèle {parallele:?}"
    );
    assert!(
        parallele * 3 <= sequentiel,
        "la passe n'est pas parallèle : {parallele:?} contre {sequentiel:?} en séquentiel"
    );
}

/// Le parallélisme ne dépasse JAMAIS le débit du service : 30 éléments,
/// un limiteur à une requête toutes les 20 ms, 4 à la fois. Deux départs ne
/// sont jamais à moins de 20 ms l'un de l'autre, et la passe reste plus
/// rapide que la version séquentielle (latence 80 ms > intervalle).
#[tokio::test]
async fn le_debit_permis_par_la_source_est_tenu_en_parallele() {
    let intervalle = Duration::from_millis(20);
    let limiteur = RateLimiter::with_interval(intervalle);
    let latence = Duration::from_millis(80);
    let (parallele, mut departs) = passe_simulee(30, 4, Some(&limiteur), latence).await;
    departs.sort();
    for paire in departs.windows(2) {
        let ecart = paire[1] - paire[0];
        assert!(
            ecart + Duration::from_millis(1) >= intervalle,
            "deux requêtes à {ecart:?} d'intervalle : le débit du service est dépassé"
        );
    }
    let limiteur_seq = RateLimiter::with_interval(intervalle);
    let (sequentiel, _) = passe_simulee(30, 1, Some(&limiteur_seq), latence).await;
    eprintln!(
        "30 éléments, 20 ms/requête, 80 ms de latence : séquentiel {sequentiel:?}, 4 en parallèle {parallele:?}"
    );
    assert!(
        parallele * 2 <= sequentiel,
        "{parallele:?} contre {sequentiel:?}"
    );
}

/// L'admission reste un élément à la fois : une pause d'admission de 30 ms
/// (la pause « en lecture ») espace les DÉPARTS comme en séquentiel, même
/// quatre à la fois.
#[tokio::test]
async fn la_pause_de_lecture_ne_se_multiplie_pas_par_la_largeur() {
    let departs = std::sync::Mutex::new(Vec::new());
    let n = 8;
    en_parallele_borne(
        0..n,
        4,
        || tokio::time::sleep(Duration::from_millis(30)),
        |_| source_simulee(None, &departs, Duration::from_millis(1)),
    )
    .collect::<Vec<_>>()
    .await;
    let mut d = departs.into_inner().unwrap();
    d.sort();
    for paire in d.windows(2) {
        assert!(
            paire[1] - paire[0] >= Duration::from_millis(29),
            "deux éléments admis ensemble malgré la pause"
        );
    }
}

/// La porte d'un hôte : jamais plus de REQUETES_PAR_HOTE requêtes en vol.
#[tokio::test]
async fn la_porte_d_un_hote_borne_ses_requetes_simultanees() {
    let en_vol = Arc::new(AtomicUsize::new(0));
    let pic = Arc::new(AtomicUsize::new(0));
    let mut taches = Vec::new();
    for _ in 0..12 {
        let en_vol = en_vol.clone();
        let pic = pic.clone();
        taches.push(tokio::spawn(async move {
            let _p = passer(Hote::Mozaiklabs, "").await.unwrap();
            let n = en_vol.fetch_add(1, Ordering::SeqCst) + 1;
            pic.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(1_000)).await;
            en_vol.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for t in taches {
        t.await.unwrap();
    }
    let pic = pic.load(Ordering::SeqCst);
    assert!(
        pic >= 2,
        "la porte n'a rien laissé passer en parallèle (pic {pic})"
    );
    assert!(
        pic <= REQUETES_PAR_HOTE,
        "{pic} requêtes simultanées vers un même hôte (plafond {REQUETES_PAR_HOTE})"
    );
}

fn source<'a>(latence_ms: u64, rend: Option<&'static str>) -> BoxFuture<'a, Option<&'static str>> {
    Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(latence_ms)).await;
        rend
    })
}

/// La priorité est celle de la cascade : la source préférée gagne même quand
/// une source moins bonne répond avant elle, et seule la gagnante est
/// « téléchargée ».
#[tokio::test]
async fn la_meilleure_source_gagne_meme_quand_elle_repond_en_dernier() {
    let telecharges = std::sync::Mutex::new(Vec::new());
    let r = premiere_retenue(
        vec![
            ("preferee", source(120, Some("a"))),
            ("rapide", source(5, Some("b"))),
        ],
        |nom, v| {
            telecharges.lock().unwrap().push(nom);
            async move { Some(v) }
        },
    )
    .await;
    assert_eq!(r, Some(("a", "preferee")));
    assert_eq!(*telecharges.lock().unwrap(), vec!["preferee"]);
}

/// Les recherches se recouvrent : trois sources de 100 ms dont seule la
/// dernière trouve, en ~100 ms et non 300.
#[tokio::test]
async fn les_sources_d_un_element_sont_interrogees_en_parallele() {
    let debut = Instant::now();
    let r = premiere_retenue(
        vec![
            (1, source(100, None)),
            (2, source(100, None)),
            (3, source(100, Some("c"))),
        ],
        |_, v| async move { Some(v) },
    )
    .await;
    let duree = debut.elapsed();
    assert_eq!(r, Some(("c", 3)));
    assert!(
        duree < Duration::from_millis(250),
        "les sources ont été interrogées l'une après l'autre ({duree:?})"
    );
}

/// Une image refusée (empreinte rejetée, téléchargement raté) passe la main
/// à la source suivante ; une source lente de rang inférieur n'est pas
/// attendue une fois la gagnante trouvée.
#[tokio::test]
async fn un_refus_passe_a_la_suivante_et_la_gagnante_n_attend_pas_les_moins_bonnes() {
    let debut = Instant::now();
    let r = premiere_retenue(
        vec![
            ("refusee", source(10, Some("x"))),
            ("bonne", source(20, Some("y"))),
            ("lente", source(2_000, Some("z"))),
        ],
        |nom, v| async move { (nom != "refusee").then_some(v) },
    )
    .await;
    assert_eq!(r, Some(("y", "bonne")));
    assert!(debut.elapsed() < Duration::from_millis(1_000));
}

#[tokio::test]
async fn les_echecs_passagers_sont_comptes_par_element() {
    let ((), n) = en_comptant_les_transitoires(async {
        let _ = premiere_retenue(
            vec![
                (
                    1,
                    Box::pin(async {
                        noter_transitoire();
                        None::<u8>
                    }) as BoxFuture<'_, Option<u8>>,
                ),
                (
                    2,
                    Box::pin(async {
                        noter_transitoire();
                        None::<u8>
                    }) as BoxFuture<'_, Option<u8>>,
                ),
            ],
            |_, v| async move { Some(v) },
        )
        .await;
    })
    .await;
    assert_eq!(n, 2);
    let ((), zero) = en_comptant_les_transitoires(async {}).await;
    assert_eq!(
        zero, 0,
        "le compte d'un élément ne déborde pas sur le suivant"
    );
}

#[test]
fn le_cache_negatif_ecarte_puis_redonne_sa_chance_apres_le_delai() {
    let mut c = CacheNegatif::vide(CLE_ABSENTS_ARTISTES);
    let t0 = 1_000_000;
    assert!(c.a_retenter(7, t0));
    c.noter_absent(7, t0);
    assert!(
        !c.a_retenter(7, t0 + 3600),
        "un absent est retenté dès la passe suivante"
    );
    assert!(
        c.a_retenter(7, t0 + DELAI_ABSENT_S),
        "un absent n'est jamais retenté"
    );
    c.oublier(7);
    assert!(c.a_retenter(7, t0 + 1));
}

#[test]
fn le_cache_negatif_survit_au_redemarrage_et_se_purge() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
    let t0 = 2_000_000;
    let mut c = CacheNegatif::charger(&settings, CLE_ABSENTS_ALBUMS);
    c.noter_absent(1, t0);
    c.noter_absent(2, t0 - DELAI_ABSENT_S - 1);
    c.enregistrer(&settings, t0);
    let relu = CacheNegatif::charger(&settings, CLE_ABSENTS_ALBUMS);
    assert!(!relu.a_retenter(1, t0 + 10));
    assert_eq!(relu.len(), 1, "l'entrée échue devait être retirée");
}

#[test]
fn chaque_service_a_son_limiteur_et_ses_debits_publies() {
    assert_eq!(
        Hote::de_l_url("https://musicbrainz.org/ws/2/artist/x"),
        Hote::MusicBrainz
    );
    assert_eq!(
        Hote::de_l_url("https://coverartarchive.org/release/x/front-500"),
        Hote::CoverArtArchive
    );
    assert_eq!(
        Hote::de_l_url("https://upload.wikimedia.org/a.jpg"),
        Hote::Wikimedia
    );
    assert_eq!(
        Hote::de_l_url("https://commons.wikimedia.org/wiki/Special:Redirect/file/a"),
        Hote::Wikimedia
    );
    assert_eq!(
        Hote::de_l_url("https://api.discogs.com/database/search"),
        Hote::Discogs
    );
    assert_eq!(
        Hote::de_l_url("https://e-cdns-images.dzcdn.net/a.jpg"),
        Hote::Deezer
    );
    assert_eq!(
        Hote::de_l_url("https://ws.audioscrobbler.com/2.0/"),
        Hote::LastFm
    );
    assert_eq!(Hote::de_l_url("https://example.org/x"), Hote::Autre);
    // MusicBrainz passe par LE limiteur partagé (1/s), pas par un second.
    let (mb, cle) = Hote::MusicBrainz.limiteur();
    assert!(std::ptr::eq(mb, &*MUSICBRAINZ));
    assert!(mb.intervalle_courant(cle) >= Duration::from_secs(1));
    let (td, cle) = Hote::TheAudioDb.limiteur();
    assert_eq!(
        td.intervalle_courant(cle),
        Duration::from_secs(2),
        "TheAudioDB : 30/min"
    );
    let (it, cle) = Hote::Itunes.limiteur();
    assert_eq!(
        it.intervalle_courant(cle),
        Duration::from_secs(3),
        "iTunes : 20/min"
    );
    let (d, cle) = Hote::Discogs.limiteur();
    assert!(std::ptr::eq(d, &*DISCOGS));
    assert!(d.intervalle_courant(cle) >= Duration::from_millis(1000));
}

/// Un `429` avec `Retry-After: 1` : la requête suivante vers le même hôte
/// attend au moins une seconde. Serveur local, aucun service réel.
#[tokio::test]
async fn un_429_avec_retry_after_retarde_la_requete_suivante() {
    use axum::{Router, http::StatusCode, routing::get};
    let appels = Arc::new(AtomicUsize::new(0));
    let a = appels.clone();
    let app = Router::new().route(
        "/x",
        get(move || {
            let a = a.clone();
            async move {
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], "{}")
                } else {
                    (StatusCode::OK, [("retry-after", "0")], "{\"ok\":true}")
                }
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    let url = format!("http://{adresse}/x");
    let client = reqwest::Client::new();
    let (premier, n) = en_comptant_les_transitoires(json(client.get(&url), &url)).await;
    assert!(premier.is_none());
    assert_eq!(
        n, 1,
        "un 429 est un échec passager : jamais inscrit au cache négatif"
    );
    let debut = Instant::now();
    let second = json(client.get(&url), &url).await;
    assert_eq!(second, Some(serde_json::json!({"ok": true})));
    assert!(
        debut.elapsed() >= Duration::from_millis(950),
        "Retry-After ignoré : seconde requête après {:?}",
        debut.elapsed()
    );
}
