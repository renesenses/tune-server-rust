//! #7265 — un DSD que la sortie WASAPI ne peut pas porter en DoP part en PCM,
//! au lieu d'être refusé.
//!
//! Constat (JeanPitre, Windows, fil « Juste une suggestion… », 27/10/2026) :
//! privé d'ASIO (#6452), il ne lit plus aucun DSD en WASAPI. Sa zone garde le
//! mode DSD « natif » (ou « dop ») de l'époque ASIO. Sur une sortie locale ce
//! mode demande un porteur DoP (`transport_dsd`), et deux chemins le
//! refusaient ensuite, APRÈS que l'orchestrateur a choisi le DoP :
//!
//! - **WASAPI partagé** : la sortie s'ouvre à la cadence du mixeur de Windows
//!   (176,4 kHz chez lui). Un DoP à une autre cadence (DSD128 → 352,8 kHz) ne
//!   survit pas au rééchantillonnage, et le fil de lecture refuse la piste
//!   (`DopRuptureChemin`, `outputs/local.rs`, `refuser_le_porteur_dop`) ;
//! - **WASAPI exclusif** : le transport ouvre AU FORMAT DE LA SOURCE ou
//!   refuse (`record_exclusive_open_failure`, « aucun repli ») ; un pilote
//!   qui ne prend pas 24 bits à la cadence DoP arrête donc la zone.
//!
//! Dans les deux cas la piste ne jouait pas, alors qu'une conversion DSD →
//! PCM — ce que fait le mode « auto » — l'aurait fait entendre. La règle
//! devient : **on demande d'abord à la sortie si elle peut porter ce DoP**
//! (format du mixeur en partagé, `IsFormatSupported` en exclusif). Si elle
//! répond non, la piste part en PCM haute résolution, le journal le dit
//! (`dsd_dop_ecarte_converti_en_pcm`) et le chemin du signal en donne la
//! raison (`code = "dsd_dop_ecarte"`). Si elle ne sait pas répondre
//! (pilote occupé, autre plateforme), rien ne change : le DoP est tenté
//! comme avant.
//!
//! Le module est pur et sans `cfg` : la sonde Windows vit dans
//! `outputs/wasapi_exclusive.rs` et rend un [`AccueilDuDop`] ; la décision se
//! juge ici par `cargo test` sous Linux, là où WASAPI ne compile pas.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::dsd_to_dop::rupture_du_porteur_dop;

/// Ce que la sortie a répondu sur un porteur DoP (cadence DoP, 24 bits, voies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccueilDuDop {
    /// Le pilote prend le format en exclusif, avec au moins 24 bits valides.
    Accepte,
    /// Aucune réponse exploitable : autre plateforme, autre bras (ASIO,
    /// CoreAudio), pilote occupé, sonde impossible. Rien n'est changé.
    Inconnu,
    /// WASAPI exclusif : le pilote refuse tous les formats 24 bits à cette
    /// cadence. `detail` reprend la négociation (formats et `HRESULT`).
    ExclusifRefuse { detail: String },
    /// WASAPI partagé : le format du mixeur de Windows, auquel la sortie
    /// s'ouvrira. Le DoP ne passe que s'il est à la même cadence et sur les
    /// mêmes voies (la règle de #3233, [`rupture_du_porteur_dop`]).
    MixeurPartage { cadence: u32, voies: u16 },
}

/// Pourquoi le DoP a été écarté pour cette piste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DopEcarte {
    /// Le pilote refuse le DoP en exclusif.
    ExclusifRefuse { cadence_dop: u32, voies: u16 },
    /// Le mixeur partagé de Windows n'est pas au format du DoP.
    MixeurPartage {
        cadence_dop: u32,
        voies: u16,
        cadence_mixeur: u32,
        voies_mixeur: u16,
    },
}

impl DopEcarte {
    /// Code stable, pour le journal et le chemin du signal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExclusifRefuse { .. } => "dop_refuse_par_le_pilote_en_exclusif",
            Self::MixeurPartage { .. } => "dop_incompatible_avec_le_mixeur_partage",
        }
    }

    /// La raison, en clair, que le chemin du signal affiche.
    #[must_use]
    pub fn raison(&self) -> String {
        match self {
            Self::ExclusifRefuse { cadence_dop, voies } => format!(
                "Converti en PCM : la sortie WASAPI exclusive n'accepte pas le DoP \
                 ({} kHz, 24 bits, {voies} voies) ; le DAC ne l'annonce pas",
                khz(*cadence_dop)
            ),
            Self::MixeurPartage {
                cadence_dop,
                voies,
                cadence_mixeur,
                voies_mixeur,
            } => format!(
                "Converti en PCM : en WASAPI partagé, Windows mélange à {} kHz sur \
                 {voies_mixeur} voies, et un DoP à {} kHz sur {voies} voies n'y survit \
                 pas. Pour du DSD en DoP, passez la sortie en WASAPI exclusif",
                khz(*cadence_mixeur),
                khz(*cadence_dop)
            ),
        }
    }
}

fn khz(hz: u32) -> String {
    let k = f64::from(hz) / 1000.0;
    format!("{k:.1}").replace('.', ",")
}

/// LA règle, pure : faut-il écarter le DoP que la zone demande ?
///
/// `None` = le DoP part comme avant (accepté, ou réponse inconnue).
#[must_use]
pub fn decider(accueil: &AccueilDuDop, cadence_dop: u32, voies: u16) -> Option<DopEcarte> {
    match accueil {
        AccueilDuDop::Accepte | AccueilDuDop::Inconnu => None,
        AccueilDuDop::ExclusifRefuse { .. } => {
            Some(DopEcarte::ExclusifRefuse { cadence_dop, voies })
        }
        AccueilDuDop::MixeurPartage {
            cadence,
            voies: voies_mixeur,
        } => rupture_du_porteur_dop(true, cadence_dop, *cadence, voies, *voies_mixeur).map(|_| {
            DopEcarte::MixeurPartage {
                cadence_dop,
                voies,
                cadence_mixeur: *cadence,
                voies_mixeur: *voies_mixeur,
            }
        }),
    }
}

// ── Ce que l'exclusif a déjà répondu, par sortie ────────────────────────────
//
// Entre deux pistes, le flux exclusif précédent tient encore l'endpoint : la
// sonde répond alors « occupé », donc `Inconnu`. Une réponse CONCLUANTE est
// retenue jusqu'au redémarrage, pour que la piste suivante d'un album ne
// retente pas un DoP déjà refusé. Le partagé n'est jamais retenu : le format
// du mixeur change dès que l'utilisateur touche aux réglages de Windows.

type CleExclusive = (String, u32, u16);

fn exclusif_connu() -> &'static Mutex<HashMap<CleExclusive, AccueilDuDop>> {
    static T: OnceLock<Mutex<HashMap<CleExclusive, AccueilDuDop>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// La réponse exclusive déjà connue pour ce périphérique et ce format.
#[must_use]
pub fn accueil_exclusif_connu(
    peripherique: &str,
    cadence_dop: u32,
    voies: u16,
) -> Option<AccueilDuDop> {
    exclusif_connu()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(peripherique.to_string(), cadence_dop, voies))
        .cloned()
}

/// Retient une réponse exclusive, si elle est concluante.
pub fn retenir_accueil_exclusif(
    peripherique: &str,
    cadence_dop: u32,
    voies: u16,
    accueil: &AccueilDuDop,
) {
    if matches!(
        accueil,
        AccueilDuDop::Accepte | AccueilDuDop::ExclusifRefuse { .. }
    ) {
        exclusif_connu()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                (peripherique.to_string(), cadence_dop, voies),
                accueil.clone(),
            );
    }
}

// ── La raison affichée, par zone ────────────────────────────────────────────

fn par_zone() -> &'static Mutex<HashMap<i64, DopEcarte>> {
    static R: OnceLock<Mutex<HashMap<i64, DopEcarte>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pose la raison de la piste DSD en cours sur la zone.
pub fn enregistrer(zone_id: i64, ecart: DopEcarte) {
    par_zone()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(zone_id, ecart);
}

/// La raison posée sur la zone, s'il y en a une.
#[must_use]
pub fn ecart_de_la_zone(zone_id: i64) -> Option<DopEcarte> {
    par_zone()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&zone_id)
        .cloned()
}

/// Retire la raison : appelé à chaque nouvelle piste DSD, avant de décider.
pub fn oublier(zone_id: i64) {
    par_zone()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&zone_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOP64: u32 = 176_400;
    const DOP128: u32 = 352_800;

    /// Le cas de JeanPitre : mixeur à 176,4 kHz, DSD128 en DoP à 352,8 kHz.
    /// Avant : refus dans le fil de lecture. Maintenant : PCM, avec la raison.
    #[test]
    fn partage_a_une_autre_cadence_ecarte_le_dop() {
        let accueil = AccueilDuDop::MixeurPartage {
            cadence: DOP64,
            voies: 2,
        };
        let ecart = decider(&accueil, DOP128, 2).expect("le DoP doit être écarté");
        assert_eq!(ecart.code(), "dop_incompatible_avec_le_mixeur_partage");
        let raison = ecart.raison();
        assert!(raison.contains("352,8 kHz"), "{raison}");
        assert!(raison.contains("176,4 kHz"), "{raison}");
        assert!(raison.contains("WASAPI exclusif"), "{raison}");
    }

    /// Le seul chemin partagé où le DoP passe vraiment (#3233) reste ouvert :
    /// mixeur à la cadence du DoP, mêmes voies.
    #[test]
    fn partage_a_la_cadence_du_dop_garde_le_dop() {
        let accueil = AccueilDuDop::MixeurPartage {
            cadence: DOP64,
            voies: 2,
        };
        assert_eq!(decider(&accueil, DOP64, 2), None);
    }

    #[test]
    fn partage_sur_d_autres_voies_ecarte_le_dop() {
        let accueil = AccueilDuDop::MixeurPartage {
            cadence: DOP64,
            voies: 8,
        };
        assert!(matches!(
            decider(&accueil, DOP64, 2),
            Some(DopEcarte::MixeurPartage {
                voies_mixeur: 8,
                ..
            })
        ));
    }

    #[test]
    fn exclusif_refuse_ecarte_le_dop() {
        let accueil = AccueilDuDop::ExclusifRefuse {
            detail: "24/24 → 0x88890008".into(),
        };
        let ecart = decider(&accueil, DOP128, 2).expect("écarté");
        assert_eq!(ecart.code(), "dop_refuse_par_le_pilote_en_exclusif");
        assert!(ecart.raison().contains("352,8 kHz"));
    }

    /// Ni « accepté » ni « je ne sais pas » ne changent le comportement
    /// d'avant : le DoP est tenté.
    #[test]
    fn accepte_ou_inconnu_ne_change_rien() {
        assert_eq!(decider(&AccueilDuDop::Accepte, DOP128, 2), None);
        assert_eq!(decider(&AccueilDuDop::Inconnu, DOP128, 2), None);
    }

    #[test]
    fn seule_une_reponse_exclusive_concluante_est_retenue() {
        let nom = "DAC de test 7265";
        retenir_accueil_exclusif(nom, DOP128, 2, &AccueilDuDop::Inconnu);
        assert_eq!(accueil_exclusif_connu(nom, DOP128, 2), None);
        retenir_accueil_exclusif(
            nom,
            DOP128,
            2,
            &AccueilDuDop::ExclusifRefuse {
                detail: String::new(),
            },
        );
        assert!(matches!(
            accueil_exclusif_connu(nom, DOP128, 2),
            Some(AccueilDuDop::ExclusifRefuse { .. })
        ));
        assert_eq!(accueil_exclusif_connu(nom, DOP64, 2), None);
    }

    #[test]
    fn la_raison_vit_par_zone_et_s_oublie() {
        let zone = 7265_001;
        enregistrer(
            zone,
            DopEcarte::ExclusifRefuse {
                cadence_dop: DOP64,
                voies: 2,
            },
        );
        assert!(ecart_de_la_zone(zone).is_some());
        oublier(zone);
        assert!(ecart_de_la_zone(zone).is_none());
    }
}
