//! #3208 — la période ALSA telle que le backend l'emploie, et la garde de
//! préchargement qu'elle commande.
//!
//! La décision pure et la garde de branchement (« aucun site n'écrit plus
//! `BufferSize::Default` ») sont dans `crate::audio::periode_alsa`, hors
//! `local-audio`, pour tourner dans la porte `test` de la CI — qui ne compile
//! pas cette fonctionnalité. Ce fichier-ci tient ce qui EXIGE cpal : la
//! traduction en `cpal::BufferSize`, et le fait qu'une configuration venue du
//! périphérique reçoive bien la période.
//!
//! ⚠️ Ce qu'aucune épreuve ne peut établir ici : qu'ALSA ACCEPTE la période
//! demandée. Il faut une carte son pour l'apprendre, et la machine de
//! compilation n'en a aucune (`/proc/asound` absent). C'est précisément l'objet
//! du levier : le relevé se fait sur du matériel.

use super::periode::{avec_periode, config_de_flux, garde_de_prechargement, taille_de_periode};

const MARQUEUR_ENFANT: &str = "TUNE_TEST_PERIODE_ALSA_LOCAL_ENFANT";
const CHEMIN_ENFANT: &str =
    "outputs::local::periode_alsa_i3208::le_backend_ouvre_avec_la_periode_armee";

fn cfg_avec(buffer_size: cpal::BufferSize) -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: 2,
        sample_rate: 44_100,
        buffer_size,
    }
}

/// Désarmé, RIEN ne change — le témoin obligatoire du ticket.
#[test]
fn sans_variable_le_pilote_choisit_comme_avant() {
    assert_eq!(taille_de_periode(), cpal::BufferSize::Default);
    assert_eq!(
        config_de_flux(2, 44_100).buffer_size,
        cpal::BufferSize::Default
    );
}

/// Une configuration VENUE DU PÉRIPHÉRIQUE n'est jamais écrasée quand aucune
/// période n'est armée : `avec_periode` ne remplace pas un `buffer_size` par
/// `Default`.
#[test]
fn sans_variable_une_config_du_peripherique_est_rendue_intacte() {
    let du_peripherique = cfg_avec(cpal::BufferSize::Fixed(512));
    assert_eq!(
        avec_periode(du_peripherique.clone()).buffer_size,
        du_peripherique.buffer_size
    );
}

/// La garde telle que le backend l'appelle : avec la configuration qu'il ouvre.
/// Sans période, le compte est celui d'hier au sample près — ~500 ms puis
/// ~200 ms, les deux comptes réellement employés.
#[test]
fn sans_periode_la_garde_reste_celle_d_hier() {
    let cfg = cfg_avec(cpal::BufferSize::Default);
    let cinq_cents_ms = 44_100 * 2 / 2;
    let deux_cents_ms = 44_100 * 2 / 5;
    assert_eq!(garde_de_prechargement(&cfg, cinq_cents_ms), cinq_cents_ms);
    assert_eq!(garde_de_prechargement(&cfg, deux_cents_ms), deux_cents_ms);
}

/// Avec une période, la garde n'est plus un nombre de millisecondes hérité de
/// CoreAudio : c'est un nombre de périodes.
#[test]
fn avec_une_periode_la_garde_se_compte_en_periodes() {
    let cfg = cfg_avec(cpal::BufferSize::Fixed(256));
    let cinq_cents_ms = 44_100 * 2 / 2;
    assert_eq!(
        garde_de_prechargement(&cfg, cinq_cents_ms),
        256 * 2 * crate::audio::periode_alsa::PERIODES_DE_GARDE
    );
    // Et jamais au-dessus du compte d'origine.
    let cfg_enorme = cfg_avec(cpal::BufferSize::Fixed(65_536));
    assert_eq!(
        garde_de_prechargement(&cfg_enorme, cinq_cents_ms),
        cinq_cents_ms
    );
}

/// Exécutée SEULE par l'épreuve parente, dans un processus armé.
#[test]
fn le_backend_ouvre_avec_la_periode_armee() {
    if std::env::var(MARQUEUR_ENFANT).is_err() {
        return;
    }
    let attendu: u32 = std::env::var(crate::audio::periode_alsa::VARIABLE_PERIODE)
        .expect("le parent arme la variable")
        .parse()
        .expect("le parent arme une valeur numérique");
    let attendu = if cfg!(target_os = "linux") {
        cpal::BufferSize::Fixed(attendu)
    } else {
        cpal::BufferSize::Default
    };
    // Les trois portes d'entrée de la production, dans l'ordre où elle les
    // emprunte : le littéral, la configuration venue du périphérique, la garde.
    assert_eq!(config_de_flux(2, 44_100).buffer_size, attendu);
    assert_eq!(
        avec_periode(cfg_avec(cpal::BufferSize::Default)).buffer_size,
        attendu
    );
    let garde = garde_de_prechargement(&config_de_flux(2, 44_100), 44_100);
    if cfg!(target_os = "linux") {
        assert_eq!(
            garde,
            256 * 2 * crate::audio::periode_alsa::PERIODES_DE_GARDE
        );
    } else {
        assert_eq!(garde, 44_100, "hors Linux, la garde ne bouge pas");
    }
}

/// La variable ne se lit qu'une fois par processus : on relance donc le VRAI
/// binaire d'épreuve avec l'environnement armé plutôt que d'appeler `set_var`,
/// qui casserait les épreuves voisines du même processus.
#[test]
fn la_periode_armee_traverse_la_decision_du_backend() {
    let exe = std::env::current_exe().expect("binaire d'épreuve");
    let sortie = std::process::Command::new(exe)
        .args(["--exact", CHEMIN_ENFANT, "--nocapture"])
        .env(MARQUEUR_ENFANT, "1")
        .env(crate::audio::periode_alsa::VARIABLE_PERIODE, "256")
        .output()
        .expect("relancer le binaire d'épreuve");
    let texte = format!(
        "{}{}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr)
    );
    assert!(
        sortie.status.success(),
        "la période armée n'atteint pas la décision du backend :\n{texte}"
    );
    assert!(
        texte.contains("1 passed"),
        "le filtre « {CHEMIN_ENFANT} » ne désigne plus l'épreuve enfant :\n{texte}"
    );
}
