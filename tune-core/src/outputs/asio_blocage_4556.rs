//! #4556 — quand le balayage ASIO est SUSPENDU, le refus de lecture doit le
//! dire, nommer le témoin et indiquer le geste qui débloque.
//!
//! Le coupe-circuit de #1283/#4168 transforme « Tune ne démarre plus » en
//! « Tune démarre sans ASIO ». Le prix se paye plus loin : le parc local est
//! alors un repli WASAPI, la zone créée sous un nom ASIO n'y figure pas, et
//! `gate_or_rebind_offline_zone` rend `zone_output_unavailable` — « Vérifiez
//! qu'elle est branchée et allumée ». Marco Polo (fil 1852, SMSL SU-1) en a
//! conclu que son DAC était mort, alors que Windows le voyait très bien et que
//! le serveur savait parfaitement qu'il **n'avait pas regardé** du côté d'ASIO.
//!
//! Ce module porte les trois choses qui manquaient :
//!
//! 1. **Le motif du blocage**, posé une fois au démarrage par
//!    `startup::spawn_asio_warm_scan` — avec le chemin du témoin, la seule
//!    pièce qui permet de réparer à la main.
//! 2. **La constatation que le parc local est un repli**, posée par
//!    `startup::register_local_outputs` au moment exact où elle est MESURÉE :
//!    backend demandé `asio`, hôte muet, ré-énumération WASAPI. Sans elle, une
//!    machine réglée en WASAPI avec un témoin ASIO oublié se verrait accuser
//!    ASIO à tort.
//! 3. **La sentinelle du refus**, à l'image de `free_zone_cap:` et de
//!    `bitperfect_strict_refused:` : un code stable que la couche HTTP et le
//!    client peuvent lire pour offrir le bouton « Réarmer ASIO » là où le
//!    défaut se manifeste, au lieu de l'enterrer dans l'écran Diagnostics.
//!
//! ⚠️ L'état vit dans un [`EtatDeBlocage`] **instanciable**, dont [`ETAT`] est
//! l'unique exemplaire de production. Les tests se donnent le leur : muter le
//! global depuis un test empoisonnerait, en parallèle, les dizaines de tests
//! de `orchestrator` qui attendent l'ANCIEN message.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Le balayage ASIO est suspendu par le témoin de plantage. Réarmable.
pub const CODE_APRES_PLANTAGE: &str = "asio_scan_blocked_after_crash";

/// Le balayage ASIO est coupé par `TUNE_DISABLE_ASIO_SCAN`. Un bouton ne doit
/// PAS contourner un coupe-circuit posé par l'exploitant.
pub const CODE_PAR_ENVIRONNEMENT: &str = "asio_scan_disabled_by_env";

/// Préfixe de la sentinelle portée par `Err(String)` jusqu'à la route HTTP.
///
/// Volontairement DISJOINT de `zone_output_unavailable:` — le caractère qui
/// suit `zone_output_unavailable` est `_` et non `:`, donc l'ancien
/// `strip_prefix` ne l'attrape pas et l'ancien chemin reste intact.
pub const PREFIXE_SENTINELLE: &str = "zone_output_unavailable_asio:";

/// La route qui réarme le témoin. Écrite ici pour qu'il n'y ait qu'un seul
/// endroit où la lire : le commentaire de triage de #4556 la donnait fausse
/// (`/api/v1/system/asio-warm-scan/rearm`, sans le segment `audio`).
pub const ROUTE_DE_REARMEMENT: &str = "/api/v1/system/audio/asio-warm-scan/rearm";

/// Pourquoi l'énumération ASIO est fermée pour ce processus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotifDeBlocage {
    /// Le témoin de plantage était là au démarrage : un pilote ASIO a emporté
    /// le processus précédent.
    ApresPlantage,
    /// `TUNE_DISABLE_ASIO_SCAN` est armée.
    ParEnvironnement,
}

impl MotifDeBlocage {
    /// Le code stable — champ `reason` de la réponse HTTP, champ `code` de
    /// l'événement `zone.playback_error`.
    pub fn code(self) -> &'static str {
        match self {
            Self::ApresPlantage => CODE_APRES_PLANTAGE,
            Self::ParEnvironnement => CODE_PAR_ENVIRONNEMENT,
        }
    }

    /// Le bouton « Réarmer ASIO » a-t-il un sens ? Non quand c'est
    /// l'environnement qui coupe : `rearm_asio_warm_scan` refuse déjà de
    /// retirer le témoin dans ce cas.
    pub fn peut_rearmer(self) -> bool {
        matches!(self, Self::ApresPlantage)
    }

    fn depuis_code_interne(brut: u8) -> Option<Self> {
        match brut {
            1 => Some(Self::ApresPlantage),
            2 => Some(Self::ParEnvironnement),
            _ => None,
        }
    }

    fn code_interne(self) -> u8 {
        match self {
            Self::ApresPlantage => 1,
            Self::ParEnvironnement => 2,
        }
    }
}

/// L'état du coupe-circuit, tel qu'un refus doit pouvoir le raconter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlocageAsio {
    pub motif: MotifDeBlocage,
    /// Chemin du fichier témoin, quand il y en a un. Il part dans le message :
    /// c'est ce qu'on demande au testeur de regarder, et ce qu'un support peut
    /// lui faire supprimer à la main si l'interface est inaccessible.
    pub temoin: Option<String>,
}

impl BlocageAsio {
    /// La phrase française — celle du journal, de l'événement WebSocket et du
    /// `message` HTTP pour les clients qui ne connaissent pas encore le code.
    ///
    /// Elle fait trois choses que l'ancienne ne faisait pas : elle dit que le
    /// serveur N'A PAS REGARDÉ, elle nomme le témoin, et elle donne le geste.
    pub fn message_fr(&self, appareil: Option<&str>, zone: &str) -> String {
        let cible = match appareil {
            Some(nom) => format!("La sortie « {nom} » de la zone « {zone} »"),
            None => format!("La sortie de la zone « {zone} »"),
        };
        let temoin = match self.temoin.as_deref() {
            Some(chemin) => format!(" (témoin : {chemin})"),
            None => String::new(),
        };
        match self.motif {
            MotifDeBlocage::ApresPlantage => format!(
                "{cible} n'a pas pu être cherchée : le balayage ASIO est suspendu depuis un \
                 plantage de pilote{temoin}. Votre appareil n'est pas forcément en cause — Tune \
                 n'a pas ouvert ASIO de ce démarrage. Réarmez le balayage ASIO puis redémarrez \
                 Tune. En attendant, vous pouvez choisir le même appareil sous WASAPI dans les \
                 réglages de la zone."
            ),
            MotifDeBlocage::ParEnvironnement => format!(
                "{cible} n'a pas pu être cherchée : le balayage ASIO est désactivé par la \
                 variable d'environnement TUNE_DISABLE_ASIO_SCAN{temoin}. Retirez-la puis \
                 redémarrez Tune, ou choisissez une sortie WASAPI dans les réglages de la zone."
            ),
        }
    }

    /// `zone_output_unavailable_asio:<code>:<phrase>`.
    pub fn sentinelle(&self, message: &str) -> String {
        format!("{PREFIXE_SENTINELLE}{}:{message}", self.motif.code())
    }
}

/// L'état du coupe-circuit pour UN processus. Instanciable pour que les tests
/// n'aient jamais à toucher [`ETAT`].
#[derive(Default)]
pub struct EtatDeBlocage {
    motif: AtomicU8,
    temoin: Mutex<Option<String>>,
    parc_est_un_repli: AtomicBool,
}

impl EtatDeBlocage {
    const fn neuf() -> Self {
        Self {
            motif: AtomicU8::new(0),
            temoin: Mutex::new(None),
            parc_est_un_repli: AtomicBool::new(false),
        }
    }

    /// Ferme le coupe-circuit et retient POURQUOI.
    pub fn bloquer(&self, motif: MotifDeBlocage, temoin: Option<&str>) {
        if let Ok(mut slot) = self.temoin.lock() {
            *slot = temoin.map(str::to_string);
        }
        self.motif.store(motif.code_interne(), Ordering::Release);
    }

    /// Le blocage en cours, s'il y en a un.
    pub fn blocage(&self) -> Option<BlocageAsio> {
        let motif = MotifDeBlocage::depuis_code_interne(self.motif.load(Ordering::Acquire))?;
        let temoin = self.temoin.lock().ok().and_then(|slot| slot.clone());
        Some(BlocageAsio { motif, temoin })
    }

    /// `true` dès que l'énumération ASIO est fermée, quel qu'en soit le motif.
    pub fn enumeration_bloquee(&self) -> bool {
        self.motif.load(Ordering::Acquire) != 0
    }

    /// Le parc local publié pour ce démarrage est un **repli WASAPI** faute
    /// d'énumération ASIO.
    pub fn noter_repli_wasapi_apres_blocage(&self) {
        self.parc_est_un_repli.store(true, Ordering::Release);
    }

    /// Le blocage à raconter à l'utilisateur d'une zone `local:` introuvable,
    /// ou `None` s'il n'y a rien d'honnête à lui dire.
    ///
    /// Exige les DEUX conditions : la porte est fermée **et** le parc local
    /// publié est un repli. Une machine réglée en WASAPI, dont le témoin ASIO
    /// traîne depuis des mois, garde donc l'ancien message — son DAC est
    /// vraiment absent, et l'énumération WASAPI l'a bien mesuré.
    pub fn blocage_expliquant_un_parc_de_repli(&self) -> Option<BlocageAsio> {
        if !self.parc_est_un_repli.load(Ordering::Acquire) {
            return None;
        }
        self.blocage()
    }
}

/// L'unique exemplaire de production.
static ETAT: EtatDeBlocage = EtatDeBlocage::neuf();

/// Ferme le coupe-circuit du processus. Appelé par
/// `startup::spawn_asio_warm_scan`, via
/// `outputs::local::block_asio_device_enumeration` — un seul site d'appel,
/// pour que l'état global et la porte de `parc.rs` ne puissent pas diverger.
pub fn bloquer(motif: MotifDeBlocage, temoin: Option<&str>) {
    ETAT.bloquer(motif, temoin);
}

/// Voir [`EtatDeBlocage::blocage`].
pub fn blocage() -> Option<BlocageAsio> {
    ETAT.blocage()
}

/// Voir [`EtatDeBlocage::enumeration_bloquee`].
pub fn enumeration_bloquee() -> bool {
    ETAT.enumeration_bloquee()
}

/// Voir [`EtatDeBlocage::noter_repli_wasapi_apres_blocage`].
pub fn noter_repli_wasapi_apres_blocage() {
    ETAT.noter_repli_wasapi_apres_blocage();
}

/// Voir [`EtatDeBlocage::blocage_expliquant_un_parc_de_repli`].
pub fn blocage_expliquant_un_parc_de_repli() -> Option<BlocageAsio> {
    ETAT.blocage_expliquant_un_parc_de_repli()
}

/// Relit une sentinelle : `(code, phrase)`. `None` pour tout autre message.
///
/// Le code ne contient jamais de `:` ; la phrase, si — et un chemin Windows
/// aussi (`C:\Users\…`). D'où le `split_once(':')` et pas un `split`.
pub fn depuis_sentinelle(message: &str) -> Option<(&str, &str)> {
    let reste = message.strip_prefix(PREFIXE_SENTINELLE)?;
    let (code, phrase) = reste.split_once(':')?;
    if code != CODE_APRES_PLANTAGE && code != CODE_PAR_ENVIRONNEMENT {
        return None;
    }
    Some((code, phrase))
}

/// Le bouton « Réarmer ASIO » est-il proposable pour ce code ?
pub fn code_rearmable(code: &str) -> bool {
    code == CODE_APRES_PLANTAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sans_blocage_il_n_y_a_rien_a_raconter() {
        let etat = EtatDeBlocage::default();
        assert_eq!(etat.blocage(), None);
        assert!(!etat.enumeration_bloquee());
        // Même un repli noté par erreur ne fabrique pas un motif.
        etat.noter_repli_wasapi_apres_blocage();
        assert_eq!(etat.blocage_expliquant_un_parc_de_repli(), None);
    }

    /// Le cœur de #4556 : un témoin SEUL n'autorise pas à accuser ASIO.
    ///
    /// Machine Windows réglée en WASAPI, témoin ASIO oublié depuis des mois :
    /// le parc WASAPI a bien été énuméré, le DAC absent est vraiment absent.
    #[test]
    fn un_temoin_sans_repli_mesure_ne_change_pas_le_message() {
        let etat = EtatDeBlocage::default();
        etat.bloquer(
            MotifDeBlocage::ApresPlantage,
            Some(r"C:\tmp\asio-warm.pending"),
        );
        assert!(etat.enumeration_bloquee());
        assert_eq!(
            etat.blocage_expliquant_un_parc_de_repli(),
            None,
            "sans repli MESURÉ, le refus ne doit pas accuser ASIO"
        );
    }

    #[test]
    fn temoin_plus_repli_donne_un_message_qui_nomme_le_temoin_et_le_geste() {
        let etat = EtatDeBlocage::default();
        let temoin = r"C:\Users\Marco\AppData\Local\TuneServer\asio-warm.pending";
        etat.bloquer(MotifDeBlocage::ApresPlantage, Some(temoin));
        etat.noter_repli_wasapi_apres_blocage();

        let blocage = etat
            .blocage_expliquant_un_parc_de_repli()
            .expect("les deux conditions sont là");
        assert_eq!(blocage.motif, MotifDeBlocage::ApresPlantage);
        let message = blocage.message_fr(Some("USB DAC ASIO"), "USB DAC ASIO");
        assert!(message.contains("USB DAC ASIO"), "{message}");
        assert!(
            message.contains(temoin),
            "le témoin doit être nommé : {message}"
        );
        assert!(
            message.contains("Réarmez"),
            "le geste doit être donné : {message}"
        );
        assert!(
            !message.contains("branchée et allumée"),
            "le refus ne doit plus accuser le câble : {message}"
        );
    }

    #[test]
    fn la_variable_d_environnement_ne_propose_pas_le_bouton() {
        let etat = EtatDeBlocage::default();
        etat.bloquer(MotifDeBlocage::ParEnvironnement, None);
        etat.noter_repli_wasapi_apres_blocage();
        let blocage = etat.blocage_expliquant_un_parc_de_repli().unwrap();
        assert!(!blocage.motif.peut_rearmer());
        let message = blocage.message_fr(None, "Salon");
        assert!(message.contains("TUNE_DISABLE_ASIO_SCAN"), "{message}");
        assert!(
            !message.contains("témoin :"),
            "sans fichier témoin, ne pas en inventer un : {message}"
        );
        assert!(!code_rearmable(CODE_PAR_ENVIRONNEMENT));
        assert!(code_rearmable(CODE_APRES_PLANTAGE));
    }

    /// Un chemin Windows porte un `:` : la relecture doit rendre la phrase
    /// ENTIÈRE, pas son premier morceau.
    #[test]
    fn la_sentinelle_survit_a_un_chemin_windows() {
        let blocage = BlocageAsio {
            motif: MotifDeBlocage::ApresPlantage,
            temoin: Some(r"C:\Users\Marco\AppData\Local\TuneServer\asio-warm.pending".into()),
        };
        let message = blocage.message_fr(Some("USB DAC ASIO"), "USB DAC ASIO");
        let sentinelle = blocage.sentinelle(&message);
        let (code, relu) = depuis_sentinelle(&sentinelle).expect("sentinelle relisible");
        assert_eq!(code, CODE_APRES_PLANTAGE);
        assert_eq!(relu, message);
        assert!(relu.contains(r"C:\Users\Marco"), "{relu}");
    }

    /// L'ancienne sentinelle ne doit pas être happée par la nouvelle, et
    /// réciproquement : les deux préfixes sont disjoints.
    #[test]
    fn les_deux_sentinelles_ne_se_confondent_pas() {
        let ancienne = "zone_output_unavailable:La sortie n'est plus disponible.";
        assert_eq!(depuis_sentinelle(ancienne), None);
        let nouvelle = format!("{PREFIXE_SENTINELLE}{CODE_APRES_PLANTAGE}:bla");
        assert_eq!(nouvelle.strip_prefix("zone_output_unavailable:"), None);
        assert!(depuis_sentinelle(&nouvelle).is_some());
        // Un code inconnu n'est pas une sentinelle de ce module.
        assert_eq!(
            depuis_sentinelle(&format!("{PREFIXE_SENTINELLE}autre_chose:bla")),
            None
        );
    }

    /// La route de réarmement est celle qui est MONTÉE : le segment `audio`
    /// manquait dans le commentaire de triage, et un bouton qui appelle une
    /// 404 serait pire que pas de bouton.
    #[test]
    fn la_route_de_rearmement_porte_le_segment_audio() {
        assert_eq!(
            ROUTE_DE_REARMEMENT,
            "/api/v1/system/audio/asio-warm-scan/rearm"
        );
    }
}
