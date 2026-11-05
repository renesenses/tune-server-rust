//! #7616 — un étage de greffon natif TIERS sur une zone RÉSEAU.
//!
//! `greffon_natif_tiers_tests` ne couvre que la sortie locale. Ici, une VRAIE
//! bibliothèque native (le gain ×0,5 du banc #6522) est posée seule sur
//! l'étage casque d'une zone réseau, crossfeed intégré décoché, et l'on
//! mesure ce que le renderer reçoit, avec les montages de #2742 :
//!
//! - le relais des services, `StreamingDsp` (`load_streaming_dsp`), le porte ;
//! - cas 1 : un égaliseur ré-encode déjà la piste de la bibliothèque, l'étage
//!   tiers rejoint ce ré-encodage ;
//! - cas 2 : étage tiers seul, renderer qui accepte le LPCM (DLNA, OpenHome,
//!   Chromecast) : WAV progressif, l'étage appliqué au fil de l'eau ;
//! - cas 3 : étage tiers seul, renderer sans LPCM : la piste part telle
//!   quelle, sans effet — comme le crossfeed intégré.
//!
//! Le gain ×0,5 retire 6,02 dB à la voie gauche (sinus 1 kHz) : c'est la
//! signature de l'étage dans les octets servis.
use super::crossfeed_bibliotheque_reseau::tests::{
    DEVICE, Montage, Renderer, SR, Servi, armer_l_egaliseur, jouer, journal, monter,
    niveau_gauche_db_pcm, pcm_source,
};
use super::greffon_natif_tiers_tests::enregistrer_les_bibliotheques_de_test;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::mock::MockOutput;

/// Identifiant du greffon de test : un étage tiers ordinaire (ni
/// `crossfeed-…`, ni `crosstalk`).
const ID: &str = "gain-reseau-7616";

/// Gain du greffon : −6,02 dB.
const GAIN_DB: f64 = -6.0206;

/// Pose l'étage tiers sur la zone du montage et décoche le crossfeed intégré
/// que `monter` arme : l'étage tiers est seul sur l'étage casque. La
/// compensation de niveau est coupée, pour que la mesure ne lise que l'étage.
fn etage_tiers_seul(m: &Montage) {
    sans_crossfeed_ni_compensation(m);
    let s = SettingsRepo::with_backend(m.orch.db.clone());
    s.set(&format!("plugin_{ID}_installed"), "true").unwrap();
    s.set(&format!("plugin_{ID}_enabled"), "true").unwrap();
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(m.zone_id, ID),
        r#"{"enabled":true,"gain":0.5}"#,
    )
    .unwrap();
    assert!(
        m.orch.crossfeed_configure(m.zone_id).is_none(),
        "le crossfeed intégré doit être éteint"
    );
    let etage = m
        .orch
        .load_crossfeed_processor(m.zone_id, SR)
        .expect("l'étage casque porte l'étage tiers");
    assert_eq!(etage.decrire(), vec![format!("tiers:{ID}@{SR}")]);
}

/// Crossfeed intégré décoché (`monter` l'arme) et compensation de niveau
/// coupée : la mesure ne lit que les étages posés par le test.
fn sans_crossfeed_ni_compensation(m: &Montage) {
    let s = SettingsRepo::with_backend(m.orch.db.clone());
    s.set(
        &format!("zone_{}_crossfeed", m.zone_id),
        r#"{"enabled":false}"#,
    )
    .unwrap();
    s.set(&format!("zone_{}_level_compensation", m.zone_id), "false")
        .unwrap();
}

/// Niveau de la voie gauche de la source, la référence de chaque mesure.
fn niveau_source_db() -> f64 {
    niveau_gauche_db_pcm(&pcm_source(), 16)
}

/// Les octets servis portent l'étage tiers UNE fois : −6 dB sous
/// `reference_db` (le niveau servi sans l'étage), ni 0, ni −12.
fn porte_l_etage_tiers(cas: &str, s: &Servi, reference_db: f64) {
    let ecart = s.niveau_gauche_db - reference_db;
    assert!(
        (ecart - GAIN_DB).abs() < 1.0,
        "{cas} : la voie gauche servie doit perdre {GAIN_DB:.1} dB (un passage de \
         l'étage tiers), écart mesuré {ecart:.2} dB"
    );
    assert!(
        s.diaphonie_db < -60.0,
        "{cas} : aucun crossfeed n'est armé, diaphonie mesurée {:.1} dB",
        s.diaphonie_db
    );
}

#[tokio::test]
async fn greffon_natif_tiers_sur_zone_reseau_7616() {
    let (_ids, _dossier) = enregistrer_les_bibliotheques_de_test(&[ID]);

    // Relais des services : `StreamingDsp` porte l'étage tiers.
    {
        let m = monter(Renderer::AnnonceLeLpcm, None).await;
        etage_tiers_seul(&m);
        let mut chaine = m.orch.load_streaming_dsp(m.zone_id, None, SR, 2);
        assert!(chaine.is_active(), "le relais réseau ignore l'étage tiers");
        let mut pcm = pcm_source();
        chaine.process(&mut pcm, 16);
        let ecart = niveau_gauche_db_pcm(&pcm, 16) - niveau_source_db();
        println!("#7616 StreamingDsp : écart {ecart:.2} dB");
        assert!(
            (ecart - GAIN_DB).abs() < 0.1,
            "StreamingDsp : écart {ecart:.2} dB, attendu {GAIN_DB:.2} dB"
        );
    }

    // Cas 1 — DLNA, égaliseur armé : le ré-encodage existant porte l'étage.
    // L'égaliseur a sa propre réserve (pré-gain) : la référence est la même
    // zone, égaliseur seul, sans l'étage tiers.
    {
        let m = monter(Renderer::SansLpcm, None).await;
        sans_crossfeed_ni_compensation(&m);
        armer_l_egaliseur(&m);
        let egaliseur_seul = jouer(&m, None).await;
        journal("7616 cas 1 témoin (DLNA, égaliseur seul)", &egaliseur_seul);
        etage_tiers_seul(&m);
        let s = jouer(&m, None).await;
        journal("7616 cas 1 (DLNA, égaliseur + étage tiers)", &s);
        assert_eq!(s.mime, "audio/flac", "le format servi ne change pas");
        assert!(!s.progressif, "le chemin reste le fichier");
        porte_l_etage_tiers("cas 1 DLNA", &s, egaliseur_seul.niveau_gauche_db);
    }

    // Cas 2 — étage tiers seul, renderer qui accepte le LPCM : DLNA (absent
    // du registre, présumé capable), OpenHome (idem) et Chromecast (type
    // qui accepte le LPCM 16 bits sans sonde).
    for (type_sortie, enregistrer) in [("dlna", false), ("openhome", false), ("chromecast", true)] {
        let m = monter(Renderer::AnnonceLeLpcm, Some((type_sortie, DEVICE))).await;
        if enregistrer {
            m.orch.outputs.lock().await.register(Box::new(
                MockOutput::new(DEVICE, "C19").with_type(type_sortie),
            ));
        }
        etage_tiers_seul(&m);
        let s = jouer(&m, None).await;
        journal(&format!("7616 cas 2 ({type_sortie}, étage tiers seul)"), &s);
        assert_eq!(s.mime, "audio/wav", "{type_sortie}");
        assert!(s.progressif, "{type_sortie} : WAV progressif attendu");
        porte_l_etage_tiers(&format!("cas 2 {type_sortie}"), &s, niveau_source_db());
    }

    // Cas 3 — étage tiers seul, renderer DLNA sans LPCM : sans effet, la
    // piste part telle quelle.
    {
        let m = monter(Renderer::SansLpcm, None).await;
        etage_tiers_seul(&m);
        let s = jouer(&m, None).await;
        journal("7616 cas 3 (DLNA sans LPCM, étage tiers seul)", &s);
        assert_eq!(s.mime, "audio/flac");
        assert!(
            s.tel_quel,
            "cas 3 : les octets de la source, sans ré-encodage"
        );
        let ecart = s.niveau_gauche_db - niveau_source_db();
        assert!(
            ecart.abs() < 0.1,
            "cas 3 : l'étage tiers ne doit pas agir, écart {ecart:.2} dB"
        );
    }
}
