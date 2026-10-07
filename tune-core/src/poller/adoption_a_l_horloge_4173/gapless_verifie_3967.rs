//! #3967 — l'enchaînement VÉRIFIÉ, renderer par renderer : annoncé, refusé,
//! ignoré, passage manqué.
//!
//! Le même banc que #4173 (vrai `tick`, vrai orchestrateur, renderer simulé
//! en `dlna`), mais l'horloge avance d'une seconde par sondage, comme en
//! vrai : la fin de piste se joue sur les gardes réelles du sondeur, pas sur
//! une horloge injectée d'un bloc.
//!
//! ## Le défaut établi avant correctif
//!
//! Un renderer qui acquitte `SetNextAVTransportURI`, joue la piste jusqu'au
//! bout puis S'ARRÊTE (`STOPPED`, position 0) : la chute 236 s → 0 était prise
//! pour un passage. L'écran avançait sur la suivante, aucun `Play` ne partait,
//! et la zone restait affichée en lecture, muette — quarante sondages plus
//! tard, toujours rien. Une chute de position ne prouve un passage que si le
//! TRANSPORT joue.
//!
//! ## Les quatre conduites, et ce que chacune doit laisser
//!
//! | renderer | `SetNext` | fin de piste |
//! |---|---|---|
//! | annonce l'action et passe | posé | adopté, zéro `Play` |
//! | n'annonce pas l'action (SCPD) | jamais posé | `Play` de la suivante, sans attente |
//! | refuse (faute SOAP) | refusé, rien d'armé | `Play` de la suivante |
//! | acquitte et ignore | posé | `Play` de la suivante, jamais un silence |
//!
//! Le cinquième, le passage vu en retard (`TRANSITIONING` à zéro, puis
//! `PLAYING`) : adopté sans relance, même quand les deux pistes ont la même
//! durée et que seule la position peut le dire.

use super::*;
use crate::outputs::traits::{AnnonceSuivante, TransportState};

/// Au-delà, un repli qui n'est pas parti est un silence, pas une attente.
const SONDAGES_MAX: usize = 20;

impl Banc {
    async fn mock<R>(&self, f: impl FnOnce(&MockOutput) -> R) -> R {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        f(sortie.as_any().downcast_ref::<MockOutput>().unwrap())
    }

    async fn le_scpd_dit(&self, annonce: AnnonceSuivante) {
        self.mock(|m| m.annoncer_la_suivante(annonce)).await;
    }

    async fn le_renderer_refuse_le_set_next(&self) {
        self.mock(|m| m.refuser_le_set_next(Some("UPnPError 401 Invalid Action")))
            .await;
    }

    async fn set_next_envoyes(&self) -> usize {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_next_call_count().await
    }

    /// Ce que le renderer rapporte, SANS toucher à l'état du sondeur : c'est
    /// le sondeur qui doit le lire.
    async fn le_transport(&self, etat: TransportState, position_ms: u64, duree_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_position(position_ms);
        mock.set_duration(duree_ms);
    }

    /// Une seconde passe : les horloges du sondeur vieillissent d'autant, la
    /// piste a été chargée il y a longtemps (hors grâce de chargement).
    fn une_seconde_passe(&mut self) {
        // L'état de sondage est retiré quand la fin de piste agit, puis
        // recréé au sondage suivant : rien à vieillir entre les deux.
        let Some(ps) = self.poll_states.get_mut(&self.zone_id) else {
            return;
        };
        ps.track_loaded_at = Instant::now() - Duration::from_secs(600);
        if let Some(t) = ps.track_started_at {
            ps.track_started_at = Some(t - Duration::from_secs(1));
        }
    }

    /// La fenêtre d'armement s'ouvre (207 s), puis la piste joue jusqu'à
    /// 236 s, la position avançant normalement.
    async fn jouer_jusqu_a_la_fin(&mut self) {
        for (position, horloge) in [(207_000, 210), (225_000, 228), (236_000, 239)] {
            self.renderer_a(position, horloge).await;
            self.tic().await;
        }
        // Le renderer a tiré la piste finie jusqu'au dernier octet : sa fin
        // est une vraie fin, pas un décrochage sur un flux incomplet (#4645).
        let flux = self.flux_finie.clone();
        let total = self
            .orchestrator
            .streamer_total_bytes(&flux)
            .await
            .expect("la piste finie a une taille connue");
        self.le_renderer_tire(&flux, total).await;
    }

    /// Le renderer reste `STOPPED` à zéro ; rend le nombre de sondages avant
    /// le premier `Play`, ou `None` s'il n'est jamais parti.
    async fn sondages_avant_le_repli(&mut self) -> Option<usize> {
        for n in 1..=SONDAGES_MAX {
            self.le_transport(TransportState::Stopped, 0, 0).await;
            self.une_seconde_passe();
            self.tic().await;
            if !self.play_complets().await.is_empty() {
                return Some(n);
            }
        }
        None
    }

    async fn verifier_le_repli_sur_la_suivante(&self) {
        assert_eq!(
            self.play_complets().await,
            vec![ARMEE.to_string()],
            "le repli doit relancer la piste suivante, une fois"
        );
        let (position, titre, _) = self.ecran().await;
        assert_eq!((position, titre.as_str()), (1, ARMEE));
        assert_eq!(
            self.playback.get_state(self.zone_id).await.state,
            crate::playback::PlayState::Playing
        );
    }
}

// ── Annoncé ─────────────────────────────────────────────────────────────────

/// Le renderer annonce l'action, la suivante est posée dans la fenêtre, il
/// passe : position remise à zéro, `PLAYING`, URI du flux armé. Adopté sans
/// aucun `Play`.
#[tokio::test]
async fn annonce_la_suivante_est_preparee_et_le_passage_adopte_sans_play() {
    let mut banc = Banc::monter().await;
    banc.le_scpd_dit(AnnonceSuivante::Annoncee).await;
    banc.jouer_jusqu_a_la_fin().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "annoncée, la suivante doit être posée dans la fenêtre d'armement"
    );
    let url = banc.url_armee().await.unwrap();

    banc.mock(|m| m.set_position(0)).await;
    banc.le_renderer_rapporte(Some(url)).await;
    banc.le_transport(TransportState::Playing, 1_000, 212_000)
        .await;
    banc.une_seconde_passe();
    banc.tic().await;

    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
}

/// Contre-épreuve de l'annonce : le SCPD qui ne l'annonce PAS. Rien n'est
/// posé, à aucun sondage de la fenêtre.
#[tokio::test]
async fn non_annoncee_la_suivante_n_est_jamais_posee() {
    let mut banc = Banc::monter().await;
    banc.le_scpd_dit(AnnonceSuivante::NonAnnoncee).await;
    banc.jouer_jusqu_a_la_fin().await;
    assert_eq!(
        banc.set_next_envoyes().await,
        0,
        "un renderer qui n'annonce pas `SetNextAVTransportURI` ne doit pas le recevoir"
    );
    assert!(
        banc.orchestrator
            .flux_pre_arme(banc.zone_id)
            .await
            .is_none()
    );
}

/// Le SCPD illisible ou absent (`Inconnue`) garde l'armement d'avant.
#[tokio::test]
async fn annonce_inconnue_arme_comme_avant() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a_la_fin().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
}

/// Non annoncée, la fin de piste enchaîne par `Play` — et PLUS VITE que
/// pour un renderer qui acquitte puis ignore : aucune transition à guetter.
#[tokio::test]
async fn non_annoncee_la_fin_enchaine_sans_attendre_de_transition() {
    let mut non_annonce = Banc::monter().await;
    non_annonce.le_scpd_dit(AnnonceSuivante::NonAnnoncee).await;
    non_annonce.jouer_jusqu_a_la_fin().await;
    let sans_armement = non_annonce
        .sondages_avant_le_repli()
        .await
        .expect("la piste suivante doit partir");
    non_annonce.verifier_le_repli_sur_la_suivante().await;

    let mut ignore = Banc::monter().await;
    ignore.jouer_jusqu_a_la_fin().await;
    let avec_armement = ignore
        .sondages_avant_le_repli()
        .await
        .expect("la piste suivante doit partir");
    assert!(
        sans_armement < avec_armement,
        "sans `SetNext` posé, rien à attendre : {sans_armement} sondages, contre \
         {avec_armement} quand la suivante est posée puis ignorée"
    );
}

// ── Refusé ──────────────────────────────────────────────────────────────────

/// Le renderer répond une faute au `SetNext` : rien n'est armé, et la fin
/// de piste relance la suivante.
#[tokio::test]
async fn refuse_rien_n_est_arme_et_la_fin_relance_la_suivante() {
    let mut banc = Banc::monter().await;
    banc.le_renderer_refuse_le_set_next().await;
    banc.jouer_jusqu_a_la_fin().await;
    assert!(
        banc.set_next_envoyes().await >= 1,
        "la suivante a été proposée"
    );
    assert!(
        !banc.poll_states[&banc.zone_id].gapless_sent,
        "un `SetNext` refusé n'arme rien"
    );
    assert!(banc.sondages_avant_le_repli().await.is_some());
    banc.verifier_le_repli_sur_la_suivante().await;
}

/// Contre-épreuve du refus : le même renderer, qui accepte, est armé.
#[tokio::test]
async fn accepte_le_meme_renderer_est_arme() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a_la_fin().await;
    assert!(banc.poll_states[&banc.zone_id].gapless_sent);
    assert!(
        banc.orchestrator
            .flux_pre_arme(banc.zone_id)
            .await
            .is_some()
    );
}

// ── Ignoré ──────────────────────────────────────────────────────────────────

/// LE défaut : `SetNext` acquitté, renderer `STOPPED` à zéro. Avant le
/// correctif, l'écran avançait sans `Play` et la zone restait muette.
#[tokio::test]
async fn ignore_le_renderer_arrete_a_zero_recoit_le_play_de_la_suivante() {
    for annonce in [AnnonceSuivante::Annoncee, AnnonceSuivante::Inconnue] {
        let mut banc = Banc::monter().await;
        banc.le_scpd_dit(annonce).await;
        banc.jouer_jusqu_a_la_fin().await;
        assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);

        banc.le_transport(TransportState::Stopped, 0, 0).await;
        banc.une_seconde_passe();
        banc.tic().await;
        let (position, titre, _) = banc.ecran().await;
        assert_eq!(
            (position, titre.as_str()),
            (0, FINIE),
            "{annonce:?} : un renderer ARRÊTÉ à zéro n'a pas enchaîné — l'écran ne \
             doit pas avancer sans `Play`"
        );

        assert!(
            banc.sondages_avant_le_repli().await.is_some(),
            "{annonce:?} : en {SONDAGES_MAX} sondages, le repli doit relancer la suivante"
        );
        banc.verifier_le_repli_sur_la_suivante().await;
    }
}

/// Contre-épreuve de l'ignoré : le même renderer qui HONORE la suivante
/// (`PLAYING` près de zéro sur le flux armé) n'est jamais relancé.
#[tokio::test]
async fn honore_le_meme_renderer_n_est_jamais_relance() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a_la_fin().await;
    let url = banc.url_armee().await.unwrap();
    banc.le_renderer_rapporte(Some(url)).await;
    for position in [1_000, 2_000, 3_000, 4_000] {
        banc.le_transport(TransportState::Playing, position, 212_000)
            .await;
        banc.une_seconde_passe();
        banc.tic().await;
    }
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
}

// ── Passage vu en retard ────────────────────────────────────────────────────

/// Le passage réel, vu en deux temps : `TRANSITIONING` à zéro, puis `PLAYING`
/// à 1,5 s. Les deux pistes ont la MÊME durée : seule la position peut dire
/// le passage, et la chute différée doit encore le dire au sondage suivant.
#[tokio::test]
async fn passage_vu_en_retard_meme_duree_adopte_sans_play() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a_la_fin().await;
    let url = banc.url_armee().await.unwrap();

    banc.le_transport(TransportState::Transitioning, 0, DUREE_RENDERER_MS)
        .await;
    banc.une_seconde_passe();
    banc.tic().await;
    banc.le_renderer_rapporte(Some(url)).await;
    banc.le_transport(TransportState::Playing, 1_500, DUREE_RENDERER_MS)
        .await;
    banc.une_seconde_passe();
    banc.tic().await;

    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "un passage réel ne doit jamais être relancé"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "la chute différée doit être reconnue au premier `PLAYING`"
    );
}

/// Contre-épreuve du passage vu en retard : `TRANSITIONING` à zéro, puis
/// `STOPPED` — aucun passage. Pas d'avance sans `Play`, et le repli part.
#[tokio::test]
async fn transition_avortee_le_repli_relance_la_suivante() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a_la_fin().await;
    banc.le_transport(TransportState::Transitioning, 0, DUREE_RENDERER_MS)
        .await;
    banc.une_seconde_passe();
    banc.tic().await;
    let (position, _, _) = banc.ecran().await;
    assert_eq!(position, 0, "TRANSITIONING à zéro ne prouve pas le passage");
    assert!(banc.sondages_avant_le_repli().await.is_some());
    banc.verifier_le_repli_sur_la_suivante().await;
}

#[test]
fn seul_playing_atteste_une_chute() {
    use decisions::chute_a_differer_hors_lecture as differer;
    assert!(!differer(true, TransportState::Playing));
    for etat in [
        TransportState::Stopped,
        TransportState::Transitioning,
        TransportState::Paused,
    ] {
        assert!(differer(true, etat), "{etat:?}");
        assert!(!differer(false, etat), "sans chute, rien à différer");
    }
}
