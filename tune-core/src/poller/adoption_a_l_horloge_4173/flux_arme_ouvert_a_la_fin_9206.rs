//! #9206 (fil 2961) — gapless DSD vers un renderer DLNA (Android 7.1.2,
//! lecteur `Lavf/57.83.100`, DSF natif, `SetNext` accepté, pas de `Next`).
//!
//! ## Le journal (rc12, transition B)
//!
//! ```text
//! 19:48:17      gapless_next_set suivante=Inconnue
//! 19:48:46.993  stream_request (flux armé) range="bytes=0-"
//! 19:48:47.003  stream_request (flux armé) sonde de fin
//! 19:48:47.497  stream_request (flux armé) range="bytes=28-"   <- données DSD
//! 19:48:48.138  position_past_end_advancing position_ms=67000 track_dur=67293
//!               epingle_piste_finie=true enchainement=Aucun
//! 19:48:48.138  gapless_dsd_next_dlna_non_suivi motif="avance_bloquee"
//! 19:48:49.6    Pause, SetAVTransportURI, Play                  <- ~2,5 s de blanc
//! ```
//!
//! À la transition A, le même renderer avait ouvert le flux armé à 16.8 et
//! remis sa position à zéro à 18.0 : il bascule SEUL, 1,2 s après
//! l'ouverture, mais nomme encore la piste finie dans l'intervalle.
//!
//! ## Les épreuves (vrai `tick`, renderer simulé, suivant DSD armé)
//!
//! - le renderer lit le flux armé à la fin : la fin épinglée est différée,
//!   la bascule est suivie sans `Play`, aucun échec DSD n'est noté — que la
//!   bascule se lise par l'URI ou par la seule remise à zéro de la position ;
//! - renderer qui sonde l'en-tête au `SetNext` puis lit à la fin : daté à la
//!   lecture, différé ;
//! - contre-épreuve DMP-A6 (#4173/#4382) : flux armé lu 27 s avant la fin,
//!   renderer épinglé sur la piste finie → relance au MÊME sondage qu'avant ;
//! - contre-épreuve : flux lu à la fin, renderer qui ne bascule jamais →
//!   relance une fois la fenêtre passée (borné) ;
//! - contre-épreuve : en-tête seul → rien de différé ;
//! - suivant PCM (WAV) : rien de différé, le repli d'avant au sondage près.

use super::dsd_suivant_dlna_6910::annonce_dsd;
use super::*;
use crate::outputs::traits::AnnonceSuivante;

/// Octets tirés du flux armé par `bytes=0-` puis `bytes=28-` (DSD64 :
/// ~350 Kio/s, 0,6 s de lecture).
const OCTETS_LUS: u64 = 200_000;

impl Banc {
    /// Le renderer LIT le flux armé depuis `il_y_a` (seuil de l'en-tête
    /// franchi) et en a tiré `octets` — ce que `tune-stream-http` écrit.
    async fn le_renderer_ouvre_le_flux_arme(&self, flux: &str, octets: u64, il_y_a: Duration) {
        let sessions = self.orchestrator.streamer.sessions_state();
        let sessions = sessions.lock().await;
        let session = sessions.get(flux).expect("le flux armé existe");
        session
            .bytes_sent
            .store(octets, std::sync::atomic::Ordering::Relaxed);
        session
            .lecture_au_dela_de_l_en_tete
            .set(std::time::Instant::now() - il_y_a)
            .expect("lecture datée une seule fois");
    }

    /// Le service HTTP compte `octets` servis sur le flux armé, MAINTENANT —
    /// le vrai chemin de `tune-stream-http`.
    async fn le_service_sert(&self, flux: &str, octets: u64) {
        let sessions = self.orchestrator.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(flux)
            .expect("le flux armé existe")
            .compter_octets_servis(octets);
    }

    /// Un sondage tel que le renderer le rend, SANS retoucher la position
    /// que le sondeur a lui-même retenue : c'est elle qui dit « épinglée »
    /// et « remise à zéro ».
    async fn sondage_9206(&mut self, position_ms: u64, horloge_secs: u64) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(&self.appareil).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            mock.set_duration(DUREE_RENDERER_MS);
            mock.set_position(position_ms);
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(horloge_secs));
        self.tic().await;
    }

    fn uri_finie_9206(&self) -> String {
        format!("http://192.168.1.196:8888/stream/{}.wav", self.flux_finie)
    }

    /// Jusqu'au sondage où la position atteint la durée rapportée (pas encore
    /// épinglée), le renderer nommant la piste finie.
    async fn jusqu_a_la_duree(&mut self) {
        let uri = self.uri_finie_9206();
        self.le_renderer_rapporte(Some(uri)).await;
        self.sondage_9206(235_000, 238).await;
        self.sondage_9206(236_000, 239).await;
        self.sondage_9206(POSITION_GELEE_MS, 240).await;
        assert!(
            self.play_complets().await.is_empty(),
            "position à peine arrivée sur la durée : rien ne conclut encore"
        );
    }
}

/// Zone DSD du fil 2961, suivant DSD armé : le banc, le flux armé, son URL.
async fn monter_9206(appareil: &str) -> (Banc, String, String) {
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "auto",
        annonce_dsd(true),
        AnnonceSuivante::Annoncee,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    let flux = banc
        .orchestrator
        .flux_pre_arme(banc.zone_id)
        .await
        .expect("le suivant DSD est armé par flux");
    let url = banc.url_armee().await.expect("URL armée");
    (banc, flux, url)
}

/// Après la bascule : aucun `Play`, l'écran sur la piste armée, aucun échec
/// de suivant DSD noté pour l'appareil.
async fn verifier_la_bascule_suivie(banc: &Banc, appareil: &str) {
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "la bascule du renderer est suivie, sans `SetAVTransportURI` + `Play`"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "l'écran suit le renderer sur la piste armée"
    );
    assert_eq!(
        banc.orchestrator.echecs_dsd_suivant_pour_banc(appareil),
        0,
        "#9206 : l'appareil a enchaîné — aucun échec de suivant DSD ne doit être noté"
    );
}

/// ⭐ Le cas du fil 2961. Renderer DSD natif, suivant DSD armé ; à la fin il
/// ouvre le flux armé et le lit, mais nomme encore la piste finie au sondage
/// épinglé. Avant : relance au premier sondage épinglé (~2,5 s de blanc) et
/// garde DSD rendue à l'appareil. Après : rien ne part, la bascule du
/// renderer est suivie, aucun échec noté.
#[tokio::test]
async fn le_renderer_qui_ouvre_le_flux_arme_a_la_fin_n_est_pas_coupe_9206() {
    let appareil = "uuid:banc-9206-android-dsd";
    let (mut banc, flux, url) = monter_9206(appareil).await;
    banc.jusqu_a_la_duree().await;
    // 19:48:46.99 → 47.50 : `bytes=0-`, sonde de fin, `bytes=28-`.
    banc.le_renderer_ouvre_le_flux_arme(&flux, OCTETS_LUS, Duration::from_millis(1_150))
        .await;
    // 19:48:48.14 : position épinglée, URI = piste finie.
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "#9206 : le renderer vient d'ouvrir et de lire le flux armé — la fin épinglée ne doit \
         pas relancer la suivante au premier sondage"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (0, FINIE));

    // Le renderer a basculé — URI du flux armé, position à zéro.
    banc.le_renderer_rapporte(Some(url)).await;
    banc.sondage_9206(1_000, 242).await;
    verifier_la_bascule_suivie(&banc, appareil).await;
}

/// Même cas, mais le renderer bascule SANS mettre son URI à jour : seule la
/// remise à zéro de sa position le dit. Elle doit suffire.
#[tokio::test]
async fn remise_a_zero_pendant_le_report_uri_restee_sur_la_piste_finie_9206() {
    let appareil = "uuid:banc-9206-uri-en-retard";
    let (mut banc, flux, _) = monter_9206(appareil).await;
    banc.jusqu_a_la_duree().await;
    banc.le_renderer_ouvre_le_flux_arme(&flux, OCTETS_LUS, Duration::from_millis(1_150))
        .await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    // URI toujours celle de la piste finie ; position repartie de zéro.
    banc.sondage_9206(1_000, 242).await;
    verifier_la_bascule_suivie(&banc, appareil).await;
}

/// Renderer qui SONDE l'en-tête du flux armé au `SetNext` (quelques
/// octets), puis le LIT à la fin : la datation suit la lecture, pas la
/// sonde, et la fin est différée.
#[tokio::test]
async fn sonde_de_l_en_tete_au_set_next_puis_lecture_a_la_fin_9206() {
    let appareil = "uuid:banc-9206-sonde-en-tete";
    let (mut banc, flux, url) = monter_9206(appareil).await;
    banc.le_service_sert(&flux, 92).await;
    // La sonde est plus vieille que la fenêtre (horloge réelle : la datation
    // est un `std::time::Instant`).
    std::thread::sleep(
        Duration::from_millis(decisions::FENETRE_BASCULE_SUR_FLUX_ARME_MS)
            + Duration::from_millis(200),
    );
    banc.jusqu_a_la_duree().await;
    banc.le_service_sert(&flux, OCTETS_LUS).await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "la lecture vient de commencer : la sonde d'en-tête ne vieillit pas la datation"
    );
    banc.le_renderer_rapporte(Some(url)).await;
    banc.sondage_9206(1_000, 242).await;
    verifier_la_bascule_suivie(&banc, appareil).await;
}

/// Contre-épreuve (#4173/#4382, DMP-A6) : le flux armé a été lu ~3 s après
/// le `SetNext` (27 s avant la fin) et le renderer, épinglé, nomme la piste
/// finie. Il n'enchaînera pas : la relance part au MÊME sondage qu'avant.
#[tokio::test]
async fn le_dmp_a6_qui_a_precharge_le_flux_arme_est_relance_comme_avant_9206() {
    let (mut banc, flux, _) = monter_9206("uuid:banc-9206-dmp-a6").await;
    banc.le_renderer_ouvre_le_flux_arme(&flux, OCTETS_TIRES, Duration::from_secs(27))
        .await;
    banc.jusqu_a_la_duree().await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    verifier_le_repli(&banc, &flux).await;
}

/// Contre-épreuve : le renderer lit le flux armé à la fin mais ne bascule
/// jamais. La fin n'est différée que dans la fenêtre ; passé celle-ci, le
/// repli d'avant relance la suivante.
#[tokio::test]
async fn ouvert_a_la_fin_sans_bascule_le_repli_part_apres_la_fenetre_9206() {
    let (mut banc, flux, _) = monter_9206("uuid:banc-9206-sans-bascule").await;
    banc.jusqu_a_la_duree().await;
    let marge = Duration::from_secs(1);
    banc.le_renderer_ouvre_le_flux_arme(
        &flux,
        OCTETS_LUS,
        Duration::from_millis(decisions::FENETRE_BASCULE_SUR_FLUX_ARME_MS) - marge,
    )
    .await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "dans la fenêtre, la fin est différée"
    );
    // Horloge réelle : la lecture est datée en `std::time::Instant`.
    std::thread::sleep(marge + Duration::from_millis(200));
    banc.sondage_9206(POSITION_GELEE_MS, 242).await;
    verifier_le_repli(&banc, &flux).await;
}

/// Contre-épreuve : le flux armé n'a servi que l'en-tête (sonde refermée) —
/// rien n'est différé.
#[tokio::test]
async fn en_tete_seul_ne_differe_rien_9206() {
    let (mut banc, flux, _) = monter_9206("uuid:banc-9206-en-tete").await;
    banc.jusqu_a_la_duree().await;
    banc.le_service_sert(&flux, 92).await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    verifier_le_repli(&banc, &flux).await;
}

/// Suivant PCM (WAV) : le report est réservé au DSD. Un flux armé lu à la
/// fin ne diffère rien — le repli d'avant, au sondage près.
#[tokio::test]
async fn suivant_pcm_rien_de_differe_9206() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    banc.jusqu_a_la_duree().await;
    banc.le_renderer_ouvre_le_flux_arme(&flux, OCTETS_LUS, Duration::from_millis(1_150))
        .await;
    banc.sondage_9206(POSITION_GELEE_MS, 241).await;
    verifier_le_repli(&banc, &flux).await;
}

/// La datation : la sonde d'en-tête ne date rien ; le franchissement du
/// seuil, si.
#[tokio::test]
async fn la_lecture_est_datee_au_franchissement_de_l_en_tete_9206() {
    let mut banc = Banc::monter().await;
    let (flux, _) = banc.armer().await;
    let orchestrateur = banc.orchestrator.clone();
    banc.le_service_sert(&flux, 92).await;
    banc.le_service_sert(&flux, decisions::OCTETS_AU_DELA_DE_L_EN_TETE - 92)
        .await;
    assert_eq!(
        orchestrateur
            .streamer_lecture_au_dela_de_l_en_tete_depuis_ms(&flux)
            .await,
        None,
        "seuil atteint, pas franchi"
    );
    banc.le_service_sert(&flux, 1).await;
    assert!(
        orchestrateur
            .streamer_lecture_au_dela_de_l_en_tete_depuis_ms(&flux)
            .await
            .is_some_and(|ms| ms < 1_000)
    );
}

/// La règle pure.
#[test]
fn la_bascule_en_cours_pure_9206() {
    use decisions::{EnchainementArme as E, bascule_en_cours_sur_le_flux_arme as en_cours};
    let fenetre = decisions::FENETRE_BASCULE_SUR_FLUX_ARME_MS;
    assert!(en_cours(E::Aucun, true, Some(OCTETS_LUS), Some(1_150)));
    assert!(en_cours(E::Aucun, true, Some(OCTETS_LUS), Some(fenetre)));
    assert!(
        !en_cours(E::Aucun, false, Some(OCTETS_LUS), Some(1_150)),
        "PCM"
    );
    assert!(!en_cours(
        E::Aucun,
        true,
        Some(OCTETS_LUS),
        Some(fenetre + 1)
    ));
    assert!(
        !en_cours(E::Aucun, true, Some(OCTETS_LUS), Some(27_000)),
        "DMP-A6"
    );
    assert!(
        !en_cours(E::Aucun, true, Some(OCTETS_LUS), None),
        "non daté"
    );
    assert!(
        !en_cours(E::Aucun, true, Some(0), Some(500)),
        "RS130 : rien tiré"
    );
    assert!(
        !en_cours(E::Aucun, true, None, Some(500)),
        "session disparue"
    );
    assert!(
        !en_cours(
            E::Aucun,
            true,
            Some(decisions::OCTETS_AU_DELA_DE_L_EN_TETE),
            Some(500)
        ),
        "en-tête seul"
    );
    assert!(!en_cours(E::Certain, true, Some(OCTETS_LUS), Some(500)));
    assert!(!en_cours(E::Probable, true, Some(OCTETS_LUS), Some(500)));
}
