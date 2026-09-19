//! #3973 — « Bit-perfect strict » : la seule règle qui décide si Tune joue tel
//! quel, joue en convertissant la fréquence, ou refuse.
//!
//! Décision de Bertrand (19/09, option 3) :
//!
//! 1. **Par défaut, jouer et le dire.** Quand la sortie ne lit pas la
//!    fréquence de la source, Tune convertit comme avant — mais la conversion
//!    est nommée (« 192 → 96 kHz ») et la lecture n'est plus annoncée
//!    bit-perfect ; en PURE, le mode apparaît dégradé.
//! 2. **Option par zone, désactivée par défaut** (`zone_{id}_strict_bitperfect`) :
//!    armée, Tune REFUSE la lecture et dit pourquoi (fréquence demandée,
//!    fréquence à laquelle la sortie tourne) au lieu de convertir.
//!
//! La conversion se décide en QUATRE endroits — ouverture cpal
//! (`outputs/local/backend.rs`), changement de cadence en cours de flux
//! (`outputs/local.rs`, enchaînement gapless), plafond de zone à la résolution
//! (`orchestrator/resolve_local.rs`, DoP compris) et décodage radio
//! (`orchestrator/radio.rs`). Les quatre appellent [`decision_bitperfect`] :
//! une garde posée sur un seul annoncerait un contrat qu'elle ne tient pas.
//!
//! Le refus voyage comme une SENTINELLE (`bitperfect_strict_refused:<demandée>:<sortie>`),
//! à l'image de `free_zone_cap:` : la phrase se compose là où l'on connaît la
//! langue (route HTTP, client), pas dans le cœur.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Le code stable du refus — champ `code` de `zone.playback_error`, champ
/// `error` de la réponse HTTP, préfixe de la sentinelle.
pub const CODE_REFUS: &str = "bitperfect_strict_refused";

/// La clé de réglage d'une zone. Présente et à `"true"` = armé ; absente =
/// défaut désarmé (la clé est supprimée à la désactivation, comme
/// `zone_{id}_mono_downmix`).
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_strict_bitperfect")
}

/// Le réglage « bit-perfect strict » de la zone. **Faux par défaut** : seule
/// la chaîne `"true"` l'arme, une clé absente ou illisible ne refuse jamais
/// rien à personne.
pub fn zone_enabled(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Pourquoi la lecture est refusée : la fréquence demandée et celle à laquelle
/// la sortie (ou la zone) tourne réellement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusBitPerfect {
    /// Fréquence de la source, en Hz.
    pub demandee_hz: u32,
    /// Fréquence que la sortie accepte — celle vers laquelle Tune aurait
    /// converti.
    pub sortie_hz: u32,
}

impl RefusBitPerfect {
    /// `bitperfect_strict_refused:192000:96000`.
    pub fn sentinelle(&self) -> String {
        format!("{CODE_REFUS}:{}:{}", self.demandee_hz, self.sortie_hz)
    }

    /// Relit une sentinelle, où qu'elle commence dans le message (un appelant
    /// a pu la préfixer). `None` pour tout autre message.
    pub fn depuis_sentinelle(message: &str) -> Option<Self> {
        let debut = message.find(CODE_REFUS)?;
        let reste = message[debut + CODE_REFUS.len()..].strip_prefix(':')?;
        let mut parts = reste.split(':');
        let demandee_hz = parts.next()?.trim().parse().ok()?;
        let sortie_hz = parts
            .next()?
            .trim()
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()?;
        Some(Self {
            demandee_hz,
            sortie_hz,
        })
    }

    /// La phrase française — celle du journal et de l'événement WebSocket ;
    /// la route HTTP et le client la traduisent à partir du code.
    pub fn message_fr(&self) -> String {
        format!(
            "Bit-perfect strict : lecture refusée — la sortie ne lit pas le {} kHz sans conversion (elle tourne à {} kHz). Désactivez « Bit-perfect strict » dans les réglages de la zone pour jouer avec conversion.",
            khz(self.demandee_hz),
            khz(self.sortie_hz)
        )
    }
}

/// `192000` → `192`, `44100` → `44,1`, `22050` → `22,05` (virgule décimale).
pub fn khz(hz: u32) -> String {
    let entier = hz / 1000;
    let reste = hz % 1000;
    if reste == 0 {
        return entier.to_string();
    }
    let dec = format!("{reste:03}");
    format!("{entier},{}", dec.trim_end_matches('0'))
}

/// La charge utile de `zone.playback_error` pour un refus : le `code` stable,
/// les deux fréquences (le client compose sa phrase dans sa langue), la phrase
/// française pour les clients qui ne connaissent pas le code, et `fatal` —
/// sans lui la fenêtre de grâce du client avale le message (#1146, #3737).
pub fn charge_utile_de_refus(zone_id: i64, refus: &RefusBitPerfect) -> serde_json::Value {
    serde_json::json!({
        "zone_id": zone_id,
        "error": refus.message_fr(),
        "code": CODE_REFUS,
        "requested_hz": refus.demandee_hz,
        "device_hz": refus.sortie_hz,
        "fatal": true,
    })
}

/// Ce que la règle décide pour UNE ouverture ou UN changement de cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionBitPerfect {
    /// La sortie lit la fréquence de la source : rien à convertir.
    Jouer,
    /// Défaut : convertir, et le dire (chemin du signal, PURE dégradé).
    JouerConverti { de: u32, vers: u32 },
    /// Bit-perfect strict : ne pas jouer, et dire pourquoi.
    Refuser { cause: RefusBitPerfect },
}

impl DecisionBitPerfect {
    /// Le refus, s'il y en a un — la forme que les sites d'appel consomment.
    pub fn refus(self) -> Option<RefusBitPerfect> {
        match self {
            DecisionBitPerfect::Refuser { cause } => Some(cause),
            _ => None,
        }
    }
}

/// LA règle. `demandee` : la fréquence de la source ; `sortie` : celle à
/// laquelle la sortie tournera (cadence du périphérique, plafond de zone,
/// cadence sûre du renderer) ; `strict` : le réglage de la zone.
///
/// Fonction pure : aucun site ne rejuge l'écart lui-même.
pub fn decision_bitperfect(demandee: u32, sortie: u32, strict: bool) -> DecisionBitPerfect {
    if demandee == sortie || demandee == 0 || sortie == 0 {
        // `0` = inconnu : rien vers quoi convertir, rien à refuser.
        return DecisionBitPerfect::Jouer;
    }
    if strict {
        DecisionBitPerfect::Refuser {
            cause: RefusBitPerfect {
                demandee_hz: demandee,
                sortie_hz: sortie,
            },
        }
    } else {
        DecisionBitPerfect::JouerConverti {
            de: demandee,
            vers: sortie,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meme_frequence_joue_tel_quel_strict_ou_non() {
        assert_eq!(
            decision_bitperfect(96_000, 96_000, true),
            DecisionBitPerfect::Jouer
        );
        assert_eq!(
            decision_bitperfect(96_000, 96_000, false),
            DecisionBitPerfect::Jouer
        );
    }

    #[test]
    fn par_defaut_la_conversion_est_jouee_et_nommee() {
        assert_eq!(
            decision_bitperfect(192_000, 96_000, false),
            DecisionBitPerfect::JouerConverti {
                de: 192_000,
                vers: 96_000
            }
        );
    }

    #[test]
    fn strict_refuse_en_nommant_les_deux_frequences() {
        let refus = decision_bitperfect(192_000, 96_000, true)
            .refus()
            .expect("strict refuse");
        assert_eq!(refus.demandee_hz, 192_000);
        assert_eq!(refus.sortie_hz, 96_000);
        assert!(refus.message_fr().contains("192 kHz"));
        assert!(refus.message_fr().contains("96 kHz"));
    }

    #[test]
    fn une_frequence_inconnue_ne_refuse_rien() {
        assert_eq!(
            decision_bitperfect(0, 96_000, true),
            DecisionBitPerfect::Jouer
        );
        assert_eq!(
            decision_bitperfect(96_000, 0, true),
            DecisionBitPerfect::Jouer
        );
    }

    #[test]
    fn la_sentinelle_fait_l_aller_retour() {
        let r = RefusBitPerfect {
            demandee_hz: 352_800,
            sortie_hz: 96_000,
        };
        assert_eq!(r.sentinelle(), "bitperfect_strict_refused:352800:96000");
        assert_eq!(RefusBitPerfect::depuis_sentinelle(&r.sentinelle()), Some(r));
        assert_eq!(
            RefusBitPerfect::depuis_sentinelle(&format!("Sortie « DAC » : {}.", r.sentinelle())),
            Some(r)
        );
        assert_eq!(
            RefusBitPerfect::depuis_sentinelle("free_zone_cap:1:2"),
            None
        );
    }

    #[test]
    fn khz_ecrit_la_virgule_decimale() {
        assert_eq!(khz(192_000), "192");
        assert_eq!(khz(44_100), "44,1");
        assert_eq!(khz(22_050), "22,05");
        assert_eq!(khz(88_200), "88,2");
    }
}
