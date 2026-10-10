//! #6066 — une zone navigateur ne se dit « en écoute » que sur PREUVE.
//!
//! Écoute à distance sur iPhone (1.0.0-rc3, zone « Ce téléphone ») : le
//! journal portait `browser_playback_confirmed_announcing` alors que rien ne
//! jouait. AVPlayer avait seulement sondé le flux (`Range: bytes=0-1`, deux
//! octets, réponse rejetée ensuite). `confirmer_lecture_navigateur` tenait
//! pour preuve `stream_bytes_sent(stream_id) > 0` : n'importe quel octet,
//! tiré par n'importe quel client.
//!
//! Deux preuves désormais, et plus jamais « un octet » :
//!
//! 1. **Le lecteur du client qui joue la zone le dit** :
//!    `POST /zones/{id}/browser-playing` avec le `stream_id` qu'il joue et la
//!    position de SON horloge de lecture. Seule une position strictement
//!    positive confirme : un lecteur qui n'a pas encore avancé n'a rien fait
//!    entendre. Une fois qu'un lecteur a parlé pour une zone, le repli par
//!    les octets se tait pour elle : ce client sait dire quand il joue, on
//!    attend sa parole.
//! 2. **Repli pour les clients qui ne savent pas encore le dire** (web et iOS
//!    d'avant la route) : une durée d'audio réellement servie, au débit
//!    nominal du flux, et non une sonde de plage. Sans débit calculable
//!    (radio compressée, conteneur sans taille ni durée), un volume minimal
//!    d'octets.
//!
//! ⚠️ Le repli reste une présomption : un onglet qui précharge sans jouer
//! franchit le seuil. Seule la preuve n° 1 dit vraiment « ça joue ».

use std::collections::{HashMap, HashSet};

/// Audio servie en deçà de laquelle des octets tirés ne prouvent rien : une
/// sonde de plage (`bytes=0-1`, quelques Kio d'en-tête) en est très loin, un
/// lecteur qui démarre la dépasse en une fraction de seconde de tampon.
pub(crate) const AUDIO_MINIMAL_POUR_PREUVE_MS: u64 = 3_000;

/// Faute de débit nominal calculable, le volume d'octets qui tient lieu de
/// preuve. 64 Kio ≈ 4 s de MP3 à 128 kb/s, 8 s d'AAC à 64 kb/s.
pub(crate) const OCTETS_MINIMAUX_SANS_DEBIT: u64 = 64 * 1024;

/// Repli des clients qui ne confirment pas : ces octets tirés valent-ils une
/// écoute ? Jamais « un octet » (#6066).
pub(crate) fn octets_prouvent_une_ecoute(octets: u64, debit_nominal: Option<u64>) -> bool {
    match debit_nominal {
        Some(debit) if debit > 0 => {
            octets.saturating_mul(1_000) / debit >= AUDIO_MINIMAL_POUR_PREUVE_MS
        }
        _ => octets >= OCTETS_MINIMAUX_SANS_DEBIT,
    }
}

/// Ce que la route répond au lecteur qui signale qu'il joue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDuLecteur {
    /// L'annonce en attente pour ce flux vient de partir.
    Confirmee,
    /// Rien n'attend pour ce flux : déjà confirmé, ou lecture remplacée.
    RienEnAttente,
    /// Le lecteur n'a pas encore avancé (position nulle) : rien n'est
    /// annoncé, l'attente demeure.
    PasEncoreDeLecture,
}

impl SignalDuLecteur {
    pub fn code(self) -> &'static str {
        match self {
            Self::Confirmee => "confirmed",
            Self::RienEnAttente => "nothing_pending",
            Self::PasEncoreDeLecture => "not_playing_yet",
        }
    }
}

/// La mémoire des confirmations venues des lecteurs.
#[derive(Debug, Default)]
pub(crate) struct ConfirmationsDuLecteur {
    /// Zones dont le lecteur a déjà parlé : le repli par les octets s'y tait.
    zones_qui_confirment: HashSet<i64>,
    /// Flux que le lecteur de la zone a dit jouer, en attente d'être consommé
    /// par `confirmer_lecture_navigateur`.
    flux_confirmes: HashMap<i64, String>,
}

impl ConfirmationsDuLecteur {
    /// Le lecteur de `zone_id` a parlé. Il dit jouer `stream_id` si
    /// `joue` est vrai.
    pub(crate) fn noter(&mut self, zone_id: i64, stream_id: &str, joue: bool) {
        self.zones_qui_confirment.insert(zone_id);
        if joue {
            self.flux_confirmes.insert(zone_id, stream_id.to_string());
        }
    }

    /// Le lecteur a-t-il dit jouer CE flux ? Consomme la confirmation.
    pub(crate) fn prendre(&mut self, zone_id: i64, stream_id: &str) -> bool {
        if self.flux_confirmes.get(&zone_id).map(String::as_str) == Some(stream_id) {
            self.flux_confirmes.remove(&zone_id);
            return true;
        }
        false
    }

    /// Le repli par les octets est-il encore permis pour cette zone ?
    pub(crate) fn repli_permis(&self, zone_id: i64) -> bool {
        !self.zones_qui_confirment.contains(&zone_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas du terrain : AVPlayer sonde `bytes=0-1`, deux octets.
    #[test]
    fn une_sonde_de_plage_ne_prouve_aucune_ecoute() {
        assert!(!octets_prouvent_une_ecoute(2, None));
        assert!(!octets_prouvent_une_ecoute(2, Some(176_400)));
        assert!(!octets_prouvent_une_ecoute(1, Some(1)));
    }

    #[test]
    fn trois_secondes_d_audio_servies_valent_presomption() {
        // WAV 44,1 kHz / 16 bits / stéréo : 176 400 o/s.
        assert!(!octets_prouvent_une_ecoute(176_400 * 2, Some(176_400)));
        assert!(octets_prouvent_une_ecoute(176_400 * 3, Some(176_400)));
    }

    #[test]
    fn sans_debit_il_faut_un_volume_d_octets() {
        assert!(!octets_prouvent_une_ecoute(
            OCTETS_MINIMAUX_SANS_DEBIT - 1,
            None
        ));
        assert!(octets_prouvent_une_ecoute(OCTETS_MINIMAUX_SANS_DEBIT, None));
        assert!(octets_prouvent_une_ecoute(
            OCTETS_MINIMAUX_SANS_DEBIT,
            Some(0)
        ));
    }

    #[test]
    fn un_lecteur_qui_a_parle_fait_taire_le_repli_de_sa_seule_zone() {
        let mut c = ConfirmationsDuLecteur::default();
        assert!(c.repli_permis(7));
        c.noter(7, "s1", false);
        assert!(!c.repli_permis(7));
        assert!(c.repli_permis(8), "les autres zones gardent leur repli");
        assert!(!c.prendre(7, "s1"), "position nulle : rien n'est confirmé");
    }

    #[test]
    fn la_confirmation_porte_sur_un_flux_et_se_consomme() {
        let mut c = ConfirmationsDuLecteur::default();
        c.noter(7, "s1", true);
        assert!(!c.prendre(7, "autre"), "un autre flux ne se confirme pas");
        assert!(c.prendre(7, "s1"));
        assert!(!c.prendre(7, "s1"), "une confirmation, une annonce");
    }
}
