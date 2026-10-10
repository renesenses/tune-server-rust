//! #4382 — le blanc entre deux pistes sur l'Eversolo DMP-A6, MESURÉ sur le
//! banc, sondage par sondage (rapport de Villerio en 1.0.0-rc2, 05/10).
//!
//! ## Le faux renderer : ce que le journal du 05/10 montre de l'A6
//!
//! - armement à 207 000 ms, 30 s avant la fin : `SetNext` acquitté, la
//!   suivante TENUE (`NextURI` = notre URL, action `Next` déclarée), le flux
//!   armé tiré ;
//! - position rapportée à la seconde, un sondage par ~1 s ; elle atteint
//!   237 000 (sa durée) et s'y épingle, `PLAYING`, `TrackURI` sur la piste
//!   finie ;
//! - `Next` acquitté sans erreur SOAP, puis RIEN : position épinglée,
//!   `GetMediaInfo` rend toujours la piste finie en `CurrentURI` et notre
//!   suivante en `NextURI`, à 1 s, 2 s et 3 s.
//!
//! Le `MockOutput` de type `dlna` joue exactement cela
//! (`bascule_honoree(false)`, `media_du_transport`).
//!
//! ## Ce qui est mesuré
//!
//! Le modèle des sondages est celui de `eversolo_epingle_4382` (même
//! appareil) : sondage `k` à `1,003 k` s de l'armement, position réelle
//! `207,5 + 1,003 k` s, rapportée tronquée à la seconde et plafonnée à 237 s.
//! L'audio finit quand la position réelle atteint la durée de la file
//! (237,651 s). Le blanc CÔTÉ TUNE est l'écart entre cette fin et le
//! sondage qui envoie le `Play` de la piste suivante. Le démarrage propre à
//! l'A6 après `Play` (~1,9 s au journal du 05/10, du `Play` au premier
//! sondage à 0) s'y ajoute à l'oreille et ne dépend pas de Tune.
//!
//! Avant le correctif : `Next`, trois sondages de surveillance, relance —
//! `Play` au sondage 34, blanc côté Tune ≈ 3,9 s, à CHAQUE transition (le
//! journal : fin détectée 18:30:03.5, relance 18:30:06.5, son vers 08.5).

use super::*;

/// Octets tirés du flux armé à la fin (journal du 05/10 : 19 594 944).
const OCTETS_TIRES_0510: u64 = 19_594_944;
/// Période de sondage observée sur l'A6.
const PERIODE_S: f64 = 1.003;
/// Position réelle à l'armement (k = 0).
const POSITION_A_L_ARMEMENT_S: f64 = 207.5;

fn position_rapportee_ms(k: u32) -> u64 {
    (((POSITION_A_L_ARMEMENT_S + PERIODE_S * k as f64).floor() as u64) * 1000)
        .min(DUREE_RENDERER_MS)
}

/// Instant (s depuis l'armement) où l'audio de la piste finie s'arrête.
fn fin_de_l_audio_s() -> f64 {
    (DUREE_FILE_MS as f64 / 1000.0 - POSITION_A_L_ARMEMENT_S).max(0.0)
}

impl Banc {
    /// Le DMP-A6 du 05/10 : il tient la suivante, acquitte `Next` sans
    /// jamais l'exécuter.
    async fn l_a6_du_05_10(&self) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.poser_suivante_preparee(SuivantePreparee::Tenue);
        mock.bascule_honoree(false);
    }

    async fn nombre_de_next(&self) -> u64 {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.bascule_call_count()
    }

    /// Rejoue une fin de piste sur l'A6 et rend le blanc côté Tune (s) : de
    /// la fin de l'audio au sondage qui envoie le `Play` de la suivante.
    async fn mesurer_le_blanc(&mut self) -> f64 {
        let (flux, _) = self.armer().await;
        self.le_renderer_tire(&flux, OCTETS_TIRES_0510).await;
        let uri_finie = format!("http://tune.local:8888/stream/{}.wav", self.flux_finie);
        self.le_renderer_rapporte(Some(uri_finie)).await;

        let mut adoption_au_sondage: Option<u32> = None;
        for k in 1..=45u32 {
            {
                let reg = self.outputs.lock().await;
                let arc = reg.get(APPAREIL).unwrap();
                let sortie = arc.lock().await;
                let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
                mock.set_state(crate::outputs::traits::TransportState::Playing)
                    .await;
                mock.set_duration(DUREE_RENDERER_MS);
                mock.set_position(position_rapportee_ms(k));
            }
            let maintenant = Instant::now();
            let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
            match (adoption_au_sondage, ps.adoption_horloge.as_mut()) {
                // La surveillance vieillit au rythme des sondages.
                (Some(k0), Some(adoption)) => {
                    adoption.depuis =
                        maintenant - Duration::from_secs_f64(PERIODE_S * (k - k0) as f64);
                }
                _ => {
                    ps.track_started_at =
                        Some(maintenant - Duration::from_secs_f64(208.995 + PERIODE_S * k as f64));
                }
            }
            self.tic().await;
            if !self.play_complets().await.is_empty() {
                return PERIODE_S * k as f64 - fin_de_l_audio_s();
            }
            if adoption_au_sondage.is_none() && self.surveillance().is_some() {
                adoption_au_sondage = Some(k);
            }
        }
        panic!("aucun Play de la piste suivante en 45 sondages : la zone est restée muette");
    }
}

/// **Première transition** du processus : l'A6 n'a encore rien prouvé, Tune
/// lui demande `Next`. Le transport déclare au premier sondage qu'il ne l'a
/// pas exécuté : la relance part là, sans attendre les trois secondes, et
/// l'appareil est retenu.
#[tokio::test]
async fn premiere_transition_le_next_ignore_se_constate_au_premier_sondage() {
    let mut banc = Banc::monter().await;
    banc.l_a6_du_05_10().await;
    let blanc = banc.mesurer_le_blanc().await;
    eprintln!("blanc_cote_tune_premiere_transition_s={blanc:.2}");

    assert_eq!(
        banc.nombre_de_next().await,
        1,
        "prémisse : Next demandé une fois"
    );
    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "la relance joue la piste ADOPTÉE, aucune piste sautée"
    );
    assert!(
        blanc < 2.5,
        "#4382 : le transport de l'A6 dit dès le premier sondage que `Next` est ignoré \
         (CurrentURI = piste finie, NextURI = suivante toujours en attente) — la relance \
         doit partir là, pas après les {BASCULE_DELAI_SECS} s de surveillance ; blanc côté \
         Tune mesuré : {blanc:.2} s"
    );
    assert!(
        banc.poller
            .appareils_qui_ignorent_next
            .lock()
            .unwrap()
            .contains(APPAREIL),
        "#4382 : l'appareil qui a ignoré `Next` doit être retenu pour la transition suivante"
    );
}

/// **Transitions suivantes** : l'appareil est connu pour ignorer `Next`. Tune
/// ne le lui demande plus et relance dès la fin constatée, au sondage même
/// de l'épinglage — blanc côté Tune sous la seconde.
#[tokio::test]
async fn transition_suivante_relance_des_la_fin_sans_next() {
    let mut banc = Banc::monter().await;
    banc.l_a6_du_05_10().await;
    banc.poller
        .appareils_qui_ignorent_next
        .lock()
        .unwrap()
        .insert(APPAREIL.to_string());
    let blanc = banc.mesurer_le_blanc().await;
    eprintln!("blanc_cote_tune_transition_suivante_s={blanc:.2}");

    assert_eq!(
        banc.nombre_de_next().await,
        0,
        "#4382 : un appareil qui a déjà ignoré `Next` ne doit plus en recevoir"
    );
    assert_eq!(banc.play_complets().await, vec![ARMEE.to_string()]);
    assert!(
        blanc < 1.0,
        "#4382 : sur un A6 connu, le `Play` de la suivante doit partir dans la seconde \
         qui suit la fin de l'audio ; blanc côté Tune mesuré : {blanc:.2} s"
    );
}

/// Non-régression : un renderer qui EXÉCUTE `Next` n'est ni relancé ni
/// retenu.
#[tokio::test]
async fn un_renderer_qui_execute_next_n_est_pas_retenu() {
    let mut banc = Banc::monter().await;
    banc.l_a6_du_05_10().await;
    {
        let reg = banc.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.bascule_honoree(true);
    }
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES_0510).await;
    let uri_finie = format!("http://tune.local:8888/stream/{}.wav", banc.flux_finie);
    banc.le_renderer_rapporte(Some(uri_finie)).await;
    banc.la_fin_a_l_horloge().await;
    assert_eq!(banc.nombre_de_next().await, 1);
    assert!(banc.surveillance().is_some());

    // Le transport a pris la suivante : URI armée, position repartie.
    banc.renderer_a(1_000, 1).await;
    banc.tic().await;
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    assert!(
        !banc
            .poller
            .appareils_qui_ignorent_next
            .lock()
            .unwrap()
            .contains(APPAREIL)
    );
}
