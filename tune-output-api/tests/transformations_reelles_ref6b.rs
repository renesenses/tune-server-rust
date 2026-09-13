//! REF-6b de #2219 — **la sortie dit ce qu'elle a réellement fait.**
//!
//! `TransformationsReelles` réunit deux types qui existaient déjà —
//! `AudioSpec` (ce qui est entré) et `FormatOuvert` (ce que le périphérique a
//! ouvert) — et DÉDUIT les deux écarts, rééchantillonnage et adaptation de
//! canaux, au lieu de les ranger à part. Ces témoins gardent trois choses :
//! le type n'existe pas sans un `AudioSpec` valide, les écarts découlent des
//! formats, et une sortie qui ne publie rien rend `None`.
use tune_output_api::{AudioSpec, FormatOuvert, ProfondeurPcm, TransformationsReelles};

fn entree_96k_stereo() -> AudioSpec {
    AudioSpec::nouvelle(96_000, ProfondeurPcm::Entier24, 2).expect("format valide")
}

/// (c) L'unique constructeur exige un `AudioSpec`, et `AudioSpec::nouvelle`
/// refuse déjà le zéro canal : il n'existe donc aucun chemin qui construise
/// des transformations sur un format d'entrée invalide.
#[test]
fn ne_se_construit_pas_sur_un_audiospec_que_son_constructeur_refuse() {
    let refusee = AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 0);
    assert!(refusee.is_none(), "zéro canal doit être refusé en amont");
    let transformations = refusee.map(|entree| {
        TransformationsReelles::nouvelles(entree, FormatOuvert::new(44_100, 2), false)
    });
    assert!(
        transformations.is_none(),
        "aucune transformation ne peut être déclarée sur un format refusé"
    );
}

#[test]
fn les_ecarts_decoulent_des_deux_formats_et_ne_se_declarent_pas() {
    let entree = entree_96k_stereo();

    let intacte = TransformationsReelles::nouvelles(entree, FormatOuvert::new(96_000, 2), false);
    assert!(!intacte.reechantillonnage());
    assert!(!intacte.adaptation_canaux());
    assert!(!intacte.dsp_actif());

    let reechantillonnee =
        TransformationsReelles::nouvelles(entree, FormatOuvert::new(48_000, 2), false);
    assert!(reechantillonnee.reechantillonnage());
    assert!(!reechantillonnee.adaptation_canaux());

    let elargie = TransformationsReelles::nouvelles(entree, FormatOuvert::new(96_000, 8), true);
    assert!(!elargie.reechantillonnage());
    assert!(elargie.adaptation_canaux());
    assert!(elargie.dsp_actif());
}

#[test]
fn les_accesseurs_rendent_les_formats_tels_que_declares() {
    let entree = entree_96k_stereo();
    let ouvert = FormatOuvert::new(48_000, 2);
    let t = TransformationsReelles::nouvelles(entree, ouvert, true);
    assert_eq!(t.entree(), entree);
    assert_eq!(t.ouvert(), ouvert);
    // `Clone` + `PartialEq` : une copie dit la même chose.
    assert_eq!(t.clone(), t);
    assert_ne!(
        t,
        TransformationsReelles::nouvelles(entree, ouvert, false),
        "le DSP fait partie de l'identité"
    );
}

/// Le défaut du contrat : une sortie qui ne publie rien rend `None`, pas une
/// déclaration vide — le consommateur garde alors sa déduction.
#[test]
fn une_sortie_sans_mesure_rend_none_par_defaut_de_trait() {
    struct Muette;
    #[async_trait::async_trait]
    impl tune_output_api::OutputTarget for Muette {
        fn name(&self) -> &str {
            "muette"
        }
        fn device_id(&self) -> &str {
            "muette"
        }
        fn output_type(&self) -> &str {
            "muette"
        }
        async fn is_available(&self) -> bool {
            true
        }
        async fn pause(&self) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _position_ms: u64) -> Result<(), String> {
            Ok(())
        }
        async fn set_volume(&self, _volume: f64) -> Result<(), String> {
            Ok(())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Ok(())
        }
        async fn get_status(&self) -> Result<tune_output_api::OutputStatus, String> {
            Ok(tune_output_api::OutputStatus::default())
        }
    }
    assert!(tune_output_api::OutputTarget::transformations_reelles(&Muette).is_none());
}
