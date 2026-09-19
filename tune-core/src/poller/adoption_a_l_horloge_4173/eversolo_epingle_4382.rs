//! #4382, cas 2 — l'Eversolo DMP-A6 épinglé sur sa durée, qui nomme encore
//! la piste finie : la fin se lit sur l'horloge du RENDERER.
//!
//! ## Le journal rejoué (Villerio, DMP-A6, Tune 0.9.155, 18/09, fil 1845)
//!
//! ```text
//! 20:08:17.282 dlna_play url=".../stream/f5b05252-….wav"            (piste 1)
//! 20:08:18.191 gapless_arm_trace … position_ms=0                       (1er sondage : horloge 0)
//! 20:09:49.553 service_fichier_termine stream_id=f5b05252-… octets=41921756 complet=true
//! 20:11:47.186 gapless_arm_trace armed=true reported_duration_ms=237000
//!              queue_duration_ms=237651 position_ms=207000
//! 20:11:47.198 dlna_set_next url=".../stream/9f7e6510-….wav"
//! 20:11:48.189 gapless_arm_trace … position_ms=208000
//! 20:11:49.711 stream_request stream_id="9f7e6510-…" (tiré à ~542 Kio/s ensuite)
//! 20:12:20.219 position_past_end_advancing position_ms=237000 wall_secs=242
//!              dlna_frozen_end=true enchainement=Aucun flux_arme="9f7e6510-…"
//!              uri_courante=".../stream/f5b05252-….wav" octets_tires=25231012
//! 20:12:20.219 track_end_gap motif="position_past_end_frozen_dlna" plancher_ms=4000
//! 20:12:20.291 dlna_play url=".../stream/2d2c4075-….wav"            (piste 2, relance)
//! ```
//!
//! Le renderer rapporte +1 000 ms par sondage (~1,003 s) : il atteint 237 000
//! vers 20:12:17 et n'en bouge plus, PLAYING, `TrackURI` = piste 1. La piste 1
//! était entièrement téléchargée depuis 20:09:49 : rien ne le fait attendre
//! côté flux. Il n'enchaîne pas (25/08 : laissé seul, PLAYING éternel). Tune
//! ne concluait qu'à l'horloge `durée de file + END_MARGIN_MS` (240 651 ms)
//! — horloge de TUNE, partie au premier sondage, ~2 s devant celle du
//! renderer. La relance partait à 20:12:20.291 au lieu de ~20:12:18.
//!
//! ## Le modèle des sondages
//!
//! Sondage `k` (k = 0 à 20:11:47.186) : horloge de Tune
//! `⌊208,995 + 1,003 k⌋` s, position `min(⌊207,5 + 1,003 k⌋, 237) × 1000`.
//! k = 30 lit 237 000 pour la première fois (horloge 239), k = 31 la relit
//! inchangée (horloge 240), k = 32 est le premier où l'horloge de Tune passe
//! 240 651 ms (241).

use super::*;

/// Octets tirés du flux armé au moment du verdict (`octets_tires=25231012`).
const OCTETS_TIRES_1845: u64 = 25_231_012;
/// Le sondage qui relit 237 000 inchangé : un sondage entier d'épinglage.
const K_EPINGLE: u32 = 31;

fn horloge_secs(k: u32) -> u64 {
    (208.995 + 1.003 * k as f64).floor() as u64
}

fn position_ms(k: u32) -> u64 {
    (((207.5 + 1.003 * k as f64).floor() as u64) * 1000).min(DUREE_RENDERER_MS)
}

impl Banc {
    /// Un sondage tel que le DMP-A6 le rend, SANS toucher à la position
    /// précédente que le sondeur a lui-même retenue : c'est elle qui dit
    /// « épinglée ».
    async fn sondage_dmp_a6(&mut self, k: u32) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(APPAREIL).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            mock.set_duration(DUREE_RENDERER_MS);
            mock.set_position(position_ms(k));
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(horloge_secs(k)));
        self.tic().await;
    }

    /// Armement (k = 0) puis sondages 1..=`jusqu_a`. Rend le sondage où le
    /// `Play` de la piste 2 est parti, s'il est parti, et le flux armé.
    async fn rejouer_le_18_09(&mut self, uri: UriRapportee, jusqu_a: u32) -> (Option<u32>, String) {
        let (flux, _) = self.armer().await;
        self.le_renderer_tire(&flux, OCTETS_TIRES_1845).await;
        let uri = match uri {
            UriRapportee::PisteFinie => Some(format!(
                "http://192.168.1.196:8888/stream/{}.wav",
                self.flux_finie
            )),
            UriRapportee::Muette => None,
        };
        self.le_renderer_rapporte(uri).await;
        for k in 1..=jusqu_a {
            self.sondage_dmp_a6(k).await;
            if !self.play_complets().await.is_empty() {
                return (Some(k), flux);
            }
        }
        (None, flux)
    }
}

#[derive(Clone, Copy)]
enum UriRapportee {
    PisteFinie,
    Muette,
}

/// **Le témoin.** Épinglé sur 237 000 depuis un sondage entier, le DMP-A6
/// nomme encore le flux de « Speak to Me/Breathe » : la relance doit partir
/// À CE sondage (horloge 240), pas attendre l'horloge de Tune.
#[tokio::test]
async fn le_dmp_a6_epingle_sur_la_piste_finie_est_relance_sans_attendre_l_horloge() {
    assert_eq!(position_ms(K_EPINGLE - 1), POSITION_GELEE_MS);
    assert_eq!(position_ms(K_EPINGLE - 2), 236_000);
    assert_eq!(horloge_secs(K_EPINGLE), 240);

    let mut banc = Banc::monter().await;
    let (relance, flux) = banc
        .rejouer_le_18_09(UriRapportee::PisteFinie, K_EPINGLE + 3)
        .await;
    assert_eq!(
        relance,
        Some(K_EPINGLE),
        "sondage {K_EPINGLE} (horloge {} s, ~20:12:18) : le DMP-A6 est épinglé à 237000 depuis \
         un sondage entier et nomme encore le flux de la piste finie — aucune relance. Le repli \
         attend l'horloge de Tune (durée de file + END_MARGIN_MS = 240651 ms) : ce sont les \
         secondes de silence de #4382. Relance vue au sondage : {relance:?}",
        horloge_secs(K_EPINGLE)
    );
    // Le repli lui-même ne change pas : le renderer n'a pas enchaîné, la
    // piste 2 part par `SetAVTransportURI` + `Play`.
    verifier_le_repli(&banc, &flux).await;
}

// Contre-cas « le renderer nomme le flux ARMÉ, position épinglée » : c'est
// `l_uri_courante_nomme_le_flux_arme_on_adopte` du banc parent, dont
// `renderer_a` pose la position précédente égale à la position — épinglée —
// et qui doit toujours ADOPTER, sans `Play`.

/// Contre-cas : le renderer ne dit rien de son URI. L'épinglage ne conclut
/// pas ; au sondage épinglé, rien ne part encore.
#[tokio::test]
async fn un_renderer_muet_sur_son_uri_garde_le_chemin_a_l_horloge() {
    let mut banc = Banc::monter().await;
    let (relance, _) = banc.rejouer_le_18_09(UriRapportee::Muette, K_EPINGLE).await;
    assert_eq!(relance, None);
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (0, FINIE));
}

/// Les règles pures.
#[test]
fn l_epinglage_pur() {
    use decisions::{dlna_epingle_sur_la_piste_finie as epingle, ticks_epingle_sur_la_piste_finie};
    // La ligne du 18/09.
    assert!(epingle(
        true, true, true, 237_651, 237_000, 237_000, 237_000, true
    ));
    // Le premier sondage à 237 000 : la position vient d'arriver, elle n'est
    // pas encore épinglée.
    assert!(!epingle(
        true, true, true, 237_651, 237_000, 237_000, 236_000, true
    ));
    // URI du flux armé, ou pas d'URI : rien.
    assert!(!epingle(
        true, true, true, 237_651, 237_000, 237_000, 237_000, false
    ));
    // Sans SetNext, hors DLNA, pic trop bas : rien.
    assert!(!epingle(
        true, false, true, 237_651, 237_000, 237_000, 237_000, true
    ));
    assert!(!epingle(
        false, true, true, 237_651, 237_000, 237_000, 237_000, true
    ));
    assert!(!epingle(
        true, true, false, 237_651, 237_000, 237_000, 237_000, true
    ));
    // Un renderer qui plafonne sa position sur une durée FAUSSE (230 s pour
    // 237,6 s) joue peut-être encore : rien.
    assert!(!epingle(
        true, true, true, 237_651, 230_000, 230_000, 230_000, true
    ));
    // Durée rapportée arrondie AU-DESSUS : la position ne l'atteint jamais.
    assert!(!epingle(
        true, true, true, 237_651, 238_000, 237_000, 237_000, true
    ));

    assert_eq!(ticks_epingle_sur_la_piste_finie(237_651, 237_000), 1);
    assert_eq!(ticks_epingle_sur_la_piste_finie(237_000, 237_000), 1);
    assert_eq!(
        ticks_epingle_sur_la_piste_finie(237_900, 237_000),
        2,
        "un bout caché de 900 ms demande un sondage de plus"
    );
}
