//! Ce que chaque pilote ASIO a DÉCLARÉ du DSD natif (#5643, lot D) — et,
//! depuis #8248, ce que chaque pilote ALSA `hw:` offre en `DSD_U32`.
//!
//! La capacité se sonde UNE fois par périphérique (bascule du pilote en DSD,
//! `kAsioCanSampleRate` pour chaque cadence, retour au PCM : voir
//! `asio_exclusive::sonder_cadences_dsd`), puis vit ici pour toute la durée
//! du processus. Le sondage n'a lieu que si le verrou de périphérique ASIO est
//! libre : jamais pendant la lecture d'un autre flux. S'il est occupé, rien
//! n'est écrit, et la piste part en DoP en le disant.
//!
//! Hors de tout `cfg` : l'API (`dsd_transport`) le lit sur toutes les
//! plateformes, et la règle se teste sous Linux. Sans pilote ASIO, la table
//! reste vide et tout se comporte comme avant #5643.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Ce qu'une sortie locale a déclaré, la dernière fois qu'on l'a regardée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapaciteConnue {
    /// La sortie n'a pas de bras DSD natif : ni ASIO exclusif, ni ALSA
    /// `hw:` (WASAPI, partagé, macOS, PCM ALSA logiciel). Le libellé API
    /// reste `sortie_non_asio`, que les clients connaissent.
    SortieNonAsio,
    /// Pilote ASIO sondé : les cadences DSD qu'il accepte (vide = aucune,
    /// pilote PCM seulement).
    Asio { cadences: Vec<u32> },
    /// #8248 — PCM ALSA `hw:` sondé : les cadences DSD qu'il accepte en
    /// `DSD_U32_BE`/`DSD_U32_LE` (vide = aucun format DSD natif offert).
    AlsaDsdU32 { cadences: Vec<u32> },
}

/// Une entrée de la table : la capacité, et si elle n'est que PROVISOIRE.
///
/// #9170 — un verdict « pas de DSD natif » posé sans preuve venue du pilote
/// (PCM résolu hors `hw:`, par exemple parce que l'accès direct n'était pas
/// énuméré à cet instant) reste affiché mais se re-sonde : il ne fige plus
/// la sortie jusqu'au redémarrage.
#[derive(Debug, Clone)]
struct Entree {
    capacite: CapaciteConnue,
    provisoire_depuis: Option<Instant>,
}

/// #9170 — délai avant de re-sonder une capacité provisoire à l'ouverture
/// d'un flux (`LocalOutput::play_url` ouvre à chaque piste : on n'énumère pas
/// les périphériques à chaque fois).
pub const DELAI_AVANT_RESONDAGE: Duration = Duration::from_secs(30);

fn table() -> &'static Mutex<HashMap<String, Entree>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Entree>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// La capacité connue de la sortie `device_name` (le nom, sans le préfixe
/// `local:`).
#[must_use]
pub fn connue(device_name: &str) -> Option<CapaciteConnue> {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(device_name)
        .map(|e| e.capacite.clone())
}

/// Enregistre ce qu'on vient d'apprendre de `device_name`, sur une preuve :
/// le pilote a répondu (cadences `DSD_U32` sondées sur le `hw:` ouvert,
/// pilote ASIO sondé), ou la sortie ne peut structurellement pas porter de
/// bras natif (hôte non ALSA, plateforme sans bras). Définitif.
pub fn retenir(device_name: &str, capacite: CapaciteConnue) {
    table().lock().unwrap_or_else(|p| p.into_inner()).insert(
        device_name.to_string(),
        Entree {
            capacite,
            provisoire_depuis: None,
        },
    );
}

/// #9170 — enregistre une capacité observée SANS preuve du pilote (le nom a
/// résolu vers un PCM logiciel). Elle est publiée telle quelle, mais
/// [`a_resonder`] la rend au sondage suivant. Elle n'écrase jamais une
/// capacité prouvée : un `hw:` qui a répondu `DSD_U32` ne redevient pas
/// « non natif » parce qu'un sondage ultérieur ne l'a pas trouvé.
pub fn retenir_provisoire(device_name: &str, capacite: CapaciteConnue) {
    retenir_provisoire_a(device_name, capacite, Instant::now());
}

fn retenir_provisoire_a(device_name: &str, capacite: CapaciteConnue, maintenant: Instant) {
    let mut t = table().lock().unwrap_or_else(|p| p.into_inner());
    if t.get(device_name)
        .is_some_and(|e| e.provisoire_depuis.is_none())
    {
        return;
    }
    t.insert(
        device_name.to_string(),
        Entree {
            capacite,
            provisoire_depuis: Some(maintenant),
        },
    );
}

/// #9170 — faut-il sonder `device_name` ? Oui s'il est inconnu, ou si sa
/// capacité n'est que provisoire (sans délai : l'orchestrateur ne le demande
/// qu'à la résolution d'une piste DSD « Natif »).
#[must_use]
pub fn a_resonder(device_name: &str) -> bool {
    a_resonder_apres(device_name, Duration::ZERO, Instant::now())
}

/// #9170 — [`a_resonder`] pour l'ouverture d'un flux : une capacité
/// provisoire n'est re-sondée qu'après [`DELAI_AVANT_RESONDAGE`].
///
/// **Coût.** Un sondage énumère les périphériques ALSA (cpal) puis ouvre le
/// PCM en non bloquant : de quelques dizaines à quelques centaines de ms, sur
/// un fil bloquant, avant l'ouverture du flux. Une sortie dont le verdict
/// reste INCONNU (DAC introuvable, repli sur le défaut, PCM occupé : rien
/// n'est retenu) serait donc re-sondée à chaque piste ; un sondage sans
/// réponse ([`noter_sondage_sans_reponse`]) espace lui aussi le suivant de
/// [`DELAI_AVANT_RESONDAGE`]. La résolution d'une piste DSD « Natif »
/// ([`a_resonder`]) n'attend pas ce délai : elle ne concerne que le DSD.
#[must_use]
pub fn a_resonder_a_l_ouverture(device_name: &str) -> bool {
    let maintenant = Instant::now();
    let espace = sans_reponse()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(device_name)
        .is_none_or(|t| maintenant.saturating_duration_since(*t) >= DELAI_AVANT_RESONDAGE);
    espace && a_resonder_apres(device_name, DELAI_AVANT_RESONDAGE, maintenant)
}

fn sans_reponse() -> &'static Mutex<HashMap<String, Instant>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// #9170 — un sondage de `device_name` n'a rien appris (introuvable, repli,
/// occupé) : rien n'est retenu, mais l'ouverture suivante ne re-sonde
/// qu'après [`DELAI_AVANT_RESONDAGE`] (voir le coût ci-dessus).
pub fn noter_sondage_sans_reponse(device_name: &str) {
    sans_reponse()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(device_name.to_string(), Instant::now());
}

/// #9170 — le nom ne mène plus au `hw:` qui a prouvé sa capacité (DAC
/// débranché, remplacé, nom qui résout désormais vers un PCM logiciel) : la
/// capacité reste publiée mais redevient provisoire, donc re-sondée à la
/// piste DSD suivante au lieu de faire échouer chaque piste native jusqu'au
/// redémarrage.
pub fn retrograder_en_provisoire(device_name: &str) {
    if let Some(e) = table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(device_name)
    {
        e.provisoire_depuis.get_or_insert_with(Instant::now);
    }
}

/// #9170 — le pilote a RÉPONDU à l'ouverture native (re-sondage du `hw:`)
/// sans offrir la cadence du fichier : sa réponse entière remplace la
/// capacité retenue — un autre DAC branché sous le même nom ne garde pas les
/// cadences de l'ancien, et la piste suivante part en DoP dès le premier
/// échec. `voies_sondees` : le sondage portait sur les voies de la table
/// ([`crate::outputs::local::VOIES_SONDEES_DSD_ALSA`]) ; sinon la réponse ne
/// vaut que pour ces voies-là, et seule la cadence est oubliée.
pub fn retenir_reponse_du_pilote(
    device_name: &str,
    voies_sondees: bool,
    cadence: u32,
    mut cadences: Vec<u32>,
) {
    if voies_sondees {
        cadences.retain(|c| *c != cadence);
        retenir(device_name, CapaciteConnue::AlsaDsdU32 { cadences });
    } else {
        oublier_cadence(device_name, cadence);
    }
}

fn a_resonder_apres(device_name: &str, delai: Duration, maintenant: Instant) -> bool {
    match table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(device_name)
    {
        None => true,
        Some(Entree {
            provisoire_depuis: Some(depuis),
            ..
        }) => maintenant.saturating_duration_since(*depuis) >= delai,
        Some(_) => false,
    }
}

/// #9170 — un flux DSD natif ALSA vient de s'ouvrir ET de démarrer sur le
/// `hw:` de `device_name` : la preuve la plus forte qui soit. Les cadences
/// re-sondées à l'ouverture remplacent tout verdict antérieur, provisoire ou
/// non, pour que l'écran dise ce qui part réellement vers le DAC.
pub fn confirmer_natif_alsa(device_name: &str, cadences: Vec<u32>) {
    retenir(device_name, CapaciteConnue::AlsaDsdU32 { cadences });
}

/// Le pilote a déclaré `cadence` mais l'ouverture native l'a refusée : on la
/// retire, pour que la piste suivante parte en DoP au lieu d'échouer encore.
pub fn oublier_cadence(device_name: &str, cadence: u32) {
    let mut t = table().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(Entree {
        capacite: CapaciteConnue::Asio { cadences } | CapaciteConnue::AlsaDsdU32 { cadences },
        ..
    }) = t.get_mut(device_name)
    {
        cadences.retain(|c| *c != cadence);
    }
}

/// Ce que l'API publie : « natif » si la sortie `output_device_id`
/// (`local:<nom>`) est connue comme ASIO ou ALSA `hw:` avec au moins une
/// cadence DSD déclarée. Ne sonde jamais : une route de lecture ne touche pas
/// au pilote.
#[must_use]
pub fn natif_annonce(output_device_id: Option<&str>) -> bool {
    let Some(nom) = output_device_id.and_then(|id| id.strip_prefix("local:")) else {
        return false;
    };
    matches!(
        connue(nom),
        Some(CapaciteConnue::Asio { cadences } | CapaciteConnue::AlsaDsdU32 { cadences })
            if !cadences.is_empty()
    )
}

/// #9170 — ce qui est RÉELLEMENT parti vers la sortie pour la dernière piste
/// DSD d'une zone réglée « Natif » : la capacité dit ce que la sortie
/// accepte, ceci dit ce que la zone a fait. L'écran des réglages le préfère
/// à la capacité quand il existe, pour ne pas annoncer un DoP qui n'a pas eu
/// lieu (DSD512 converti en PCM sous un plafond de 768 kHz, ou servi en
/// natif).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportReelDsd {
    /// Flux DSD brut confié au bras natif (ASIO, ALSA `DSD_U32`).
    Natif,
    /// DSD emballé en DoP.
    Dop,
    /// DSD converti en PCM (DoP hors plafond, DoP écarté par la sortie).
    Pcm,
}

/// Ce qui est retenu par zone : la sortie, et le transport — un natif
/// décidé n'est RÉEL qu'une fois le flux démarré par le bras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EtatTransport {
    NatifEnAttente,
    Reel(TransportReelDsd),
}

fn transports_reels() -> &'static Mutex<HashMap<i64, (String, EtatTransport)>> {
    static TABLE: OnceLock<Mutex<HashMap<i64, (String, EtatTransport)>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// #9170 — retient le transport réel de la dernière piste DSD de `zone_id`,
/// jouée sur `output_device_id` (`local:<nom>`).
pub fn noter_transport_reel(zone_id: i64, output_device_id: &str, transport: TransportReelDsd) {
    transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            zone_id,
            (output_device_id.to_string(), EtatTransport::Reel(transport)),
        );
}

/// #9170 — l'orchestrateur a DÉCIDÉ le natif pour `zone_id` : rien n'est
/// affiché tant que le bras n'a pas démarré le flux
/// ([`confirmer_transport_natif`]) ; un échec l'efface
/// ([`abandonner_transport_natif`]).
pub fn noter_natif_en_attente(zone_id: i64, output_device_id: &str) {
    transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            zone_id,
            (output_device_id.to_string(), EtatTransport::NatifEnAttente),
        );
}

/// #9170 — le flux DSD natif a démarré sur `output_device_id` : les zones qui
/// l'attendaient sur cette sortie sont réellement en natif.
pub fn confirmer_transport_natif(output_device_id: &str) {
    for (sortie, etat) in transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values_mut()
    {
        if sortie == output_device_id && *etat == EtatTransport::NatifEnAttente {
            *etat = EtatTransport::Reel(TransportReelDsd::Natif);
        }
    }
}

/// #9170 — le bras natif a échoué sur `output_device_id` : rien n'est parti,
/// le natif attendu ne s'affiche pas.
pub fn abandonner_transport_natif(output_device_id: &str) {
    transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, (sortie, etat)| {
            !(sortie == output_device_id && *etat == EtatTransport::NatifEnAttente)
        });
}

/// #9170 — la piste DSD de `zone_id` a été refusée (strict) : rien n'est
/// parti, l'ancien transport ne doit pas survivre.
pub fn effacer_transport_reel(zone_id: i64) {
    transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&zone_id);
}

/// #9170 — le transport réel de la dernière piste DSD de `zone_id`, s'il a
/// été relevé sur la sortie `output_device_id` (une autre sortie ne compte
/// pas : la zone a changé de sortie depuis). Un natif seulement décidé n'est
/// pas un transport réel.
#[must_use]
pub fn transport_reel(zone_id: i64, output_device_id: &str) -> Option<TransportReelDsd> {
    match transports_reels()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&zone_id)
    {
        Some((sortie, EtatTransport::Reel(t))) if sortie == output_device_id => Some(*t),
        _ => None,
    }
}

#[cfg(test)]
mod tests_9170 {
    use super::*;

    /// Un verdict provisoire se re-sonde : tout de suite à la résolution,
    /// après le délai à l'ouverture d'un flux.
    #[test]
    fn un_verdict_provisoire_se_resonde() {
        let nom = "9170-provisoire";
        let t0 = Instant::now();
        retenir_provisoire_a(nom, CapaciteConnue::SortieNonAsio, t0);
        assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
        assert!(a_resonder(nom));
        assert!(!a_resonder_apres(nom, DELAI_AVANT_RESONDAGE, t0));
        assert!(a_resonder_apres(
            nom,
            DELAI_AVANT_RESONDAGE,
            t0 + DELAI_AVANT_RESONDAGE
        ));
    }

    /// Une capacité prouvée ne se re-sonde pas et ne cède pas à un verdict
    /// provisoire ultérieur.
    #[test]
    fn une_capacite_prouvee_ne_cede_pas_au_provisoire() {
        let nom = "9170-prouvee";
        retenir(
            nom,
            CapaciteConnue::AlsaDsdU32 {
                cadences: vec![22_579_200],
            },
        );
        retenir_provisoire(nom, CapaciteConnue::SortieNonAsio);
        assert_eq!(
            connue(nom),
            Some(CapaciteConnue::AlsaDsdU32 {
                cadences: vec![22_579_200]
            })
        );
        assert!(!a_resonder(nom));
        assert!(!a_resonder_a_l_ouverture(nom));
    }

    /// Une lecture native réussie corrige un verdict « pas de natif », même
    /// définitif.
    #[test]
    fn une_lecture_native_corrige_le_verdict() {
        let nom = "9170-confirme";
        retenir(nom, CapaciteConnue::SortieNonAsio);
        confirmer_natif_alsa(nom, vec![22_579_200]);
        assert!(natif_annonce(Some(&format!("local:{nom}"))));
    }

    #[test]
    fn le_transport_reel_suit_la_sortie_de_la_zone() {
        noter_transport_reel(-9170, "local:A", TransportReelDsd::Pcm);
        assert_eq!(
            transport_reel(-9170, "local:A"),
            Some(TransportReelDsd::Pcm)
        );
        assert_eq!(transport_reel(-9170, "local:B"), None);
    }

    /// Relecture #9172 : un natif décidé puis un bras en échec n'est jamais
    /// affiché « natif » ; démarré, il l'est.
    #[test]
    fn natif_decide_puis_bras_en_echec_n_est_pas_affiche_natif() {
        noter_transport_reel(-91_721, "local:Z8", TransportReelDsd::Pcm);
        noter_natif_en_attente(-91_721, "local:Z8");
        assert_eq!(transport_reel(-91_721, "local:Z8"), None);
        abandonner_transport_natif("local:Z8");
        assert_eq!(transport_reel(-91_721, "local:Z8"), None);

        noter_natif_en_attente(-91_722, "local:Z8b");
        confirmer_transport_natif("local:Z8b");
        assert_eq!(
            transport_reel(-91_722, "local:Z8b"),
            Some(TransportReelDsd::Natif)
        );
        effacer_transport_reel(-91_722);
        assert_eq!(transport_reel(-91_722, "local:Z8b"), None);
    }

    /// Relecture #9172 : autre DAC sous le même nom — la réponse du pilote à
    /// l'ouverture remplace la capacité prouvée dès le premier échec.
    #[test]
    fn la_reponse_du_pilote_a_l_ouverture_remplace_la_capacite() {
        let nom = "9170-autre-dac";
        retenir(
            nom,
            CapaciteConnue::AlsaDsdU32 {
                cadences: vec![2_822_400, 22_579_200],
            },
        );
        retenir_reponse_du_pilote(nom, true, 22_579_200, vec![2_822_400]);
        assert_eq!(
            connue(nom),
            Some(CapaciteConnue::AlsaDsdU32 {
                cadences: vec![2_822_400]
            })
        );
        retenir_reponse_du_pilote(nom, true, 2_822_400, vec![]);
        assert_eq!(
            connue(nom),
            Some(CapaciteConnue::AlsaDsdU32 { cadences: vec![] })
        );
        assert!(!natif_annonce(Some(&format!("local:{nom}"))));
    }

    /// Relecture #9172 : un nom qui ne mène plus au `hw:` prouvé rend la
    /// capacité provisoire (re-sondée), sans l'effacer de l'écran.
    #[test]
    fn un_hw_perdu_retrograde_la_capacite_en_provisoire() {
        let nom = "9170-hw-perdu";
        retenir(
            nom,
            CapaciteConnue::AlsaDsdU32 {
                cadences: vec![22_579_200],
            },
        );
        assert!(!a_resonder(nom));
        retrograder_en_provisoire(nom);
        assert!(a_resonder(nom));
        retenir_provisoire(nom, CapaciteConnue::SortieNonAsio);
        assert_eq!(connue(nom), Some(CapaciteConnue::SortieNonAsio));
    }

    /// Un sondage sans réponse espace le suivant à l'ouverture.
    #[test]
    fn un_sondage_sans_reponse_espace_le_suivant_a_l_ouverture() {
        let nom = "9170-sans-reponse";
        assert!(a_resonder_a_l_ouverture(nom));
        noter_sondage_sans_reponse(nom);
        assert!(!a_resonder_a_l_ouverture(nom));
        assert!(a_resonder(nom), "la résolution DSD n'attend pas le délai");
    }
}
