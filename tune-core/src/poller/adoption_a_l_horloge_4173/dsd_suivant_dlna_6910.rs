//! #6910 (fil 2412) — DSF en DLNA vers un renderer DSD natif : un blanc
//! entre The Wall pistes 2 et 3, deux pistes enchaînées sur le disque.
//!
//! ## Ce que montre le journal du testeur
//!
//! ```text
//! 18:59:48 gapless_skipped_dsd_next_dlna zone_id=29 mime=audio/x-dsd
//! 19:00:06 gapless_skipped_dsd_next_dlna zone_id=29 mime=audio/x-dsd
//! 19:00:18 dlna_dsd_reached_end … auto_next
//! ```
//!
//! La garde de #402 refusait d'armer TOUT suivant DSD sur DLNA, quel que soit
//! le renderer : le HiFi Rose RS130 acquittait `SetNextAVTransportURI` pour un
//! flux DSD sans jamais y passer, et l'album se coupait après la piste 1. Le
//! renderer du testeur annonce le DSD et `SetNextAVTransportURI` ; il n'a
//! jamais reçu la suivante.
//!
//! ## Ce que cloue ce banc (vrai `tick`, renderer simulé en `dlna`)
//!
//! | renderer | suivant DSD |
//! |---|---|
//! | annonce le DSD et `SetNext` | armé |
//! | DSD brut forcé (« natif ») sans annonce DSD | garde conservée |
//! | annonce le DSD, SCPD illisible | garde conservée |
//! | annonce tout, ne suit pas (RS130) | une transition perdue, puis garde rendue |
//! | zone en DoP | armé en WAV, comme avant |
//!
//! ## Ce que le banc ne prouve pas
//!
//! Qu'un renderer DSD natif réel bascule sur le DSD armé : le faux renderer
//! acquitte, il ne joue rien. La preuve sur l'appareil reste à faire.

use super::*;
use crate::outputs::dlna::DsdCapability;
use crate::outputs::traits::{AnnonceSuivante, TransportState};

const DSF: &str = "ref_dsd64_stereo.dsf";

fn annonce_dsd(oui: bool) -> DsdCapability {
    DsdCapability {
        supports_dsf: oui,
        supports_dff: false,
        dsf_mime: oui.then(|| "audio/x-dsd".to_string()),
    }
}

impl Banc {
    /// La piste `ARMEE` devient un DSF ; la zone passe au mode DSD `mode` ;
    /// le renderer a annoncé `dsd` (GetProtocolInfo) et `suivante` (SCPD).
    async fn monter_en_dsd(
        appareil: &str,
        mode: &str,
        dsd: DsdCapability,
        suivante: AnnonceSuivante,
    ) -> Self {
        let mut banc = Self::monter_sur(appareil).await;
        let f = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        std::fs::copy(
            format!("{}/tests/fixtures/dsd/{DSF}", env!("CARGO_MANIFEST_DIR")),
            f.path(),
        )
        .unwrap();
        let chemin = f.path().to_string_lossy().into_owned();
        banc.orchestrator
            .db
            .execute(
                "UPDATE tracks SET file_path = ?, format = 'dsf', sample_rate = 2822400, \
                 bit_depth = 1 WHERE title = ?",
                &[
                    &chemin as &dyn crate::db::backend::ToSqlValue,
                    &ARMEE as &dyn crate::db::backend::ToSqlValue,
                ],
            )
            .unwrap();
        banc._fichiers.push(f);
        ZoneRepo::with_backend(banc.orchestrator.db.clone())
            .update_dsd_mode(banc.zone_id, mode)
            .unwrap();
        banc.orchestrator
            .retenir_annonce_dsd_pour_banc(appareil, dsd)
            .await;
        {
            let reg = banc.outputs.lock().await;
            let arc = reg.get(appareil).unwrap();
            let sortie = arc.lock().await;
            sortie
                .as_any()
                .downcast_ref::<MockOutput>()
                .unwrap()
                .annoncer_la_suivante(suivante);
        }
        banc
    }

    /// La fenêtre d'armement s'ouvre : quelques sondages.
    async fn la_fenetre_d_armement(&mut self) {
        for (position, horloge) in [(207_000, 210), (210_000, 213)] {
            self.renderer_a(position, horloge).await;
            self.tic().await;
        }
    }

    async fn url_armee_6910(&self) -> Option<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .next_uri()
            .await
    }
}

/// ⭐ Le cas du fil 2412 : renderer DLNA qui annonce le DSD et
/// `SetNextAVTransportURI`, zone en « auto » : le DSF part brut ET il est
/// armé en suivante.
#[tokio::test]
async fn renderer_dsd_natif_qui_annonce_setnext_recoit_le_suivant_dsd() {
    let appareil = "uuid:banc-6910-dsd-natif";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "auto",
        annonce_dsd(true),
        AnnonceSuivante::Annoncee,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "#6910 : le renderer annonce le DSD et SetNextAVTransportURI — le suivant DSD \
         doit être armé (gapless_skipped_dsd_next_dlna l'écartait)"
    );
    assert!(banc.poll_states[&banc.zone_id].gapless_sent);
    assert!(
        banc.poll_states[&banc.zone_id]
            .gapless_dsd_skip_pos
            .is_none()
    );
    let url = banc.url_armee_6910().await.unwrap_or_default();
    assert!(
        !url.to_lowercase().ends_with(".wav"),
        "le suivant part en DSD brut, pas en DoP : {url}"
    );
}

/// Garde conservée : DSD brut forcé par le mode « natif » sur un renderer qui
/// n'annonce PAS le DSD (comme l'Eversolo DMP-A8). Rien ne dit qu'il tient
/// un suivant DSD.
#[tokio::test]
async fn dsd_brut_force_sans_annonce_dsd_garde_la_garde() {
    let appareil = "uuid:banc-6910-natif-force";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "native",
        annonce_dsd(false),
        AnnonceSuivante::Annoncee,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(
        banc.armees().await,
        Vec::<String>::new(),
        "#402 : sans annonce DSD du renderer, le suivant DSD n'est pas armé"
    );
    assert_eq!(
        banc.poll_states[&banc.zone_id].gapless_dsd_skip_pos,
        Some(1),
        "la position écartée est verrouillée, comme avant"
    );
}

/// Garde conservée : le renderer annonce le DSD, mais son SCPD n'a rien dit
/// de `SetNextAVTransportURI` (injoignable, illisible).
#[tokio::test]
async fn scpd_illisible_garde_la_garde() {
    let appareil = "uuid:banc-6910-scpd-inconnu";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "auto",
        annonce_dsd(true),
        AnnonceSuivante::Inconnue,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(banc.armees().await, Vec::<String>::new());
    assert_eq!(
        banc.poll_states[&banc.zone_id].gapless_dsd_skip_pos,
        Some(1)
    );
}

/// Le RS130 de #402 : il annonce tout, acquitte, puis s'arrête à zéro sans
/// passer au DSD armé. La suivante est relancée (jamais un silence), et
/// l'appareil retrouve la garde : une transition perdue, pas une par piste.
#[tokio::test]
async fn suivant_dsd_arme_non_suivi_rend_la_garde_a_l_appareil() {
    let appareil = "uuid:banc-6910-rs130";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "auto",
        annonce_dsd(true),
        AnnonceSuivante::Annoncee,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    assert!(
        banc.orchestrator
            .dsd_suivant_tenu_sur_dlna(appareil, AnnonceSuivante::Annoncee)
            .await
    );

    // La piste finie va au bout, le renderer s'arrête à zéro.
    for (position, horloge) in [(225_000, 228), (236_000, 239)] {
        banc.renderer_a(position, horloge).await;
        banc.tic().await;
    }
    let flux = banc.flux_finie.clone();
    let total = banc
        .orchestrator
        .streamer_total_bytes(&flux)
        .await
        .expect("la piste finie a une taille connue");
    banc.le_renderer_tire(&flux, total).await;
    let mut relancee = false;
    for _ in 0..20 {
        {
            let reg = banc.outputs.lock().await;
            let arc = reg.get(appareil).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(TransportState::Stopped).await;
            mock.set_position(0);
            mock.set_duration(0);
        }
        if let Some(ps) = banc.poll_states.get_mut(&banc.zone_id) {
            ps.track_loaded_at = Instant::now() - Duration::from_secs(600);
            if let Some(t) = ps.track_started_at {
                ps.track_started_at = Some(t - Duration::from_secs(1));
            }
        }
        banc.tic().await;
        if !banc.play_complets().await.is_empty() {
            relancee = true;
            break;
        }
    }
    assert!(
        relancee,
        "la suivante doit être relancée, jamais un silence"
    );
    assert_eq!(banc.play_complets().await, vec![ARMEE.to_string()]);
    assert!(
        !banc
            .orchestrator
            .dsd_suivant_tenu_sur_dlna(appareil, AnnonceSuivante::Annoncee)
            .await,
        "#6910 : un renderer qui n'a pas suivi le DSD armé retrouve la garde de #402"
    );
}

/// DoP inchangé : la zone en DoP envoie le DSF dans du WAV ; la garde ne
/// s'applique pas et la suivante est armée, même sans annonce DSD.
#[tokio::test]
async fn en_dop_la_suivante_part_en_wav_et_reste_armee() {
    let appareil = "uuid:banc-6910-dop";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "dop",
        annonce_dsd(false),
        AnnonceSuivante::Inconnue,
    )
    .await;
    banc.la_fenetre_d_armement().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    assert!(
        banc.poll_states[&banc.zone_id]
            .gapless_dsd_skip_pos
            .is_none()
    );
}
