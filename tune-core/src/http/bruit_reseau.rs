//! #6255 — les derniers `bruit_suspect_reseau` du processus, et le réglage
//! qui éteint le détecteur.
//!
//! Le détecteur lui-même vit dans `tune-stream-http` (il regarde les octets
//! qui partent vers un lecteur réseau). Ce module ne fait que GARDER : une
//! fenêtre bornée des derniers événements, que le rapport de bug et l'export
//! de diagnostic relisent, et l'interrupteur que la route des réglages pose.
//!
//! Rien ici ne décide du rendu : diagnostic seulement.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

/// Clé du réglage (`settings`). Absente : le détecteur est ALLUMÉ.
pub const CLE_REGLAGE: &str = "network_noise_watch";

/// Événements gardés en mémoire, les plus récents.
pub const EVENEMENTS_GARDES: usize = 32;

static ACTIF: AtomicBool = AtomicBool::new(true);
static DERNIERS: Mutex<VecDeque<EvenementBruitReseau>> = Mutex::new(VecDeque::new());

/// Le détecteur est-il allumé ?
pub fn actif() -> bool {
    ACTIF.load(Ordering::Relaxed)
}

/// Allume ou éteint le détecteur, pour les connexions qui s'ouvrent ensuite.
pub fn regler(actif: bool) {
    ACTIF.store(actif, Ordering::Relaxed);
}

/// La valeur du réglage telle qu'elle est écrite en base. Seul un « non »
/// explicite éteint : une valeur absente ou illisible laisse allumé.
pub fn resoudre(valeur: Option<&str>) -> bool {
    !matches!(
        valeur.map(|v| v.trim().trim_matches('"').to_ascii_lowercase()),
        Some(v) if matches!(v.as_str(), "false" | "0" | "off" | "no" | "non")
    )
}

/// Un `bruit_suspect_reseau` : ce qui a été vu sur UNE connexion.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvenementBruitReseau {
    /// RFC 3339, UTC.
    pub horodatage: String,
    /// `reprise_hors_trame`, `decodage_decale_probable`, `signal_bruite`,
    /// `entete_au_milieu`.
    pub cause: &'static str,
    pub stream_id: String,
    /// `conversion` ou `fichier`.
    pub voie: &'static str,
    /// La zone, quand l'orchestrateur l'a posée sur la session ; `-1` sinon.
    pub zone_id: i64,
    /// Le `User-Agent` du lecteur.
    pub agent: String,
    /// L'en-tête `Range` reçu, tel quel.
    pub range: String,
    /// L'offset que le lecteur a demandé (`bytes=N-`).
    pub offset_demande: Option<u64>,
    /// L'offset, dans le flux, du premier octet que la connexion a envoyé.
    pub offset_servi: u64,
    /// Octets d'écart à la grille des trames de l'offset demandé.
    pub phase: u64,
    /// Taille d'une trame PCM (canaux × octets par échantillon).
    pub trame: u64,
    /// Position, en secondes de musique, de l'endroit jugé.
    pub position_s: f64,
    /// `fréquence/profondeur/canaux` du flux.
    pub format: String,
    /// Les mesures, en clair.
    pub detail: String,
}

/// Garde un événement (les [`EVENEMENTS_GARDES`] derniers).
pub fn noter(evenement: EvenementBruitReseau) {
    let mut d = DERNIERS.lock().unwrap_or_else(|e| e.into_inner());
    while d.len() >= EVENEMENTS_GARDES {
        d.pop_front();
    }
    d.push_back(evenement);
}

/// Les derniers événements, du plus ancien au plus récent.
pub fn derniers() -> Vec<EvenementBruitReseau> {
    DERNIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// L'horodatage d'un événement noté maintenant.
pub fn maintenant() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// La section du rapport de bug. `nom_de_zone` traduit un `zone_id` en nom
/// lisible (l'appareil) quand il est connu.
pub fn section_du_rapport(
    evenements: &[EvenementBruitReseau],
    nom_de_zone: impl Fn(i64) -> Option<String>,
) -> String {
    let mut md = String::from("## Lecteurs reseau — bruit suspect (#6255)\n");
    if !actif() {
        md.push_str(&format!("- Detecteur ETEINT (reglage `{CLE_REGLAGE}`)\n"));
    }
    if evenements.is_empty() {
        md.push_str("- Aucun evenement `bruit_suspect_reseau` depuis le demarrage\n\n");
        return md;
    }
    for e in evenements.iter().rev() {
        let zone = if e.zone_id < 0 {
            "zone inconnue".to_string()
        } else {
            match nom_de_zone(e.zone_id) {
                Some(nom) => format!("zone {} ({nom})", e.zone_id),
                None => format!("zone {}", e.zone_id),
            }
        };
        md.push_str(&format!(
            "- {} `{}` {} — {}, agent `{}`, range `{}`, servi depuis {}, phase {}/{} a {:.1} s, {} ; {}\n",
            e.horodatage,
            e.cause,
            e.voie,
            zone,
            e.agent,
            e.range,
            e.offset_servi,
            e.phase,
            e.trame,
            e.position_s,
            e.format,
            e.detail,
        ));
    }
    md.push_str(
        "  (le serveur n'entend pas le lecteur : il juge ce qu'il ENVOIE et comment un \
lecteur qui decode depuis le debut de la connexion le lira. Diagnostic seulement)\n\n",
    );
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evenement(cause: &'static str, zone_id: i64) -> EvenementBruitReseau {
        EvenementBruitReseau {
            horodatage: "2026-10-13T08:00:00.000Z".into(),
            cause,
            stream_id: "s".into(),
            voie: "conversion",
            zone_id,
            agent: "Lavf/58.45.100".into(),
            range: "bytes=31958520-".into(),
            offset_demande: Some(31_958_520),
            offset_servi: 31_958_520,
            phase: 4,
            trame: 6,
            position_s: 120.8,
            format: "44100/24/2".into(),
            detail: "-".into(),
        }
    }

    #[test]
    fn seul_un_non_explicite_eteint() {
        assert!(resoudre(None));
        assert!(resoudre(Some("true")));
        assert!(resoudre(Some("n'importe quoi")));
        for non in ["false", "\"false\"", "0", "off", "No", " non "] {
            assert!(!resoudre(Some(non)), "{non}");
        }
    }

    #[test]
    fn la_section_nomme_la_zone_et_la_cause() {
        let md = section_du_rapport(
            &[
                evenement("reprise_hors_trame", 10),
                evenement("signal_bruite", -1),
            ],
            |id| (id == 10).then(|| "Salon DMP-A8".to_string()),
        );
        assert!(
            md.contains("`reprise_hors_trame` conversion — zone 10 (Salon DMP-A8)"),
            "{md}"
        );
        assert!(md.contains("phase 4/6 a 120.8 s"), "{md}");
        assert!(md.contains("zone inconnue"), "{md}");
        // Le plus récent en premier.
        assert!(md.find("signal_bruite").unwrap() < md.find("reprise_hors_trame").unwrap());
        let vide = section_du_rapport(&[], |_| None);
        assert!(vide.contains("Aucun evenement"), "{vide}");
    }
}
