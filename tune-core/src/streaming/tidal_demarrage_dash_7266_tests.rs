//! #7266 — sur une zone DLNA, un FLAC CD TIDAL démarrait après 11 à 60 s.
//!
//! Depuis la rc8 (#6334), le FLAC CD arrive en MPEG-DASH à SegmentTemplate,
//! une soixantaine de segments de ~4 s pour une piste de 4 min. Avant que la
//! moindre URL ne parte vers le renderer, `download_dash_segments` les
//! demandait au CDN UN PAR UN : l'attente valait la somme des allers-retours.
//!
//! Le faux CDN ci-dessous, sur la boucle locale, met 100 ms à rendre chaque
//! segment (une latence de CDN ordinaire vue d'une box). 60 segments un par
//! un : 6 s au moins. Il compte aussi les requêtes en vol, pour vérifier que
//! l'attente est réduite sans que la mémoire tenue grossisse sans borne.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const SEGMENTS: u32 = 60;
const LATENCE_MS: u64 = 100;
const INIT: &[u8] = b"\0\0\0\x18ftypiso6essai-init";

#[derive(Default)]
struct Compteurs {
    en_vol: AtomicUsize,
    max_en_vol: AtomicUsize,
    /// Segments média demandés au faux CDN.
    demandes: AtomicUsize,
}

/// Faux CDN : `0.mp4` est l'init, `n.mp4` rend un octet, `n`. Le segment
/// `en_echec` (s'il y en a un) répond 503.
async fn faux_cdn(en_echec: Option<u32>) -> (String, Arc<Compteurs>) {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    #[derive(Clone)]
    struct Etat {
        compteurs: Arc<Compteurs>,
        en_echec: Option<u32>,
    }

    async fn segment(
        State(etat): State<Etat>,
        Path(fichier): Path<String>,
    ) -> axum::response::Response {
        let numero: u32 = fichier.trim_end_matches(".mp4").parse().unwrap_or(999);
        if numero == 0 {
            return INIT.to_vec().into_response();
        }
        let c = &etat.compteurs;
        c.demandes.fetch_add(1, Ordering::SeqCst);
        let n = c.en_vol.fetch_add(1, Ordering::SeqCst) + 1;
        c.max_en_vol.fetch_max(n, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(LATENCE_MS)).await;
        c.en_vol.fetch_sub(1, Ordering::SeqCst);
        if Some(numero) == etat.en_echec {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        vec![numero as u8].into_response()
    }

    let compteurs = Arc::new(Compteurs::default());
    let app = axum::Router::new()
        .route("/mediatracks/essai/{fichier}", get(segment))
        .with_state(Etat {
            compteurs: compteurs.clone(),
            en_echec,
        });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://{adresse}"), compteurs)
}

fn segments(base: &str) -> DashSegmentInfo {
    DashSegmentInfo {
        init_url: format!("{base}/mediatracks/essai/0.mp4"),
        media_template: format!("{base}/mediatracks/essai/$Number$.mp4"),
        start_number: 1,
        segment_count: SEGMENTS,
    }
}

async fn telecharger(base: &str) -> (Vec<u8>, Duration) {
    let svc = TidalService::new();
    let debut = Instant::now();
    let chemin = svc
        .download_dash_segments(&segments(base), "7266")
        .await
        .expect("init et segments servis par le faux CDN");
    let duree = debut.elapsed();
    let octets = std::fs::read(&chemin).unwrap();
    let _ = std::fs::remove_file(&chemin);
    (octets, duree)
}

/// Le témoin du dossier : borne l'étape qui retenait le Play d'un FLAC CD.
/// Un par un, 60 × 100 ms = 6 s au moins ; six en vol, ~1 s. La borne de 3 s
/// laisse de la marge à une machine chargée sans laisser passer l'ancien
/// comportement.
#[tokio::test]
async fn le_flac_cd_dash_ne_s_attend_plus_segment_apres_segment_7266() {
    let (base, _) = faux_cdn(None).await;
    let (octets, duree) = telecharger(&base).await;
    let un_par_un = Duration::from_millis(LATENCE_MS * SEGMENTS as u64);
    assert!(
        duree < Duration::from_secs(3),
        "les {SEGMENTS} segments DASH ont pris {duree:?} (un par un : {un_par_un:?} au moins) : \
         le Play attend encore le dernier segment, requête après requête"
    );
    let attendus: Vec<u8> = (1..=SEGMENTS as u8).collect();
    assert_eq!(&octets[..INIT.len()], INIT, "l'init ouvre le fichier");
    assert_eq!(
        &octets[INIT.len()..],
        &attendus[..],
        "segments écrits dans l'ordre"
    );
}

/// Pas de prétampon qui grossit sans borne : jamais plus de
/// `SEGMENTS_DASH_EN_VOL` requêtes (donc de segments en mémoire) à la fois.
#[tokio::test]
async fn les_segments_en_vol_restent_bornes_7266() {
    let (base, compteurs) = faux_cdn(None).await;
    let _ = telecharger(&base).await;
    let max = compteurs.max_en_vol.load(Ordering::SeqCst);
    assert!(
        max <= SEGMENTS_DASH_EN_VOL,
        "{max} segments en vol, plafond {SEGMENTS_DASH_EN_VOL}"
    );
    assert!(max > 1, "un seul segment en vol : téléchargement un par un");
}

/// Un segment refusé est sauté et journalisé comme avant ; les autres
/// restent dans l'ordre (le parallélisme ne mélange rien).
#[tokio::test]
async fn un_segment_refuse_n_emmele_pas_les_suivants_7266() {
    let (base, _) = faux_cdn(Some(17)).await;
    let (octets, _) = telecharger(&base).await;
    let attendus: Vec<u8> = (1..=SEGMENTS as u8).filter(|n| *n != 17).collect();
    assert_eq!(&octets[INIT.len()..], &attendus[..]);
}

/// Le téléchargement en croissance, tel qu'une sortie locale le demande.
async fn en_croissance(
    base: &str,
) -> (String, Option<Arc<std::sync::atomic::AtomicBool>>, Duration) {
    let svc = TidalService::new();
    let debut = Instant::now();
    let (chemin, complet) = crate::audio::dash_growth::pendant_le_telechargement(
        true,
        svc.telecharger_les_segments_dash(&segments(base), "7266-croissance"),
    )
    .await
    .expect("init servi par le faux CDN");
    (chemin, complet, debut.elapsed())
}

/// #7266, sortie locale : le chemin revient dès le segment d'init, sans
/// attendre les 60 segments (≥ 1 s à six en vol). Le fichier se complète
/// ensuite, dans l'ordre, six segments en vol au plus, et le témoin
/// `complet` passe à vrai à la fin.
#[tokio::test]
async fn en_croissance_le_chemin_revient_des_l_init_7266() {
    let (base, compteurs) = faux_cdn(None).await;
    let (chemin, complet, duree) = en_croissance(&base).await;
    let complet = complet.expect("un téléchargement en croissance a son témoin");
    assert!(
        duree < Duration::from_millis(LATENCE_MS * 3),
        "chemin rendu en {duree:?} : il attend encore les segments"
    );
    assert!(
        !complet.load(Ordering::SeqCst),
        "le fichier ne peut pas être complet dès l'init"
    );
    let poignee = crate::audio::dash_growth::take_for(&chemin).expect("poignée inscrite");
    let attente = poignee.clone();
    let taille = tokio::task::spawn_blocking(move || attente.attendre_la_fin())
        .await
        .unwrap()
        .expect("téléchargement mené au bout");
    assert!(complet.load(Ordering::SeqCst), "témoin posé à la fin");
    let octets = std::fs::read(&chemin).unwrap();
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(octets.len() as u64, taille);
    let attendus: Vec<u8> = (1..=SEGMENTS as u8).collect();
    assert_eq!(&octets[..INIT.len()], INIT);
    assert_eq!(
        &octets[INIT.len()..],
        &attendus[..],
        "segments dans l'ordre"
    );
    let max = compteurs.max_en_vol.load(Ordering::SeqCst);
    assert!(
        max > 1 && max <= SEGMENTS_DASH_EN_VOL,
        "{max} segments en vol (plafond {SEGMENTS_DASH_EN_VOL}, et pas un par un)"
    );
    drop(poignee);
}

/// Une lecture abandonnée (le décodeur a lâché la poignée) arrête le
/// téléchargement au lieu de tirer la piste entière en concurrence avec la
/// suivante, et le fichier partiel est retiré.
#[tokio::test]
async fn une_lecture_abandonnee_arrete_le_telechargement_7266() {
    let (base, compteurs) = faux_cdn(None).await;
    let (chemin, complet, _) = en_croissance(&base).await;
    drop(crate::audio::dash_growth::take_for(&chemin).expect("poignée inscrite"));
    tokio::time::sleep(Duration::from_millis(LATENCE_MS * SEGMENTS as u64 / 2)).await;
    let demandes = compteurs.demandes.load(Ordering::SeqCst);
    assert!(
        demandes < SEGMENTS as usize / 2,
        "{demandes} segments demandés après l'abandon de la lecture"
    );
    assert!(
        !std::path::Path::new(&chemin).exists(),
        "le fichier partiel reste sur le disque"
    );
    assert!(!complet.unwrap().load(Ordering::SeqCst));
}

/// Hors d'une résolution pour une sortie locale (renderer réseau,
/// préchargement), rien ne change : le fichier rendu est complet.
#[tokio::test]
async fn hors_sortie_locale_le_fichier_rendu_est_complet_7266() {
    let (base, _) = faux_cdn(None).await;
    let svc = TidalService::new();
    let (chemin, complet) = crate::audio::dash_growth::pendant_le_telechargement(
        false,
        svc.telecharger_les_segments_dash(&segments(&base), "7266-complet"),
    )
    .await
    .unwrap();
    assert!(complet.is_none());
    assert!(crate::audio::dash_growth::take_for(&chemin).is_none());
    let octets = std::fs::read(&chemin).unwrap();
    let _ = std::fs::remove_file(&chemin);
    assert_eq!(octets.len(), INIT.len() + SEGMENTS as usize);
}
