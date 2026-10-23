//! #6452 — un balayage des sorties audio bloqué sous Windows ne doit plus
//! accuser « le greffon ALSA/PipeWire ».
//!
//! Le journal de borderies.jean-pierre (fil 2248, Windows, hôte ASIO) portait
//! `local_audio_enumeration_timed_out … (greffon ALSA/PipeWire bloqué ?)`
//! toutes les 2 min 10 s : une couche qui n'existe pas sous Windows.

use super::*;

#[test]
fn sous_windows_le_soupcon_nomme_asio_ou_wasapi_et_jamais_alsa() {
    let s = soupcon_du_balayage_bloque("windows");
    assert!(s.contains("ASIO") && s.contains("WASAPI"), "{s}");
    assert!(!s.contains("ALSA") && !s.contains("PipeWire"), "{s}");
}

#[test]
fn sous_macos_le_soupcon_nomme_coreaudio() {
    let s = soupcon_du_balayage_bloque("macos");
    assert!(s.contains("CoreAudio"), "{s}");
    assert!(!s.contains("ALSA") && !s.contains("PipeWire"), "{s}");
}

#[test]
fn sous_linux_le_soupcon_reste_le_greffon_alsa_pipewire() {
    assert_eq!(
        soupcon_du_balayage_bloque("linux"),
        "greffon ALSA/PipeWire bloqué ?"
    );
}

/// Témoin de site : aucun des deux avertissements de balayage bloqué ne porte
/// plus le soupçon ALSA/PipeWire EN DUR ; tous deux le demandent à
/// [`soupcon_du_balayage_bloque`], pour le système qui tourne.
#[test]
fn les_deux_avertissements_demandent_le_soupcon_au_systeme_qui_tourne() {
    let source = include_str!("parc.rs");
    let prod = source
        .split("#[cfg(test)]")
        .next()
        .expect("le code de production précède les tests");
    // Le seul endroit où la phrase ALSA/PipeWire a le droit de figurer : le
    // bras Linux de la fonction elle-même.
    let debut = prod
        .find("pub(crate) fn soupcon_du_balayage_bloque(")
        .expect("la fonction existe");
    let fin = debut + prod[debut..].find("\n}\n").expect("fin de la fonction");
    let hors_fonction = format!("{}{}", &prod[..debut], &prod[fin..]);
    assert!(
        !hors_fonction.contains("greffon ALSA/PipeWire bloqué"),
        "un avertissement accuse encore ALSA/PipeWire en dur, quel que soit le système"
    );
    for evenement in [
        "\"local_audio_enumeration_timed_out",
        "\"local_audio_scan_in_progress_unresponsive_serving_cache",
    ] {
        let site = hors_fonction.find(evenement).expect(evenement);
        let avant = &hors_fonction[site.saturating_sub(300)..site];
        assert!(
            avant.contains("soupcon = soupcon_du_balayage_bloque(std::env::consts::OS)"),
            "{evenement} ne demande pas son soupçon au système qui tourne"
        );
    }
}
