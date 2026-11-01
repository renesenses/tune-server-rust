//! Un flux AES67 tel que l'utilisateur le déclare.
//!
//! Un émetteur AES67 ne découvre rien : les récepteurs ne s'annoncent pas
//! (§ 2.2 de l'étude). Une « Sortie AES67 » est donc un FLUX que l'on crée —
//! nom, groupe multicast, canaux, interface — et auquel on abonne ensuite son
//! récepteur (dans le logiciel de contrôle de ce récepteur).
//!
//! Les choix encore ouverts (§ 5 de l'étude) restent des réglages, sans
//! valeur imposée au-delà de l'étape E1 :
//! - la cadence est un champ ; seule 48 kHz est acceptée tant que le 96 kHz
//!   n'est pas tranché ;
//! - le volume logiciel est un interrupteur, éteint par défaut (volume fixe) ;
//! - le DSCP est réglable (AF41 = 34 par défaut, valeur de l'étude).

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

pub const PORT_PAR_DEFAUT: u16 = 5004;
pub const CADENCE_PAR_DEFAUT: u32 = 48_000;
pub const DUREE_PAQUET_US: u32 = 1_000;
pub const TYPE_CHARGE_PAR_DEFAUT: u8 = 98;
pub const TTL_PAR_DEFAUT: u8 = 16;
/// AF41, la classe média recommandée (§ 1.1 de l'étude).
pub const DSCP_PAR_DEFAUT: u8 = 34;
pub const CANAUX_MAX: u16 = 8;

/// Préfixe de l'identifiant d'appareil des sorties de ce greffon.
pub const PREFIXE_APPAREIL: &str = "aes67:";
/// `output_type` des zones de ce greffon.
pub const TYPE_DE_SORTIE: &str = "aes67";

fn port_par_defaut() -> u16 {
    PORT_PAR_DEFAUT
}
fn canaux_par_defaut() -> u16 {
    2
}
fn cadence_par_defaut() -> u32 {
    CADENCE_PAR_DEFAUT
}
fn ttl_par_defaut() -> u8 {
    TTL_PAR_DEFAUT
}
fn dscp_par_defaut() -> u8 {
    DSCP_PAR_DEFAUT
}
fn type_charge_par_defaut() -> u8 {
    TYPE_CHARGE_PAR_DEFAUT
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FluxAes67 {
    /// Identifiant stable : minuscules, chiffres, tirets. Sert à l'identifiant
    /// d'appareil `aes67:<id>`, donc à la zone.
    pub id: String,
    /// Le nom affiché par Tune et annoncé dans le SDP (« Tune - <nom> »).
    pub nom: String,
    /// Groupe multicast de destination (239.69.x.y : la plage usuelle des
    /// récepteurs AES67).
    pub adresse: Ipv4Addr,
    #[serde(default = "port_par_defaut")]
    pub port: u16,
    #[serde(default = "canaux_par_defaut")]
    pub canaux: u16,
    #[serde(default = "cadence_par_defaut")]
    pub cadence: u32,
    /// Interface d'émission (son adresse IPv4). `None` : celle de la route
    /// vers le groupe.
    #[serde(default)]
    pub interface: Option<Ipv4Addr>,
    #[serde(default = "ttl_par_defaut")]
    pub ttl: u8,
    #[serde(default = "dscp_par_defaut")]
    pub dscp: u8,
    /// Volume logiciel dans Tune. Éteint : le flux sort à pleine échelle et
    /// le récepteur (DAC, ampli) règle le volume.
    #[serde(default)]
    pub volume_logiciel: bool,
    #[serde(default = "type_charge_par_defaut")]
    pub type_charge: u8,
}

impl FluxAes67 {
    pub fn device_id(&self) -> String {
        format!("{PREFIXE_APPAREIL}{}", self.id)
    }

    /// Le nom annoncé dans le SDP.
    pub fn nom_de_session(&self) -> String {
        format!("Tune - {}", crate::sdp::nettoyer_texte(&self.nom))
    }

    /// Ce qu'un utilisateur peut enregistrer. Le banc d'essai construit des
    /// flux en unicast sur la boucle locale sans passer par ici.
    pub fn valider(&self) -> Result<(), String> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 32
            && self
                .id
                .bytes()
                .all(|o| o.is_ascii_lowercase() || o.is_ascii_digit() || o == b'-');
        if !id_ok {
            return Err("id : 1 à 32 caractères parmi a-z, 0-9 et « - »".into());
        }
        let nom = crate::sdp::nettoyer_texte(&self.nom);
        if nom.is_empty() || nom.chars().count() > 64 {
            return Err("nom : 1 à 64 caractères".into());
        }
        // Récepteurs AES67 visés : multicast seulement, dans la portée
        // administrative 239.0.0.0/8 (§ 1.2 de l'étude).
        if self.adresse.octets()[0] != 239 {
            return Err("adresse : un groupe multicast 239.x.y.z".into());
        }
        if self.adresse == *crate::sap::DESTINATION_SAP.ip() {
            return Err("adresse : 239.255.255.255 est réservée à SAP".into());
        }
        if self.port == 0 || self.port == crate::sap::DESTINATION_SAP.port() {
            return Err("port : réservé ou nul".into());
        }
        if self.canaux == 0 || self.canaux > CANAUX_MAX {
            return Err(format!("canaux : 1 à {CANAUX_MAX}"));
        }
        if self.cadence != CADENCE_PAR_DEFAUT {
            return Err("cadence : seule 48000 Hz est prise en charge pour l'instant".into());
        }
        if !(96..=127).contains(&self.type_charge) {
            return Err("type de charge : 96 à 127 (dynamique)".into());
        }
        if self.dscp > 63 {
            return Err("dscp : 0 à 63".into());
        }
        if self.ttl == 0 {
            return Err("ttl : au moins 1".into());
        }
        Ok(())
    }
}

/// Une liste de flux enregistrable : chaque flux valide, identifiants et
/// couples (groupe, port) uniques — AES67 veut un seul émetteur par groupe.
pub fn valider_la_liste(flux: &[FluxAes67]) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    let mut groupes = std::collections::HashSet::new();
    for f in flux {
        f.valider()
            .map_err(|e| format!("flux « {} » : {e}", f.id))?;
        if !ids.insert(f.id.clone()) {
            return Err(format!("flux « {} » : identifiant en double", f.id));
        }
        if !groupes.insert((f.adresse, f.port)) {
            return Err(format!(
                "flux « {} » : {}:{} déjà utilisé par un autre flux",
                f.id, f.adresse, f.port
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn salon() -> FluxAes67 {
        serde_json::from_value(serde_json::json!({
            "id": "salon",
            "nom": "Salon",
            "adresse": "239.69.1.10",
        }))
        .unwrap()
    }

    #[test]
    fn un_flux_minimal_prend_les_valeurs_de_l_etude() {
        let f = salon();
        assert_eq!(f.port, 5004);
        assert_eq!(f.canaux, 2);
        assert_eq!(f.cadence, 48_000);
        assert_eq!(f.ttl, 16);
        assert_eq!(f.dscp, 34);
        assert_eq!(f.type_charge, 98);
        assert!(!f.volume_logiciel, "volume fixe par défaut");
        assert_eq!(f.interface, None);
        assert!(f.valider().is_ok());
        assert_eq!(f.device_id(), "aes67:salon");
        assert_eq!(f.nom_de_session(), "Tune - Salon");
    }

    #[test]
    fn ce_qu_un_recepteur_aes67_refuserait_est_refuse_a_la_saisie() {
        let refus = |modifier: fn(&mut FluxAes67)| {
            let mut f = salon();
            modifier(&mut f);
            f.valider().unwrap_err()
        };
        assert!(refus(|f| f.adresse = Ipv4Addr::new(192, 168, 1, 2)).contains("239"));
        assert!(refus(|f| f.adresse = Ipv4Addr::new(224, 0, 1, 129)).contains("239"));
        assert!(refus(|f| f.adresse = Ipv4Addr::new(239, 255, 255, 255)).contains("SAP"));
        assert!(refus(|f| f.cadence = 44_100).contains("48000"));
        assert!(refus(|f| f.cadence = 96_000).contains("48000"));
        assert!(refus(|f| f.canaux = 9).contains("canaux"));
        assert!(refus(|f| f.canaux = 0).contains("canaux"));
        assert!(refus(|f| f.type_charge = 10).contains("type"));
        assert!(refus(|f| f.dscp = 64).contains("dscp"));
        assert!(refus(|f| f.id = "Salon".into()).contains("id"));
        assert!(refus(|f| f.nom = "\r\n".into()).contains("nom"));
        assert!(refus(|f| f.port = 9875).contains("port"));
    }

    #[test]
    fn deux_flux_ne_partagent_ni_id_ni_groupe() {
        let a = salon();
        let mut b = salon();
        b.id = "bureau".into();
        assert!(
            valider_la_liste(&[a.clone(), b.clone()])
                .unwrap_err()
                .contains("déjà")
        );
        b.adresse = Ipv4Addr::new(239, 69, 1, 11);
        assert!(valider_la_liste(&[a.clone(), b.clone()]).is_ok());
        b.id = "salon".into();
        assert!(valider_la_liste(&[a, b]).unwrap_err().contains("double"));
    }
}
