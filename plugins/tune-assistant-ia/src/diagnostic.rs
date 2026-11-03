//! Le diagnostic en LECTURE SEULE : « Pourquoi ma zone ne joue pas ? »
//! (décisions de Bertrand du 23/10/2026, étude #6834 § 1).
//!
//! # Ce qui part, et seulement après l'avoir montré
//!
//! L'hôte rassemble ([`Collecte`]) : la configuration et l'état de la zone,
//! son chemin du signal — ou, zone arrêtée, son ÉQUIVALENT STATIQUE (la
//! sortie, le backend, le format et les réglages qui le décident) —, les
//! dernières lignes du journal (bornées, déjà passées par
//! `tune_core::confidentialite::anonymiser`), la version et le système, et
//! la liste des NOMS sensibles (zones, appareils, hôtes, titres récents).
//!
//! Le greffon filtre le journal ([`LIGNES_MAX`] lignes : avertissements,
//! erreurs, et lignes qui citent la zone ou un appareil), puis passe TOUT par
//! `anonymiser` et par la pseudonymisation stricte
//! ([`crate::pseudonymes`]). Le texte exact qui partirait est rendu par
//! `POST /diagnostic/apercu`, avec son coût estimé ; `POST /diagnostic` n'envoie
//! QUE un aperçu montré, une seule fois ([`Apercus::prendre`]) : chaque envoi
//! exige son aperçu.
//!
//! # Ce qui revient
//!
//! Un schéma fermé ([`valider`]) : un constat, des causes probables dont
//! chaque INDICE doit figurer mot pour mot dans le texte envoyé (le modèle
//! ne peut citer que ce qu'il a lu ; une cause sans indice vérifié est
//! écartée), des gestes À ESSAYER — des textes, jamais des appels : rien
//! n'est exécuté —, et un résumé pour un rapport de bug. Les vrais noms sont
//! rétablis pour l'écran ; le résumé, lui, reste pseudonymisé : c'est lui qui
//! peut être joint au rapport publié sur le forum, si l'utilisateur coche.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::appel::Choix;
use crate::pseudonymes::Pseudonymiseur;

pub const USAGE_DIAGNOSTIC: &str = "diagnostic";
/// Lignes de journal envoyées, au plus.
pub const LIGNES_MAX: usize = 200;
/// Caractères d'une ligne de journal, au plus.
pub const LIGNE_MAX: usize = 400;
/// La question de l'utilisateur, au plus.
pub const QUESTION_MAX: usize = 500;
/// Un aperçu vaut pour un envoi pendant ce délai.
pub const APERCU_DUREE: Duration = Duration::from_secs(15 * 60);
/// Aperçus gardés en mémoire, au plus.
pub const APERCUS_MAX: usize = 50;
/// Jetons de sortie.
pub const MAX_JETONS: u32 = 4096;

/// Ce que l'hôte rassemble pour un diagnostic.
#[derive(Clone, Debug, Default)]
pub struct ContexteBrut {
    /// La zone (configuration et état, chemin du signal compris s'il existe).
    pub zone: Option<Value>,
    /// Les dernières lignes du journal, plus anciennes d'abord.
    pub journal: Vec<String>,
    /// Version, système.
    pub systeme: Value,
    /// Les noms sensibles, par catégorie de [`crate::pseudonymes::CATEGORIES`].
    pub noms: BTreeMap<String, Vec<String>>,
}

/// La collecte de l'hôte : le greffon n'a pas accès à l'état du serveur.
pub type Collecte = Arc<
    dyn Fn(Option<i64>) -> Pin<Box<dyn Future<Output = Result<ContexteBrut, String>> + Send>>
        + Send
        + Sync,
>;

/// Les clés d'une zone qui disent comment son signal est produit, quand elle
/// ne joue pas et que le chemin du signal manque.
const CLES_STATIQUES: &[&str] = &[
    "output",
    "backend",
    "device",
    "renderer",
    "dsd",
    "sample",
    "rate",
    "bit",
    "format",
    "resampl",
    "eq",
    "volume",
    "mode",
    "type",
    "exclusive",
    "buffer",
    "latency",
    "gain",
    "replay",
    "crossfade",
    "channel",
];

/// Les clés qui ne servent pas au diagnostic : la file et la piste en cours.
const CLES_RETIREES: &[&str] = &["current_track", "queue", "artwork", "cover", "image"];

/// La zone, réduite, et son chemin du signal ou son équivalent statique.
pub fn zone_pour_diagnostic(zone: &Value) -> (Value, Value) {
    let mut z = zone.clone();
    if let Some(o) = z.as_object_mut() {
        o.retain(|k, _| !CLES_RETIREES.iter().any(|r| k.contains(r)));
    }
    let chemin = zone.get("signal_path").filter(|v| !v.is_null()).cloned();
    if let Some(o) = z.as_object_mut() {
        o.remove("signal_path");
    }
    let chemin = match chemin {
        Some(c) => json!({ "chemin_du_signal": c }),
        None => {
            let mut statique = Map::new();
            if let Some(o) = zone.as_object() {
                for (k, v) in o {
                    let bas = k.to_lowercase();
                    if CLES_STATIQUES.iter().any(|c| bas.contains(c))
                        && !CLES_RETIREES.iter().any(|r| bas.contains(r))
                        && k != "signal_path"
                    {
                        statique.insert(k.clone(), v.clone());
                    }
                }
            }
            json!({
                "chemin_du_signal": null,
                "chemin_statique": statique,
                "note": "zone arrêtée : pas de chemin du signal en cours ; voici la configuration de sortie qui le décide",
            })
        }
    };
    (z, chemin)
}

/// Les lignes utiles : avertissements, erreurs, et lignes qui citent un nom
/// de zone ou d'appareil ; les plus récentes, [`LIGNES_MAX`] au plus.
pub fn filtrer_journal(lignes: &[String], noms: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let cites: Vec<String> = ["zone", "appareil"]
        .iter()
        .flat_map(|c| noms.get(*c).cloned().unwrap_or_default())
        .filter(|n| n.chars().count() >= 3)
        .map(|n| n.to_lowercase())
        .collect();
    let mut gardees: Vec<String> = lignes
        .iter()
        .filter(|l| {
            let haut = l.to_uppercase();
            let bas = l.to_lowercase();
            haut.contains("WARN")
                || haut.contains("ERROR")
                || cites.iter().any(|n| bas.contains(n.as_str()))
        })
        .map(|l| l.chars().take(LIGNE_MAX).collect::<String>())
        .collect();
    if gardees.len() > LIGNES_MAX {
        gardees.drain(..gardees.len() - LIGNES_MAX);
    }
    gardees
}

fn passer_json(v: &mut Value, p: &mut Pseudonymiseur) {
    match v {
        Value::String(s) => *s = p.passer(s),
        Value::Array(a) => a.iter_mut().for_each(|x| passer_json(x, p)),
        Value::Object(o) => o.values_mut().for_each(|x| passer_json(x, p)),
        _ => {}
    }
}

/// Ce qui partirait : la consigne, le message (pseudonymisé), et la table qui
/// reste sur le serveur.
pub struct Prepare {
    pub systeme: String,
    pub message: String,
    pub pseudonymes: Pseudonymiseur,
    pub lignes_journal: usize,
}

pub fn preparer(brut: &ContexteBrut, question: &str, langue: &str) -> Prepare {
    let mut p = Pseudonymiseur::new();
    for (categorie, noms) in &brut.noms {
        p.noms(categorie, noms);
    }
    let journal = filtrer_journal(&brut.journal, &brut.noms);
    let (zone, chemin) = match &brut.zone {
        Some(z) => {
            let (z, c) = zone_pour_diagnostic(z);
            (z, c)
        }
        None => (Value::Null, Value::Null),
    };
    let mut contexte = json!({
        "question": question.chars().take(QUESTION_MAX).collect::<String>(),
        "systeme": brut.systeme,
        "zone": zone,
        "signal": chemin,
        "journal": journal,
    });
    // `anonymiser` d'abord (jetons, clés, utilisateur, chemins personnels),
    // puis la passe stricte.
    tune_core::confidentialite::anonymiser_json(&mut contexte);
    passer_json(&mut contexte, &mut p);
    Prepare {
        systeme: consigne(langue),
        message: serde_json::to_string_pretty(&contexte).unwrap_or_default(),
        pseudonymes: p,
        lignes_journal: journal.len(),
    }
}

pub fn consigne(langue: &str) -> String {
    format!(
        "You help a user understand why a zone of their music server (Tune) does not play. You receive, \
as JSON, the user's question, the server version, the zone configuration and state, its signal path \
(or, when stopped, the static output configuration that decides it) and recent log lines. Names, \
addresses and titles are replaced by placeholders such as <zone-1>, <ip-2>, <piste-1>: keep them as \
they are. Answer in {langue}. You only READ: never claim you changed anything. Reply with ONE JSON \
object and nothing else:\n\
{{\"constat\": what you see, \"causes_probables\": [{{\"cause\": string, \"indices\": [EXACT \
substrings copied from the input that support it], \"confiance\": \"faible\" | \"moyenne\" | \
\"elevee\"}}], \"a_essayer\": [{{\"geste\": what the user can try, \"ou\": screen or setting}}], \
\"rapport_utile\": true | false, \"resume_pour_le_rapport\": short neutral summary for a bug report}}\n\
Every indice MUST be copied verbatim from the input; a cause you cannot support with an indice must \
not be listed. Prefer \"probable\" causes to verdicts."
    )
}

fn texte(v: Option<&Value>, max: usize) -> Option<String> {
    v.and_then(Value::as_str)
        .map(|s| s.trim().chars().take(max).collect::<String>())
        .filter(|s| !s.is_empty())
}

fn espaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// La réponse validée (encore pseudonymisée).
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub constat: String,
    pub causes: Vec<Value>,
    pub a_essayer: Vec<Value>,
    pub rapport_utile: bool,
    pub resume: String,
    pub ecartes: Vec<Value>,
}

/// Valide la réponse contre le texte EXACT qui est parti.
pub fn valider(brut: &Value, envoye: &str) -> Result<Diagnostic, &'static str> {
    let constat = texte(brut.get("constat"), 1500).ok_or("reponse_invalide")?;
    let envoye_norme = espaces(envoye);
    let mut causes = Vec::new();
    let mut ecartes = Vec::new();
    for c in brut
        .get("causes_probables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(10)
    {
        let Some(cause) = texte(c.get("cause"), 500) else {
            ecartes.push(json!({ "cause": c, "raison": "cause_vide" }));
            continue;
        };
        let mut verifies = Vec::new();
        for i in c
            .get("indices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match i.as_str().map(|s| espaces(s.trim())) {
                Some(s) if s.chars().count() >= 3 && envoye_norme.contains(&s) => verifies.push(s),
                _ => {
                    ecartes.push(json!({ "indice": i, "raison": "indice_absent_du_texte_envoye" }))
                }
            }
        }
        if verifies.is_empty() {
            ecartes.push(json!({ "cause": cause, "raison": "aucun_indice_verifie" }));
            continue;
        }
        let confiance = match c.get("confiance").and_then(Value::as_str) {
            Some("elevee" | "élevée" | "high") => "elevee",
            Some("moyenne" | "medium") => "moyenne",
            _ => "faible",
        };
        causes.push(json!({ "cause": cause, "indices": verifies, "confiance": confiance }));
    }
    let a_essayer: Vec<Value> = brut
        .get("a_essayer")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(10)
        .filter_map(|g| {
            Some(json!({
                "geste": texte(g.get("geste"), 300)?,
                "ou": texte(g.get("ou"), 120),
            }))
        })
        .collect();
    Ok(Diagnostic {
        constat,
        causes,
        a_essayer,
        rapport_utile: brut
            .get("rapport_utile")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        resume: texte(brut.get("resume_pour_le_rapport"), 1500).unwrap_or_default(),
        ecartes,
    })
}

fn retablir_json(v: &mut Value, p: &Pseudonymiseur) {
    match v {
        Value::String(s) => *s = p.retablir(s),
        Value::Array(a) => a.iter_mut().for_each(|x| retablir_json(x, p)),
        Value::Object(o) => o.values_mut().for_each(|x| retablir_json(x, p)),
        _ => {}
    }
}

/// La réponse pour l'écran : les vrais noms rétablis dans le constat, les
/// causes et les gestes ; le résumé pour le rapport reste pseudonymisé (et
/// repasse par `anonymiser`) — c'est lui qui peut partir sur le forum.
pub fn pour_l_ecran(d: &Diagnostic, p: &Pseudonymiseur) -> Value {
    let mut affiche = json!({
        "constat": d.constat,
        "causes_probables": d.causes,
        "a_essayer": d.a_essayer,
    });
    retablir_json(&mut affiche, p);
    affiche["rapport_utile"] = json!(d.rapport_utile);
    affiche["resume_pour_le_rapport"] = json!(tune_core::confidentialite::anonymiser(&d.resume));
    affiche["ecartes"] = json!(d.ecartes);
    affiche["marque"] = json!("ia");
    affiche["actions"] = json!([]);
    affiche
}

/// Un aperçu montré, en attente de son envoi.
#[derive(Clone)]
pub struct Apercu {
    pub systeme: String,
    pub message: String,
    pub pseudonymes: Pseudonymiseur,
    pub choix: Choix,
    cree: Instant,
}

/// Les aperçus en attente, à usage UNIQUE.
#[derive(Clone, Default)]
pub struct Apercus(Arc<Mutex<HashMap<String, Apercu>>>);

fn identifiant() -> String {
    let mut o = [0u8; 16];
    if getrandom::getrandom(&mut o).is_err() {
        // Sans hasard, pas d'aperçu prévisible : l'horloge en nanosecondes.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        o.copy_from_slice(&n.to_le_bytes());
    }
    o.iter().map(|b| format!("{b:02x}")).collect()
}

impl Apercus {
    pub fn ranger(&self, p: Prepare, choix: Choix) -> String {
        let id = identifiant();
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, a| a.cree.elapsed() < APERCU_DUREE);
        if m.len() >= APERCUS_MAX
            && let Some(plus_vieux) = m.iter().min_by_key(|(_, a)| a.cree).map(|(k, _)| k.clone())
        {
            m.remove(&plus_vieux);
        }
        m.insert(
            id.clone(),
            Apercu {
                systeme: p.systeme,
                message: p.message,
                pseudonymes: p.pseudonymes,
                choix,
                cree: Instant::now(),
            },
        );
        id
    }

    /// L'aperçu `id`, retiré : il ne sert qu'à UN envoi. `None` s'il n'a
    /// jamais été montré, a déjà servi, ou a expiré.
    pub fn prendre(&self, id: &str) -> Option<Apercu> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let a = m.remove(id)?;
        (a.cree.elapsed() < APERCU_DUREE).then_some(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_indice_absent_du_texte_envoye_est_ecarte() {
        let envoye = "WARN ssdp_startup_zone_hidden_skipping zone=<zone-1>";
        let brut = json!({
            "constat": "La zone est masquée.",
            "causes_probables": [
                {"cause": "Zone masquée", "indices": ["ssdp_startup_zone_hidden_skipping"], "confiance": "elevee"},
                {"cause": "Inventée", "indices": ["timeout du DAC"]}
            ],
            "a_essayer": [{"geste": "Rendre la zone visible", "ou": "Réglages ▸ Zones"}]
        });
        let d = valider(&brut, envoye).unwrap();
        assert_eq!(d.causes.len(), 1);
        assert_eq!(d.a_essayer.len(), 1);
        assert!(
            d.ecartes
                .iter()
                .any(|e| e["raison"] == "aucun_indice_verifie")
        );
    }

    #[test]
    fn sans_constat_la_reponse_est_refusee() {
        assert_eq!(
            valider(&json!({"causes_probables": []}), "x"),
            Err("reponse_invalide")
        );
    }

    #[test]
    fn une_zone_arretee_rend_son_equivalent_statique() {
        let z = json!({"name": "Salon", "state": "stopped", "output_type": "dlna",
                       "output_device_id": "uuid:x", "signal_path": null, "current_track": {"title": "So What"}});
        let (zone, chemin) = zone_pour_diagnostic(&z);
        assert!(zone.get("current_track").is_none(), "{zone}");
        assert_eq!(chemin["chemin_statique"]["output_type"], "dlna");
        assert!(chemin["chemin_du_signal"].is_null());
    }

    #[test]
    fn un_apercu_ne_sert_qu_une_fois() {
        let a = Apercus::default();
        let id = a.ranger(
            Prepare {
                systeme: "s".into(),
                message: "m".into(),
                pseudonymes: Pseudonymiseur::new(),
                lignes_journal: 0,
            },
            Choix::Defaut,
        );
        assert!(a.prendre(&id).is_some());
        assert!(a.prendre(&id).is_none(), "un aperçu a servi deux fois");
        assert!(a.prendre("inconnu").is_none());
    }
}
