//! #7293 (fil 2502, Tades, DirettaRenderer, Tune OS rc8.1, 28/10) — un
//! relevé manqué à l'instant de l'enchaînement ne cache plus le passage.
//!
//! ## Ce que le journal établit
//!
//! ```text
//! 11:16:22.947 gapless_arm_trace zone_id=29 output="dlna" armed=true reason="in_arming_window"
//!              reported_duration_ms=277000 queue_duration_ms=277386 position_ms=251000
//! 11:16:23.293 dlna_suivante_tenue device=C19 … gapless_next_set title=Meu Samba Torto
//! 11:16:48.532 stream_delivery_stall … (le renderer reprend le flux armé : il enchaîne)
//! 11:20:56.662 gapless_transition_detected zone_id=29 renderer_dur=316000 track_dur=277386
//!              peak_pos=273000
//! ```
//!
//! La piste finie (277 s) s'achève vers 11:16:48 ; le renderer passe à la
//! suivante (316 s) et repart de zéro. Tune ne le voit qu'à 11:20:56, soit
//! 248 s plus tard — exactement quand la position de la NOUVELLE piste
//! atteint `277 386 − 30 000` : la branche « près de la fin » du détecteur,
//! la seule qui restait ouverte. Ni `gapless_position_reset_detected` ni le
//! guet de fin de piste n'ont parlé : aucun relevé n'est tombé sous les 5 s
//! du début de la suivante. Le sondeur est une tâche pour toutes les zones
//! (neuf zones chez Tades, dont sept DLNA) : un seul appareil lent ailleurs
//! écarte deux relevés de 5 s et plus (#8006).
//!
//! Pendant ces quatre minutes, l'écran affiche la piste finie « en lecture »
//! et la préparation de la piste d'après n'est pas engagée : c'est la part de
//! « faisait semblant de jouer » que Tune se doit à lui-même.
//!
//! ## Le banc
//!
//! Celui de #4173 : vrai `tick`, vrai orchestrateur, renderer factice en
//! `dlna`. L'armement acquitté, le renderer tire le flux armé, puis un relevé
//! à 230 s. Le relevé suivant arrive DIX secondes plus tard (l'âge du relevé
//! est injecté, pas dormi) : le renderer joue la suivante depuis huit
//! secondes, sa durée est celle de la suivante.
//!
//! - avec le correctif : passage reconnu au premier relevé, écran sur la
//!   piste armée, flux adopté, zéro `Play` ;
//! - sans le correctif (contre-épreuve) : 8 s n'est pas « sous 5 s », l'écran
//!   reste sur la piste finie.

use super::*;

/// Durée que le renderer rapporte pour la piste armée (`ARMEE` : 212 000 en
/// file ; il arrondit à la seconde).
const DUREE_ARMEE_RENDERER_MS: u64 = 212_000;
/// Le dernier relevé pris sur la piste finie.
const DERNIER_RELEVE_MS: u64 = 230_000;
/// Ce que le renderer rapporte au relevé suivant : huit secondes dans la
/// suivante.
const POSITION_DANS_LA_SUIVANTE_MS: u64 = 8_000;
/// L'âge du relevé précédent : un appareil lent sur une autre zone a tenu
/// le sondeur pendant deux relevés de 5 s (#8006).
const RELEVE_MANQUE: Duration = Duration::from_secs(10);

impl Banc {
    /// Le renderer rapporte jouer la suivante à `position_ms`, avec SA durée,
    /// et le relevé précédent date de `age_du_releve`.
    async fn le_renderer_joue_la_suivante_depuis(
        &mut self,
        position_ms: u64,
        age_du_releve: Duration,
    ) {
        {
            let reg = self.outputs.lock().await;
            let arc = reg.get(&self.appareil).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(crate::outputs::traits::TransportState::Playing)
                .await;
            mock.set_duration(DUREE_ARMEE_RENDERER_MS);
            mock.set_position(position_ms);
        }
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.dernier_releve_de_position_at = Some(Instant::now() - age_du_releve);
    }
}

/// ⭐ #7293 — le relevé manqué de Tades : la suivante à 8 s après dix secondes
/// sans relevé, le passage est reconnu au premier sondage.
#[tokio::test]
async fn un_releve_manque_a_l_enchainement_ne_cache_plus_le_passage() {
    let mut banc = Banc::monter().await;
    let (flux, url) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;

    // 230 s : dernier relevé sur la piste finie, un tick ordinaire.
    banc.renderer_a(DERNIER_RELEVE_MS, 233).await;
    banc.tic().await;
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (0, FINIE),
        "à 230 s rien n'a encore enchaîné : l'écran reste sur la piste finie"
    );

    // Dix secondes sans relevé ; entre-temps le renderer a enchaîné et joue
    // la suivante depuis huit secondes.
    banc.le_renderer_rapporte(Some(url)).await;
    banc.le_renderer_joue_la_suivante_depuis(POSITION_DANS_LA_SUIVANTE_MS, RELEVE_MANQUE)
        .await;
    banc.tic().await;

    let (position, titre, stream_id) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "#7293 — après un relevé manqué de 10 s, une position à 8 s dans la suivante \
         est un enchaînement : l'écran doit passer sur la piste armée, pas rester \
         quatre minutes sur la piste finie « en lecture »"
    );
    assert_eq!(
        stream_id.as_deref(),
        Some(flux.as_str()),
        "la zone doit avoir ADOPTÉ le flux armé que le renderer tire"
    );
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "aucun `SetAVTransportURI` + `Play` : le renderer joue déjà la suivante"
    );
}

/// Contre-épreuve du prédicat : sans relevé manqué (un tick d'une seconde),
/// 8 s dans la piste n'est PAS une retombée — le comportement d'avant, au
/// chiffre près.
#[tokio::test]
async fn sans_releve_manque_huit_secondes_ne_sont_pas_une_retombee() {
    let mut banc = Banc::monter().await;
    let (_flux, url) = banc.armer().await;

    banc.renderer_a(DERNIER_RELEVE_MS, 233).await;
    banc.tic().await;

    banc.le_renderer_rapporte(Some(url)).await;
    banc.le_renderer_joue_la_suivante_depuis(POSITION_DANS_LA_SUIVANTE_MS, Duration::from_secs(1))
        .await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (0, FINIE),
        "à 1 Hz, 8 s n'est pas « près du début » : la marge d'avant tient"
    );
}
