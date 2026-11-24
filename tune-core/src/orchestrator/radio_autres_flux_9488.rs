//! #9488 — quand le flux par défaut d'une station ne démarre pas, essayer les
//! autres flux connus de la station (fil forum 3056).
//!
//! #8110 avait posé la mécanique : la route retient, au lancement, les autres
//! flux autorisés de la fiche ([`super::radio_replis_8110`]) et le décodeur
//! passe au suivant. Mais seulement sur un REFUS du diffuseur (403, 451 sur
//! HLS) : un flux par défaut en 503, une connexion refusée ou un flux vide
//! arrêtaient la lecture, alors que la fiche proposait un autre flux qui
//! marchait. Le contournement était de choisir soi-même ce flux comme flux
//! préféré de la station.
//!
//! Désormais tout échec AVANT le premier PCM (erreur HTTP, connexion refusée,
//! délai dépassé, réponse qui n'est pas de l'audio, flux vide) fait passer au
//! flux suivant, une fois chacun, jusqu'à épuisement. Ce qui ne change pas :
//! - une coupure EN COURS d'écoute garde la reconnexion sur le même flux (le
//!   WAV est déjà annoncé au format du premier flux) ;
//! - un refus « bit-perfect strict » de la zone n'est pas une panne du flux :
//!   il est rendu tel quel ;
//! - sans autre flux connu, tout est comme avant.
//!
//! L'ordre d'essai est celui que la route a retenu : le flux préféré de
//! l'auditeur part en premier (c'est lui que la lecture lance), puis le même
//! codec à la qualité la plus proche, puis le reste.
//!
//! Le changement est dit UNE fois, quand le flux de repli sert son premier
//! PCM, par le `zone.playback_error` non fatal que les autres avis utilisent
//! (code [`CODE_REPLI_DE_FLUX`]), et journalisé.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::radio_metadata_relais::flux_normalise;

/// Préfixe de l'échec « le flux s'est terminé avant le premier PCM », posé
/// seulement quand un autre flux de la station reste à essayer.
pub(crate) const RADIO_FLUX_VIDE: &str = "radio_flux_vide";

/// Code stable de l'avis, que le client traduit (`erreursServeur.<code>`).
pub const CODE_REPLI_DE_FLUX: &str = "radio_stream_fallback";

/// Au-delà, la mémoire des débits repart de zéro (elle ne sert qu'à nommer
/// les flux dans l'avis ; la perdre retire le débit de la phrase, rien de
/// plus).
const DEBITS_RETENUS_AU_PLUS: usize = 2048;

static DEBITS: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retient le débit annoncé par la fiche (kbit/s) de chaque flux d'une
/// station, pour que l'avis de repli puisse dire « flux 256 kbps ».
pub fn retenir_les_debits_radio(debits: Vec<(String, u32)>) {
    let Ok(mut memoire) = DEBITS.lock() else {
        return;
    };
    if memoire.len() + debits.len() > DEBITS_RETENUS_AU_PLUS {
        memoire.clear();
    }
    for (url, kbps) in debits {
        let cle = flux_normalise(&url);
        if !cle.is_empty() && kbps > 0 {
            memoire.insert(cle, kbps);
        }
    }
}

/// Le débit retenu pour ce flux, s'il est connu.
pub(crate) fn debit_retenu(url: &str) -> Option<u32> {
    DEBITS
        .lock()
        .ok()
        .and_then(|m| m.get(&flux_normalise(url)).copied())
}

/// Ce que l'avis dit : quel flux a échoué, lequel joue.
#[derive(Debug, Clone, PartialEq)]
pub struct RepliRadio {
    /// Le flux lancé (le préféré ou celui par défaut), le premier en échec.
    pub url_en_echec: String,
    /// Le flux qui joue.
    pub url_jouee: String,
    pub debit_en_echec: Option<u32>,
    pub debit_joue: Option<u32>,
    /// L'erreur du flux lancé, pour le journal.
    pub cause: String,
}

/// Appelée une seule fois, quand un flux de repli sert son premier PCM.
pub type AnnonceDeRepli = Box<dyn FnOnce(&RepliRadio) + Send>;

impl RepliRadio {
    pub(crate) fn nouveau(url_en_echec: &str, url_jouee: &str, cause: &str) -> Self {
        Self {
            url_en_echec: url_en_echec.to_string(),
            url_jouee: url_jouee.to_string(),
            debit_en_echec: debit_retenu(url_en_echec),
            debit_joue: debit_retenu(url_jouee),
            cause: cause.to_string(),
        }
    }

    /// La phrase de l'avis : « Flux 256 kbps indisponible, lecture du flux
    /// 128 kbps. » ; sans débit connu, la même phrase sans le chiffre.
    pub fn message(&self) -> String {
        let nomme = |d: Option<u32>| match d {
            Some(kbps) => format!("flux {kbps} kbps"),
            None => "flux".to_string(),
        };
        let en_echec = nomme(self.debit_en_echec);
        let joue = match self.debit_joue {
            Some(_) => format!("du {}", nomme(self.debit_joue)),
            None => "d'un autre flux de la station".to_string(),
        };
        let mut phrase = format!("{en_echec} indisponible, lecture {joue}.");
        if let Some(premiere) = phrase.get_mut(0..1) {
            premiere.make_ascii_uppercase();
        }
        phrase
    }

    /// La charge de `zone.playback_error` : non fatale, la zone joue.
    pub(crate) fn charge_utile(&self, zone_id: i64, station: &str) -> serde_json::Value {
        serde_json::json!({
            "zone_id": zone_id,
            "code": CODE_REPLI_DE_FLUX,
            "params": {
                "title": station,
                "failed_kbps": self.debit_en_echec,
                "playing_kbps": self.debit_joue,
            },
            "error": format!("« {station} » : {}", self.message()),
            // La zone JOUE, sur un autre flux de la station.
            "fatal": false,
        })
    }
}

/// L'annonce que posent les chemins de lecture radio : journal, puis avis
/// sur le bus.
pub(crate) fn annonce_sur_le_bus(
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
    zone_id: i64,
    station: String,
) -> AnnonceDeRepli {
    Box::new(move |repli: &RepliRadio| {
        tracing::warn!(
            zone_id,
            en_echec = %crate::confidentialite::masquer_url(&repli.url_en_echec),
            joue = %crate::confidentialite::masquer_url(&repli.url_jouee),
            debit_en_echec = ?repli.debit_en_echec,
            debit_joue = ?repli.debit_joue,
            cause = %repli.cause,
            "radio_repli_de_flux_annonce_9488"
        );
        if let Some(bus) = bus {
            bus.emit("zone.playback_error", repli.charge_utile(zone_id, &station));
        }
    })
}

/// Cet échec, survenu avant le premier PCM, justifie-t-il d'essayer un autre
/// flux de la station ? Tout échec du FLUX, oui ; un refus de la ZONE
/// (« bit-perfect strict », profondeur) non : il ne dépend pas du flux.
pub(crate) fn echec_qui_justifie_un_autre_flux(erreur: &str) -> bool {
    crate::audio::bitperfect_strict::RefusProfondeur::depuis_sentinelle(erreur).is_none()
        && crate::audio::bitperfect_strict::RefusBitPerfect::depuis_sentinelle(erreur).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_phrase_nomme_les_deux_debits_ou_s_en_passe() {
        let mut r = RepliRadio {
            url_en_echec: "http://a/256".into(),
            url_jouee: "http://a/128".into(),
            debit_en_echec: Some(256),
            debit_joue: Some(128),
            cause: "radio HTTP error: 503".into(),
        };
        assert_eq!(
            r.message(),
            "Flux 256 kbps indisponible, lecture du flux 128 kbps."
        );
        r.debit_joue = None;
        assert_eq!(
            r.message(),
            "Flux 256 kbps indisponible, lecture d'un autre flux de la station."
        );
        r.debit_en_echec = None;
        assert_eq!(
            r.message(),
            "Flux indisponible, lecture d'un autre flux de la station."
        );
        let charge = r.charge_utile(7, "Station");
        assert_eq!(charge["code"], CODE_REPLI_DE_FLUX);
        assert_eq!(charge["fatal"], false);
        assert_eq!(charge["zone_id"], 7);
    }

    #[test]
    fn les_debits_sont_retenus_par_flux_normalise() {
        retenir_les_debits_radio(vec![("https://D.example/9488/a.mp3".into(), 192)]);
        assert_eq!(debit_retenu("http://d.example/9488/a.mp3"), Some(192));
        assert_eq!(debit_retenu("http://d.example/9488/inconnu.mp3"), None);
    }

    #[test]
    fn un_echec_du_flux_justifie_un_autre_flux() {
        assert!(echec_qui_justifie_un_autre_flux(
            "radio HTTP error: 503 Service Unavailable"
        ));
        assert!(echec_qui_justifie_un_autre_flux(
            "radio HTTP fetch failed: connection refused"
        ));
        assert!(echec_qui_justifie_un_autre_flux(
            "radio_not_audio: text/html"
        ));
    }
}
