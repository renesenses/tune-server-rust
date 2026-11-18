//! #8696 (hors sujet relevé dans le fil 2911) — un fichier multicanal vers
//! une sortie WASAPI exclusive qui a moins de voies.
//!
//! Un DSF 6 canaux envoyé vers un SMSL SU-8 (DAC stéréo) : l'ouverture
//! exclusive échouait sur « aucun format PCM accepté pour 6ch 176400Hz … »,
//! et la zone s'arrêtait sur ce message de pilote. Tune ne mélange pas les
//! voies d'office en exclusif (ce ne serait plus bit-perfect, et le choix du
//! mélange appartient à l'auditeur) : le refus le dit, et dit comment écouter
//! quand même (grille du greffon « Réaffectation des canaux », #6230).
//!
//! Le refus voyage comme celui du bit-perfect strict (#3973) : une
//! **sentinelle** (`code:voies_du_flux:voies_declarees`) dans le message
//! d'échec de la sortie, relue par le poller qui émet `zone.playback_error`
//! avec le `code`, les `params` (le client compose sa phrase dans sa langue,
//! `erreursServeur.<code>`) et la phrase française en repli.
//!
//! Le nombre de voies vient de la configuration que **Windows déclare** pour
//! la sortie (format du mixeur), pas d'une mesure du DAC : la phrase le dit
//! ainsi. Elle n'est émise que quand l'exclusif a refusé TOUS les formats au
//! nombre de voies du fichier.
//!
//! Hors FFI, pour être jugé par `cargo test` sur toutes les plateformes.

/// Le code stable du refus.
pub const CODE: &str = "exclusive_channels_exceed_device";

/// Un flux qui a plus de voies que la sortie n'en déclare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiesEnTrop {
    pub voies_du_flux: u32,
    pub voies_declarees: u32,
}

impl VoiesEnTrop {
    /// `Some` seulement quand la sortie déclare MOINS de voies que le flux.
    /// Voies déclarées inconnues ou suffisantes : le refus a une autre cause.
    pub fn constater(voies_du_flux: u32, voies_declarees: Option<u32>) -> Option<Self> {
        let declarees = voies_declarees.filter(|&d| d > 0 && d < voies_du_flux)?;
        Some(Self {
            voies_du_flux,
            voies_declarees: declarees,
        })
    }

    /// La sentinelle portée par le message d'échec de la sortie.
    pub fn sentinelle(&self) -> String {
        format!("{CODE}:{}:{}", self.voies_du_flux, self.voies_declarees)
    }

    /// Relit une sentinelle, où qu'elle commence dans le message.
    pub fn depuis_sentinelle(message: &str) -> Option<Self> {
        let debut = message.find(CODE)?;
        let reste = message[debut + CODE.len()..].strip_prefix(':')?;
        let mut parts = reste.split(':');
        let flux = parts.next()?.parse().ok()?;
        let declarees: String = parts
            .next()?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        Self::constater(flux, Some(declarees.parse().ok()?))
    }

    /// La phrase française, pour les clients qui ne connaissent pas le code.
    pub fn message_fr(&self) -> String {
        format!(
            "Cette sortie a refusé en mode exclusif les {flux} voies de ce fichier \
             (Windows ne lui en déclare que {decl}) : le mode exclusif ne mélange pas \
             les voies. Pour l'écouter sur cette sortie, réglez une grille {flux} → {decl} \
             dans le greffon « Réaffectation des canaux » de la zone",
            flux = self.voies_du_flux,
            decl = self.voies_declarees
        )
    }

    /// La charge utile de `zone.playback_error`.
    pub fn charge_utile(&self, zone_id: i64) -> serde_json::Value {
        serde_json::json!({
            "zone_id": zone_id,
            "error": self.message_fr(),
            "code": CODE,
            "params": {
                "source_channels": self.voies_du_flux,
                "device_channels": self.voies_declarees,
            },
            "fatal": true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_dsf_six_canaux_vers_un_dac_stereo_se_dit_en_clair() {
        let v = VoiesEnTrop::constater(6, Some(2)).unwrap();
        let m = v.message_fr();
        assert!(m.contains("les 6 voies de ce fichier"), "{m}");
        assert!(m.contains("Windows ne lui en déclare que 2"), "{m}");
        assert!(m.contains("grille 6 → 2 dans le greffon « Réaffectation des canaux »"));
        let charge = v.charge_utile(7);
        assert_eq!(charge["code"], CODE);
        assert_eq!(charge["params"]["source_channels"], 6);
        assert_eq!(charge["params"]["device_channels"], 2);
        assert_eq!(charge["fatal"], true);
    }

    /// La sentinelle survit à l'enrobage de `record_exclusive_open_failure`.
    #[test]
    fn la_sentinelle_fait_l_aller_retour_dans_le_message_d_echec() {
        let v = VoiesEnTrop::constater(6, Some(2)).unwrap();
        let enrobe = format!(
            "Sortie « SMSL SU-8 » : l'ouverture WASAPI exclusive a échoué ({}). Aucun repli",
            v.sentinelle()
        );
        assert_eq!(VoiesEnTrop::depuis_sentinelle(&enrobe), Some(v));
        assert_eq!(
            VoiesEnTrop::depuis_sentinelle("aucun format PCM accepté"),
            None
        );
        assert_eq!(
            VoiesEnTrop::depuis_sentinelle("exclusive_channels_exceed_device:x:2"),
            None
        );
    }

    /// Contre-épreuve : un refus qui n'a rien à voir avec le nombre de voies
    /// garde le message de négociation.
    #[test]
    fn assez_de_voies_ou_voies_inconnues_ne_changent_rien() {
        assert_eq!(VoiesEnTrop::constater(2, Some(2)), None);
        assert_eq!(VoiesEnTrop::constater(2, Some(8)), None);
        assert_eq!(VoiesEnTrop::constater(6, None), None);
        assert_eq!(VoiesEnTrop::constater(6, Some(0)), None);
    }

    /// Le branchement : la sortie pose la sentinelle, le poller la relit et
    /// émet la charge utile codée. Comparé sans blancs.
    #[test]
    fn la_sortie_pose_la_sentinelle_et_le_poller_la_relit() {
        let sortie: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        assert!(sortie.contains("VoiesEnTrop::constater(channels,voies_declarees"));
        assert!(sortie.contains(".map(|v|v.sentinelle())"));
        let poller: String = include_str!("../poller/tick.rs")
            .split_whitespace()
            .collect();
        assert!(poller.contains("VoiesEnTrop::depuis_sentinelle(&msg"));
        assert!(poller.contains("(None,Some(voies))=>voies.charge_utile(zone_id)"));
    }
}
