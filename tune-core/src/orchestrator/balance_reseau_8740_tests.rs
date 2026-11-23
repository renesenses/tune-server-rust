//! #8740 — témoins de bout en bout : la balance de la zone (greffon
//! « Réaffectation des canaux », matrice 2 → 2) sur une zone RÉSEAU.
//!
//! Même banc que le crossfeed (#2742) : une piste FLAC de la bibliothèque,
//! sinus 1 kHz à GAUCHE et silence à DROITE, jouée par la vraie résolution
//! (`resolve_local_track`) vers un renderer DLNA FACTICE ; on relit les octets
//! que la session sert. Le crossfeed posé par le montage est retiré : la
//! balance est le seul traitement. Aucun son réel.
//!
//! Signature de l'étage : le niveau RMS de la voie gauche servie.
//! −100 (tout à gauche) la laisse intacte, +100 (tout à droite) la rend muette,
//! +50 la baisse de 6 dB ; jamais elle ne monte.
use super::crossfeed_bibliotheque_reseau::tests::{
    Montage, Renderer, Servi, armer_l_egaliseur, jouer, journal, monter, niveau_gauche_db_pcm,
    pcm_source,
};
use crate::audio::balance_reseau::reglage_de_balance;
use crate::db::settings_repo::SettingsRepo;

/// Monte la zone avec la balance `b` pour SEUL traitement.
async fn monter_avec_balance(renderer: Renderer, sortie: Option<(&str, &str)>, b: i32) -> Montage {
    let m = monter(renderer, sortie).await;
    let s = SettingsRepo::with_backend(m.orch.db.clone());
    s.delete(&format!("zone_{}_crossfeed", m.zone_id)).unwrap();
    assert!(!m.orch.zone_has_active_crossfeed(m.zone_id));
    s.set("plugin_channel-remap_installed", "true").unwrap();
    s.set(
        &crate::audio::reaffectation_canaux::cle_de_zone(m.zone_id),
        &serde_json::to_string(&reglage_de_balance(b)).unwrap(),
    )
    .unwrap();
    m
}

fn niveau_source_db() -> f64 {
    niveau_gauche_db_pcm(&pcm_source(), 16)
}

fn gauche_intacte(cas: &str, s: &Servi) {
    let source = niveau_source_db();
    assert!(
        (s.niveau_gauche_db - source).abs() < 0.1,
        "{cas} : voie gauche attendue intacte ({source:.2} dBFS), mesurée {:.2}",
        s.niveau_gauche_db
    );
}

fn aucune_balance(cas: &str, s: &Servi) {
    gauche_intacte(cas, s);
    assert!(
        !s.annonce_reaffectation,
        "{cas} : aucune balance dans les octets, le flux ne doit pas en annoncer"
    );
}

/// Épreuve, renderer LPCM : +100 rend la gauche muette, −100 la laisse
/// intacte (aucune hausse), +50 la baisse de 6 dB. La balance seule part en
/// WAV progressif (jamais le fichier entier) et le flux l'annonce.
#[tokio::test]
async fn balance_mesuree_a_moins_100_plus_50_et_plus_100_sur_un_renderer_lpcm_8740() {
    let source = niveau_source_db();
    let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, 100).await;
    let s = jouer(&m, None).await;
    journal("balance +100 (LPCM)", &s);
    assert!(s.progressif, "balance seule : WAV progressif");
    assert_eq!(s.mime, "audio/wav");
    assert!(
        s.niveau_gauche_db < source - 90.0,
        "+100 : gauche muette attendue, mesurée {:.1} dBFS",
        s.niveau_gauche_db
    );
    assert!(
        s.annonce_reaffectation,
        "+100 : le flux doit annoncer la balance"
    );

    let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, 50).await;
    let s = jouer(&m, None).await;
    journal("balance +50 (LPCM)", &s);
    let ecart = s.niveau_gauche_db - source;
    assert!(
        (ecart + 6.02).abs() < 0.1,
        "+50 : gauche à −6 dB attendue, écart mesuré {ecart:.2} dB"
    );

    let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, -100).await;
    let s = jouer(&m, None).await;
    journal("balance −100 (LPCM)", &s);
    gauche_intacte("−100", &s);
    assert!(
        s.niveau_gauche_db <= source + 0.01,
        "−100 : aucune hausse de niveau"
    );
    assert!(s.annonce_reaffectation);
}

/// Contre-épreuve : balance au centre — aucun traitement, la piste part
/// telle quelle.
#[tokio::test]
async fn balance_au_centre_la_piste_part_telle_quelle_8740() {
    let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, 0).await;
    let s = jouer(&m, None).await;
    journal("balance 0", &s);
    assert!(s.tel_quel, "0 : les octets de la source");
    aucune_balance("0", &s);
}

/// Contre-épreuve : PURE et bit-perfect strict coupent la balance, la source
/// part telle quelle.
#[tokio::test]
async fn ni_en_pure_ni_en_bit_perfect_strict_8740() {
    for (cas, cle, valeur) in [
        ("PURE", "audiophile", r#"{"enabled":true}"#),
        ("bit-perfect strict", "strict_bitperfect", "true"),
    ] {
        let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, 100).await;
        SettingsRepo::with_backend(m.orch.db.clone())
            .set(&format!("zone_{}_{cle}", m.zone_id), valeur)
            .unwrap();
        let s = jouer(&m, None).await;
        journal(cas, &s);
        assert!(s.tel_quel, "{cas} : la source telle quelle");
        aucune_balance(cas, &s);
    }
}

/// Contre-épreuve : renderer SANS LPCM, balance seule — Tune sert le fichier
/// tel quel (pas de ré-encodage du fichier entier pour une balance) : aucune
/// balance, et le flux n'en annonce aucune (le chemin du signal non plus).
#[tokio::test]
async fn sans_lpcm_la_balance_seule_ne_force_pas_le_reencodage_8740() {
    let m = monter_avec_balance(Renderer::SansLpcm, None, 100).await;
    let s = jouer(&m, None).await;
    journal("balance seule, sans LPCM", &s);
    assert_eq!(s.mime, "audio/flac");
    assert!(s.tel_quel);
    aucune_balance("sans LPCM", &s);
}

/// Épreuve : quand un autre traitement ré-encode déjà la piste (égaliseur,
/// renderer sans LPCM), la balance entre dans ce ré-encodage.
#[tokio::test]
async fn le_reencodage_existant_porte_la_balance_8740() {
    let source = niveau_source_db();
    let m = monter_avec_balance(Renderer::SansLpcm, None, 100).await;
    armer_l_egaliseur(&m);
    let s = jouer(&m, None).await;
    journal("égaliseur + balance +100", &s);
    assert!(!s.progressif, "le chemin reste le fichier");
    assert!(
        s.niveau_gauche_db < source - 90.0,
        "ré-encodage : gauche muette attendue, mesurée {:.1} dBFS",
        s.niveau_gauche_db
    );
    assert!(s.annonce_reaffectation);
}

/// Contre-épreuve : une sortie LOCALE pose la matrice elle-même ; les octets
/// que le serveur lui sert ne portent jamais la balance (sinon elle
/// s'appliquerait deux fois).
#[tokio::test]
async fn une_sortie_locale_ne_recoit_jamais_la_balance_du_serveur_8740() {
    let m = monter_avec_balance(
        Renderer::AnnonceLeLpcm,
        Some(("local", "local:Casque")),
        100,
    )
    .await;
    armer_l_egaliseur(&m);
    let s = jouer(&m, Some("local:Casque")).await;
    journal("sortie locale", &s);
    assert!(!s.annonce_reaffectation);
    assert!(m.orch.reaffectation_reseau(m.zone_id).is_none());
}

/// Le chemin des SERVICES (`load_streaming_dsp`) : la balance y est chargée
/// pour une zone réseau stéréo, appliquée en DERNIER ; rien en PURE, rien
/// hors stéréo.
#[tokio::test]
async fn le_chemin_des_services_porte_la_balance_8740() {
    let m = monter_avec_balance(Renderer::AnnonceLeLpcm, None, 100).await;
    let rg = crate::audio::replaygain::PisteRg::AUCUNE;
    let mut dsp = m.orch.load_streaming_dsp(m.zone_id, rg, 44_100, 2);
    assert!(dsp.is_active() && dsp.reaffectation_executable());
    assert!(dsp.decrire().contains("balance@44100"), "{}", dsp.decrire());
    let mut pcm = pcm_source();
    dsp.process(&mut pcm, 16);
    assert!(niveau_gauche_db_pcm(&pcm, 16) < niveau_source_db() - 90.0);
    assert!(
        m.orch
            .load_streaming_dsp(m.zone_id, rg, 44_100, 6)
            .reaffectation
            .is_none(),
        "hors stéréo : aucune balance"
    );
    SettingsRepo::with_backend(m.orch.db.clone())
        .set(
            &format!("zone_{}_audiophile", m.zone_id),
            r#"{"enabled":true}"#,
        )
        .unwrap();
    let dsp = m.orch.load_streaming_dsp(m.zone_id, rg, 44_100, 2);
    assert!(!dsp.is_active(), "PURE : aucun étage");
}
