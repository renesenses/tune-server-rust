//! #5411 (fil 2031) — un renderer qui REJOUE la piste finie ne fait plus
//! sauter la suivante.
//!
//! ## Ce que le testeur décrit (Sony BD-P2100, zone DLNA, 0.9.167)
//!
//! « Sur la liste des morceaux, à la fin du morceau N, le morceau N+1 est en
//! surbrillance, mais le morceau N est rejoué. À la fin, le morceau N+2 est
//! joué. Le N+1 a été remplacé par le N. » Le N repart du début ; c'est
//! intermittent, en ordre d'album comme en aléatoire.
//!
//! ## Ce que fait le code
//!
//! Trois chemins avancent l'ÉCRAN sur la foi d'un mouvement du renderer, sans
//! renvoyer de `SetAVTransportURI` + `Play` :
//!
//! - la retombée de position (`gapless_position_reset_detected`) : la
//!   position tombe de plus de 30 s à moins de 5 s après un `SetNext` ;
//! - le départ après un arrêt dans la garde
//!   (`gapless_confirmed_advancing_metadata`) ;
//! - la surveillance d'une bascule `Next` (#3967) ou d'une adoption à
//!   l'horloge (#4173), confirmée dès que la position quitte sa valeur gelée.
//!
//! Un renderer qui repart de zéro sur la piste FINIE produit exactement ces
//! mouvements-là. Aucun des trois ne regardait l'URI qu'il rapporte : l'écran
//! passait à N+1, le renderer jouait N, et la file sautait N+1 — le récit du
//! testeur, mot pour mot.
//!
//! ## Ce que le banc ne prouve pas
//!
//! Que le Sony rapporte bien l'URI du N quand il le rejoue (sans URI, rien ne
//! change, voir les témoins de non-régression), ni POURQUOI il le rejoue. Le
//! fil ne porte aucun journal.

use super::*;

impl Banc {
    /// Ce que le renderer rapporte au prochain sondage, SANS toucher à l'état
    /// du sondeur : c'est le sondeur qui doit voir la retombée.
    async fn le_renderer_signale(
        &self,
        etat: crate::outputs::traits::TransportState,
        position_ms: u64,
    ) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(DUREE_RENDERER_MS);
        mock.set_position(position_ms);
    }

    fn uri_de_la_piste_finie(&self) -> String {
        format!("http://192.168.1.53:8888/stream/{}.wav", self.flux_finie)
    }

    async fn la_suivante_est_tenue_et_le_next_ignore(&self) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.poser_suivante_preparee(SuivantePreparee::Tenue);
        mock.bascule_honoree(false);
    }

    fn vieillir_la_surveillance(&mut self, secs: u64) {
        self.poll_states
            .get_mut(&self.zone_id)
            .unwrap()
            .adoption_horloge
            .as_mut()
            .expect("une surveillance doit être en cours")
            .depuis = Instant::now() - Duration::from_secs(secs);
    }

    /// Fin de piste N : le `SetNext` est parti, la position approche la fin.
    async fn fin_de_la_piste_finie(&mut self) {
        self.armer().await;
        self.renderer_a(236_000, 236).await;
    }
}

/// Le délai passé, ce que le repli doit laisser : UN `Play`, celui de la
/// piste affichée, et la file sur cette piste — N+1 n'est pas sauté.
async fn verifier_que_la_suivante_est_jouee(banc: &mut Banc) {
    let delai = banc
        .surveillance()
        .expect("la surveillance doit tenir jusqu'au délai")
        .delai_secs;
    banc.vieillir_la_surveillance(delai + 1);
    banc.renderer_a(9_000, delai + 1).await;
    banc.tic().await;
    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "le renderer rejoue la piste finie : le repli doit jouer la piste affichée"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
}

/// **Le témoin, chemin de la retombée de position.** Le renderer repart à
/// 1 s en nommant encore le flux de la piste finie. L'écran avance (comme
/// avant), mais l'avance est surveillée ; sans URI du flux armé dans le
/// délai, la piste affichée est jouée.
#[tokio::test]
async fn une_retombee_sur_la_piste_finie_ne_saute_pas_la_suivante() {
    let mut banc = Banc::monter().await;
    banc.fin_de_la_piste_finie().await;
    let uri_finie = banc.uri_de_la_piste_finie();
    banc.le_renderer_rapporte(Some(uri_finie)).await;
    banc.le_renderer_signale(crate::outputs::traits::TransportState::Playing, 1_000)
        .await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "l'écran avance comme avant"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    let surveillance = banc.surveillance().expect(
        "#5411 : le renderer nomme encore la piste finie — l'avance de l'écran doit \
         être surveillée, sinon la suivante est sautée",
    );
    assert_eq!(
        surveillance.preuve,
        decisions::EnchainementArme::RejeuDeLaPisteFinie
    );

    // Il continue de rejouer la piste finie : son mouvement ne confirme rien.
    banc.renderer_a(2_000, 1).await;
    banc.tic().await;
    assert!(
        banc.surveillance().is_some(),
        "rejouer la piste finie n'est pas un signe de vie"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());

    verifier_que_la_suivante_est_jouee(&mut banc).await;
}

/// **Le témoin, chemin du départ après un arrêt dans la garde.**
#[tokio::test]
async fn un_depart_apres_arret_sur_la_piste_finie_ne_saute_pas_la_suivante() {
    let mut banc = Banc::monter().await;
    banc.fin_de_la_piste_finie().await;
    let uri_finie = banc.uri_de_la_piste_finie();
    banc.le_renderer_rapporte(Some(uri_finie)).await;
    // Arrêt dans la garde, position encore haute : pas de retombée.
    banc.le_renderer_signale(crate::outputs::traits::TransportState::Stopped, 236_000)
        .await;
    banc.tic().await;
    assert!(
        banc.poll_states
            .get(&banc.zone_id)
            .unwrap()
            .gapless_advance_pending,
        "prémisse : l'arrêt dans la garde attend la confirmation du départ"
    );
    // Il repart... sur la piste finie.
    banc.le_renderer_signale(crate::outputs::traits::TransportState::Playing, 1_000)
        .await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    assert!(
        banc.surveillance().is_some(),
        "#5411 : le départ sur la piste finie doit être surveillé"
    );
    verifier_que_la_suivante_est_jouee(&mut banc).await;
}

/// **Le témoin, chemin de la bascule `Next` (#3967).** L'appareil acquitte
/// le `Next` et repart du début de la piste finie : la position quitte sa
/// valeur gelée, ce qui confirmait la bascule.
#[tokio::test]
async fn un_next_qui_rejoue_la_piste_finie_retombe_sur_le_repli() {
    let mut banc = Banc::monter().await;
    banc.la_suivante_est_tenue_et_le_next_ignore().await;
    let (flux, _) = banc.armer().await;
    banc.le_renderer_tire(&flux, OCTETS_TIRES).await;
    let uri_finie = banc.uri_de_la_piste_finie();
    banc.le_renderer_rapporte(Some(uri_finie)).await;
    banc.la_fin_a_l_horloge().await;
    assert_eq!(
        banc.surveillance().map(|s| s.preuve),
        Some(decisions::EnchainementArme::Bascule),
        "prémisse : la bascule a été demandée et se surveille"
    );

    // Il repart du début de la piste FINIE.
    banc.renderer_a(2_000, 2).await;
    banc.tic().await;
    assert!(
        banc.surveillance().is_some(),
        "#5411 : repartir sur la piste finie ne confirme pas la bascule"
    );
    assert_eq!(banc.play_complets().await, Vec::<String>::new());

    verifier_que_la_suivante_est_jouee(&mut banc).await;
}

/// **Non-régression.** La même retombée, mais le renderer nomme le flux
/// ARMÉ : c'est un enchaînement, l'avance d'avant, sans surveillance.
#[tokio::test]
async fn une_retombee_sur_le_flux_arme_avance_comme_avant() {
    let mut banc = Banc::monter().await;
    let (_, url) = banc.armer().await;
    banc.renderer_a(236_000, 236).await;
    banc.le_renderer_rapporte(Some(url)).await;
    banc.le_renderer_signale(crate::outputs::traits::TransportState::Playing, 1_000)
        .await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    assert!(banc.surveillance().is_none());
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
}

/// **Non-régression.** Le renderer ne rapporte aucune URI : rien ne le
/// soupçonne, l'avance d'avant, sans surveillance.
#[tokio::test]
async fn une_retombee_sans_uri_avance_comme_avant() {
    let mut banc = Banc::monter().await;
    banc.fin_de_la_piste_finie().await;
    banc.le_renderer_rapporte(None).await;
    banc.le_renderer_signale(crate::outputs::traits::TransportState::Playing, 1_000)
        .await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    assert!(banc.surveillance().is_none());
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
}

/// La décision pure.
#[test]
fn la_decision_pure() {
    use decisions::renderer_rejoue_la_piste_finie as rejoue;
    let finie = Some("aaaa-finie");
    let armee = Some("bbbb-armee");
    let uri_finie = Some("http://h:8888/stream/aaaa-finie.wav");
    let uri_armee = Some("http://h:8888/stream/bbbb-armee.wav");
    assert!(rejoue(uri_finie, finie, armee, false));
    assert!(!rejoue(uri_armee, finie, armee, false));
    assert!(
        !rejoue(None, finie, armee, false),
        "sans URI, rien n'est soupçonné"
    );
    assert!(!rejoue(Some(""), finie, armee, false));
    assert!(
        !rejoue(uri_finie, finie, armee, true),
        "répéter une piste, c'est la rejouer"
    );
    assert!(!rejoue(uri_finie, None, armee, false));

    use decisions::{SuiteAdoption, suite_de_l_adoption};
    assert_eq!(
        suite_de_l_adoption(2_000, 237_000, uri_finie, "bbbb-armee", finie, false, 1, 3),
        SuiteAdoption::EnAttente,
        "le mouvement d'un rejeu ne vaut pas signe de vie"
    );
    assert_eq!(
        suite_de_l_adoption(2_000, 237_000, uri_finie, "bbbb-armee", finie, false, 3, 3),
        SuiteAdoption::Infirmee
    );
    assert_eq!(
        suite_de_l_adoption(2_000, 237_000, uri_armee, "bbbb-armee", finie, false, 1, 3),
        SuiteAdoption::Confirmee
    );
}
