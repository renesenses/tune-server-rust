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
//! | DSD brut forcé (« natif ») sans annonce DSD | armé (#8969, Eversolo DMP-A8, fil 2885) |
//! | idem, SCPD illisible | garde conservée |
//! | idem, règle #6477 apprise (lâche la piste au `SetNext`) | pas de `SetNext` |
//! | DSD non annoncé, zone hors « natif » | le DSD part en PCM, armé comme avant |
//! | annonce le DSD, SCPD illisible | garde conservée |
//! | annonce tout, ne suit pas (RS130) | deux transitions perdues, puis garde rendue (#9206) |
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

pub(super) fn annonce_dsd(oui: bool) -> DsdCapability {
    DsdCapability {
        supports_dsf: oui,
        supports_dff: false,
        dsf_mime: oui.then(|| "audio/x-dsd".to_string()),
        ..Default::default()
    }
}

impl Banc {
    /// La piste `ARMEE` devient un DSF ; la zone passe au mode DSD `mode` ;
    /// le renderer a annoncé `dsd` (GetProtocolInfo) et `suivante` (SCPD).
    pub(super) async fn monter_en_dsd(
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
    pub(super) async fn la_fenetre_d_armement(&mut self) {
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

/// ⭐ #8969 (fil 2885, Eversolo DMP-A8 du .18) — épreuve. Zone en DSD
/// « natif », renderer qui n'annonce PAS le DSD mais annonce `SetNext` : le
/// DSF part brut à chaque piste (`dsd_natif_sans_annonce_du_renderer`), la
/// suivante doit l'être aussi. Avant : `gapless_skipped_dsd_next_dlna
/// raison="dsd_non_annonce"` à chaque piste, puis `SetAVTransportURI` +
/// `Play` en fin de piste — un blanc à chaque changement de piste.
#[tokio::test]
async fn dsd_brut_force_sans_annonce_dsd_arme_la_suivante_8969() {
    let appareil = "uuid:banc-8969-natif-force";
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
        vec![ARMEE.to_string()],
        "#8969 : en « natif », le DSD part brut — sa suivante doit être armée"
    );
    assert!(banc.poll_states[&banc.zone_id].gapless_sent);
    assert!(
        banc.poll_states[&banc.zone_id]
            .gapless_dsd_skip_pos
            .is_none()
    );
    let url = banc.url_armee_6910().await.unwrap_or_default();
    assert!(
        url.to_lowercase().ends_with(".dsf"),
        "la suivante part en DSD brut, comme la piste en cours : {url}"
    );
}

/// #8969 — contre-épreuve : même renderer en « natif » sans annonce DSD, mais
/// SCPD illisible. Le « natif » ne lève que l'absence d'annonce DSD.
#[tokio::test]
async fn dsd_brut_force_scpd_illisible_garde_la_garde_8969() {
    let appareil = "uuid:banc-8969-natif-scpd-inconnu";
    let mut banc = Banc::monter_en_dsd(
        appareil,
        "native",
        annonce_dsd(false),
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

/// #8969 — contre-épreuve : renderer en « natif » sans annonce DSD dont la
/// règle #6477 est apprise (FiiO K17 : il lâche la piste en cours dès le
/// `SetNext`, #8868). Aucun `SetNext` : la fin de piste joue la suivante.
#[tokio::test]
async fn dsd_brut_force_regle_6477_apprise_pas_de_set_next_8969() {
    let appareil = "uuid:banc-8969-natif-k17";
    crate::outputs::dlna_repli_set_uri::memoriser_set_next_coupe_la_piste(appareil);
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
        "#6477 : plus aucun SetNext vers un renderer qui lâche la piste en cours"
    );
    assert!(!banc.poll_states[&banc.zone_id].gapless_sent);
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
/// passer au DSD armé. La suivante est relancée (jamais un silence). #9206 :
/// ce premier échec est NOTÉ, l'appareil garde le suivant DSD ; au deuxième,
/// il retrouve la garde — deux transitions perdues, pas une par piste.
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
            .dsd_suivant_tenu_sur_dlna(banc.zone_id, appareil, AnnonceSuivante::Annoncee)
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
        banc.orchestrator
            .dsd_suivant_tenu_sur_dlna(banc.zone_id, appareil, AnnonceSuivante::Annoncee)
            .await,
        "#9206 : un seul suivant DSD lâché ne retire plus le sans-blanc DSD à l'appareil"
    );
    // Le même constat à la transition suivante (celui que `tick` fait sur
    // `track_end_gap` avec `gapless_sent`).
    banc.orchestrator.retenir_dsd_suivant_lache(appareil, false);
    assert!(
        !banc
            .orchestrator
            .dsd_suivant_tenu_sur_dlna(banc.zone_id, appareil, AnnonceSuivante::Annoncee)
            .await,
        "#6910 : un renderer qui ne suit pas le DSD armé retrouve la garde de #402 \
         (au deuxième échec, #9206)"
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

/// #6335 — la ligne `gapless_skipped_dsd_next_dlna` dit POURQUOI le suivant
/// DSD n'est pas armé. Elle ne portait que la zone et le type MIME : un
/// renderer qui n'annonce pas le DSD et un SCPD que Tune n'a jamais lu
/// laissaient la même trace (fil 2567).
mod raison_du_refus_6335 {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Journal(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Journal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
        type Writer = Journal;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// La ligne de refus du banc, après la fenêtre d'armement.
    async fn ligne_de_refus(
        appareil: &str,
        mode: &str,
        dsd: DsdCapability,
        suivante: AnnonceSuivante,
    ) -> String {
        crate::journal_de_test::fiabiliser_la_capture();
        let journal = Journal::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _garde = tracing::subscriber::set_default(abonne);
        let mut banc = Banc::monter_en_dsd(appareil, mode, dsd, suivante).await;
        banc.la_fenetre_d_armement().await;
        assert_eq!(banc.armees().await, Vec::<String>::new());
        let texte = String::from_utf8_lossy(&journal.0.lock().unwrap()).into_owned();
        texte
            .lines()
            .find(|l| l.contains("gapless_skipped_dsd_next_dlna"))
            .unwrap_or_else(|| panic!("aucune ligne gapless_skipped_dsd_next_dlna :\n{texte}"))
            .to_string()
    }

    /// SCPD jamais lu : le refus vient de Tune, pas du renderer.
    #[tokio::test]
    async fn scpd_inconnu_nomme_setnext_inconnu() {
        let ligne = ligne_de_refus(
            "uuid:banc-6335-scpd-inconnu",
            "auto",
            annonce_dsd(true),
            AnnonceSuivante::Inconnue,
        )
        .await;
        assert!(
            ligne.contains("raison=\"setnext_inconnu\"")
                || ligne.contains("raison=setnext_inconnu"),
            "#6335 : le refus doit nommer le SCPD d'AVTransport inconnu : {ligne}"
        );
        assert!(ligne.contains("uuid:banc-6335-scpd-inconnu"), "{ligne}");
    }

    /// #8969 — DSD brut forcé (« natif ») sur un renderer qui ne l'annonce
    /// pas : l'absence d'annonce n'arrête plus rien, la ligne nomme la
    /// condition qui manque VRAIMENT (ici le SCPD).
    #[tokio::test]
    async fn natif_sans_annonce_nomme_la_vraie_condition_manquante() {
        let ligne = ligne_de_refus(
            "uuid:banc-6335-natif-force",
            "native",
            annonce_dsd(false),
            AnnonceSuivante::Inconnue,
        )
        .await;
        assert!(
            ligne.contains("setnext_inconnu") && !ligne.contains("dsd_non_annonce"),
            "#8969 : en « natif », le refus ne doit plus être dsd_non_annonce : {ligne}"
        );
    }
}
