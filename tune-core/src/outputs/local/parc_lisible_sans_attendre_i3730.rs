//! #3730 — le parc énuméré se lit SANS attendre l'énumération en cours.
//!
//! ## Le fait mesuré
//!
//! [`super::SCAN_GUARD`] est pris par [`super::list_audio_devices_with_backend`]
//! et n'est relâché qu'à la fin de la fonction — donc il est tenu pendant TOUT
//! [`super::list_audio_devices_uncached`], c'est-à-dire pendant l'énumération
//! WASAPI complète. Ce fichier documente lui-même cette opération comme
//! sondant chaque point de sortie et capable d'invalider un flux en cours.
//!
//! [`super::cached_audio_devices`] prenait ce MÊME verrou, pour lire. Tant
//! qu'elle n'était appelée que par des tâches de fond, l'attente ne coûtait
//! rien à personne.
//!
//! ## Ce qui a changé, et pourquoi ça compte maintenant
//!
//! Depuis #3322 (livré en v0.9.143), elle est sur le chemin CHAUD de l'API.
//! `output_capabilities` (`tune-server/src/routes/zones.rs`) appelle
//! `canaux_des_peripheriques_locaux()`, qui appelle
//! [`super::cached_audio_devices`], pour CHAQUE charge utile de zone :
//!
//! * `GET /zones` — **une fois par zone**, et le client web l'interroge en
//!   boucle ;
//! * `GET /zones/{id}` ;
//! * la charge utile WebSocket ;
//! * la réponse de `POST /zones/{id}/play`, par `build_zone_json`.
//!
//! Ce sont des gestionnaires `async`, le verrou est bloquant, et il n'y a
//! aucun `spawn_blocking`. Sur Windows le rescan relance l'énumération toutes
//! les 120 s dès que rien ne joue — c'est-à-dire précisément quand l'auditeur
//! est sur le point d'appuyer sur Lire. Chaque requête en vol pendant ce
//! balayage gare un fil de l'ordonnanceur Tokio.
//!
//! ## Ce que cette épreuve garde, et ce qu'elle ne prétend pas
//!
//! Elle garde le CONTRAT : « lire le parc ne dépend pas de l'énumération ».
//! Elle ne mesure pas la durée d'un balayage WASAPI réel — la machine de
//! compilation n'en a pas — et ne prétend donc pas reproduire la panne d'un
//! testeur. Elle refuse le couplage qui la rend possible.
//!
//! ⚠️ UN SEUL témoin, et non trois : [`super::DERNIER_PARC`] et
//! [`super::SCAN_GUARD`] sont des états GLOBAUX. Trois `#[test]` que
//! `cargo test` exécute en parallèle se marcheraient dessus — l'un publierait
//! un parc pendant que l'autre en compte les lignes. L'ordre est donc imposé
//! ici, dans un seul corps.

use super::{AudioDevice, SCAN_GUARD, cached_audio_devices, publier_le_parc};
use std::sync::mpsc;
use std::time::Duration;

/// Le délai au-delà duquel on considère que le lecteur ATTEND l'énumération.
///
/// Généreux à dessein : une lecture qui ne fait que cloner un `Vec` rend en
/// microsecondes. Une demi-seconde ne peut être dépassée que si l'appel s'est
/// mis en attente derrière le verrou d'énumération — le défaut même.
const DELAI: Duration = Duration::from_millis(500);

fn un_peripherique(nom: &str) -> AudioDevice {
    AudioDevice {
        name: nom.to_string(),
        endpoint_id: format!("endpoint-{nom}"),
        is_default: true,
        max_channels: 2,
        sample_rates: vec![44_100, 48_000],
        sample_rates_measured: false,
        backend: "wasapi".to_string(),
        hardware_detail: None,
    }
}

/// 🔴 CONTRE-ÉPREUVE #3730 — pendant qu'une énumération tient
/// [`SCAN_GUARD`], lire le parc rend la main.
///
/// Rebrancher [`super::cached_audio_devices`] sur `SCAN_GUARD` (l'écriture
/// d'avant #3730) rend ce témoin ROUGE sur `recv_timeout` : le fil de lecture
/// reste garé derrière le verrou tenu ici et n'envoie jamais sa réponse.
#[test]
fn lire_le_parc_n_attend_pas_l_enumeration_en_cours() {
    // ── 1. Le geste que fait l'API pendant une énumération ──────────────
    // Cette moitié vient EN PREMIER, et c'est délibéré : elle doit être ce
    // qui tombe quand on rebranche la lecture sur `SCAN_GUARD`. Un témoin
    // qui échouerait d'abord sur un contenu ne dirait pas que le défaut est
    // une ATTENTE.
    publier_le_parc(&[un_peripherique("Haut-Parleurs")]);

    // Le verrou d'énumération est pris, et le RESTE : c'est l'exacte
    // situation d'un balayage WASAPI en cours.
    let garde = SCAN_GUARD.lock().unwrap_or_else(|e| e.into_inner());

    let (tx, rx) = mpsc::channel();
    let lecteur = std::thread::spawn(move || {
        // C'est CE geste que fait `output_capabilities` sur chaque charge
        // utile de zone.
        tx.send(cached_audio_devices()).ok();
    });

    let parc = rx.recv_timeout(DELAI).expect(
        "cached_audio_devices() doit rendre la main pendant une énumération : \
         elle est appelée par output_capabilities sur GET /zones (une fois par \
         zone), GET /zones/{id}, la charge utile WebSocket et la réponse de \
         POST /zones/{id}/play — attendre le balayage WASAPI y gare un fil Tokio",
    );

    assert_eq!(
        parc.len(),
        1,
        "le parc reste lisible, et complet, pendant l'énumération"
    );
    assert_eq!(parc[0].name, "Haut-Parleurs");

    drop(garde);
    lecteur.join().expect("le fil lecteur doit se terminer");

    // ── 2. Ce qui est publié est ce qui est relu ────────────────────────
    // Sans cette moitié, rendre une liste vide en dur passerait l'épreuve
    // de délai : un vert contre rien.
    publier_le_parc(&[un_peripherique("Topping D10s"), un_peripherique("Realtek")]);
    let parc = cached_audio_devices();
    assert_eq!(parc.len(), 2, "le parc publié doit être rendu tel quel");
    assert_eq!(parc[0].name, "Topping D10s");
    assert_eq!(parc[1].name, "Realtek");

    // Une publication REMPLACE, elle n'ajoute pas : sans quoi le parc
    // enflerait à chaque balayage.
    publier_le_parc(&[un_peripherique("Haut-Parleurs")]);
    assert_eq!(cached_audio_devices().len(), 1);
}
