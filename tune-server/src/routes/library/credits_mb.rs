//! Analyse HORS RÉSEAU des relations MusicBrainz d'un enregistrement (#2799).
//!
//! Prend le JSON rendu par
//! `/ws/2/recording/<mbid>?inc=artist-credits+artist-rels` et rend les lignes à
//! écrire dans `track_credits`. Aucun appel sortant : tout est testable sur des
//! fixtures.
//!
//! **Pourquoi un module.** Les trois routes d'enrichissement des crédits
//! (`…/tracks/{id}/credits/enrich`, `…/albums/{id}/credits/enrich`,
//! `/library/enrich-credits`) portaient TROIS copies du même parseur, toutes
//! limitées à `attributes[0]` et toutes sans filtre de type. Corriger une copie
//! sur trois est le défaut dominant de cette zone ; elles empruntent désormais
//! ce module.
//!
//! ⚠️ À ne pas confondre avec `tune_core::metadata::credit_enricher`, qui sait
//! déjà faire une partie de ça — et que **personne n'appelle** (`git grep
//! credit_enricher` ne rend que sa propre déclaration `pub mod`). C'est bien
//! CE module-ci que la ROUTE emprunte.

use serde_json::Value;

use tune_core::metadata::instruments::{canoniser_instrument, normaliser};

/// Clé `settings` d'avancement de `POST /library/enrich-credits` (#2799).
///
/// Même forme et même cycle de vie que `enrich_all_status` : `running` au
/// lancement puis à chaque jalon, `done` à la fin normale. Elle vit ICI, dans
/// le module `pub(crate)`, parce que `startup.rs` la neutralise au démarrage —
/// sans quoi un arrêt en cours de passe laisse `running` en base pour toujours
/// et le bouton reste grisé (défaut #2002). La constante plutôt que le
/// littéral : renommer la clé d'un côté ne peut plus désynchroniser l'autre.
pub(crate) const REGLAGE_AVANCEMENT_CREDITS: &str = "enrich_credits_status";

/// Une ligne de `track_credits` prête à écrire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LigneCredit {
    pub(super) artist_name: String,
    pub(super) role: String,
    /// Instrument CANONISÉ (§4 de l'issue), ou `None` pour les rôles qui n'en
    /// portent pas (producteur, ingénieur…).
    pub(super) instrument: Option<String>,
}

/// Types de relation MusicBrainz retenus, et le rôle canonique écrit en base.
///
/// Tout le reste est du bruit pour une fiche de crédits — `misc`,
/// `phonographic copyright`, `legal representation`, `booking`… — et
/// remplissait la table jusqu'ici : la boucle écrivait `rel["type"]` tel quel,
/// sans filtre. Une Smart Collection `role: producer` ramassait alors autant de
/// lignes parasites que de vraies.
///
/// La liste est délibérément GÉNÉREUSE côté musique : mieux vaut une ligne de
/// trop qu'un sideman perdu. Elle ne coupe que ce qui n'a rien à faire sur une
/// pochette.
fn role_canonique(rel_type: &str) -> Option<&'static str> {
    Some(match rel_type {
        "instrument" | "performer" | "performing orchestra" => "performer",
        "vocal" => "vocal",
        "conductor" => "conductor",
        "producer" => "producer",
        "engineer" | "recording" | "audio" => "engineer",
        "mastering" => "mastering",
        "mix" | "mix-DJ" => "mixer",
        "remixer" => "remixer",
        "arranger" | "instrument arranger" | "vocal arranger" | "orchestrator" => "arranger",
        "composer" => "composer",
        "lyricist" | "writer" | "librettist" => "writer",
        "programming" => "programming",
        _ => return None,
    })
}

/// Attributs de relation qui QUALIFIENT le crédit sans nommer d'instrument.
///
/// MusicBrainz mélange les deux dans le même tableau : `["additional",
/// "guitar"]` veut dire « guitare additionnelle », pas deux instruments. Pris
/// pour un instrument, `additional` produisait une facette « additional » dans
/// les Smart Collections.
const ATTRIBUTS_NON_INSTRUMENT: &[&str] = &[
    "additional",
    "guest",
    "solo",
    "co",
    "assistant",
    "associate",
    "executive",
    "lead",
    "minor",
    "original",
    "current",
    "past",
];

// La table des familles d'instruments, `normaliser` et
// `canoniser_instrument` vivent desormais dans
// `tune_core::metadata::instruments` : `tune-smart-http` compile la MEME
// canonisation cote lecture (regles `credit` des Smart Collections) et ne
// peut pas dependre de `tune-server`. Une copie ici et les deux cotes
// divergeraient au premier ajout de famille.
/// Vrai si l'attribut qualifie le crédit au lieu de nommer un instrument.
fn est_qualificatif(attr: &str) -> bool {
    let n = normaliser(attr);
    ATTRIBUTS_NON_INSTRUMENT.contains(&n.as_str())
}

/// Instruments canonisés d'une relation, dédoublonnés en gardant l'ordre.
///
/// `["piano", "grand piano"]` — le cas cité par l'issue — rend **une** entrée
/// `piano`, pas deux lignes identiques.
fn instruments_de_la_relation(rel: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(attrs) = rel.get("attributes").and_then(|v| v.as_array()) else {
        return out;
    };
    for a in attrs {
        let Some(s) = a.as_str() else { continue };
        if est_qualificatif(s) {
            continue;
        }
        let canon = canoniser_instrument(s);
        if canon.is_empty() || out.contains(&canon) {
            continue;
        }
        out.push(canon);
    }
    out
}

/// Lignes issues de `artist-credit` (les interprètes principaux du morceau).
///
/// Contrat inchangé : rôle `artist`, un par entrée, dans l'ordre.
pub(super) fn lignes_artist_credit(data: &Value) -> Vec<LigneCredit> {
    let Some(credits) = data.get("artist-credit").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    credits
        .iter()
        .map(|credit| LigneCredit {
            artist_name: credit
                .get("name")
                .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string(),
            role: "artist".to_string(),
            instrument: None,
        })
        .collect()
}

/// Lignes issues de `relations` : sidemen, ingénieurs, producteurs (§3).
///
/// Deux changements par rapport aux trois copies qu'elle remplace :
/// 1. **tous** les attributs d'instrument sont écrits — une ligne par couple
///    artiste × instrument — au lieu du seul `attributes[0]` ;
/// 2. les types de relation sont FILTRÉS par [`role_canonique`].
pub(super) fn lignes_relations(data: &Value) -> Vec<LigneCredit> {
    let Some(relations) = data.get("relations").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rel in relations {
        let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(role) = role_canonique(rel_type) else {
            continue;
        };
        let Some(name) = rel
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };

        let instruments = instruments_de_la_relation(rel);
        if instruments.is_empty() {
            // Un chant sans attribut reste un chant : sans ce repli, une
            // Smart Collection `instrument: vocals` manquait tous les
            // crédits `vocal` nus.
            let instrument = (role == "vocal").then(|| "vocals".to_string());
            out.push(LigneCredit {
                artist_name: name.to_string(),
                role: role.to_string(),
                instrument,
            });
            continue;
        }
        for instrument in instruments {
            out.push(LigneCredit {
                artist_name: name.to_string(),
                role: role.to_string(),
                instrument: Some(instrument),
            });
        }
    }
    out
}

/// Toutes les lignes à écrire pour un enregistrement, `artist-credit` d'abord.
pub(super) fn lignes_credits(data: &Value) -> Vec<LigneCredit> {
    let mut out = lignes_artist_credit(data);
    out.extend(lignes_relations(data));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Les trois tests du canon d'instrument suivent la table : ils vivent
    // dans `tune_core::metadata::instruments`.

    #[test]
    fn tous_les_instruments_sont_ecrits_pas_seulement_le_premier() {
        let data = json!({"relations": [{
            "type": "instrument",
            "artist": {"name": "Bill Evans"},
            "attributes": ["piano", "celesta"],
        }]});
        let lignes = lignes_relations(&data);
        assert_eq!(lignes.len(), 2, "{lignes:?}");
        assert_eq!(lignes[0].instrument.as_deref(), Some("piano"));
        assert_eq!(lignes[1].instrument.as_deref(), Some("celesta"));
    }

    /// Le cas cité par l'issue : `piano` + `grand piano` sont le MÊME
    /// instrument une fois canonisés — une ligne, pas deux doublons.
    #[test]
    fn variantes_du_meme_instrument_dedoublonnees() {
        let data = json!({"relations": [{
            "type": "instrument",
            "artist": {"name": "Bill Evans"},
            "attributes": ["piano", "grand piano"],
        }]});
        let lignes = lignes_relations(&data);
        assert_eq!(lignes.len(), 1, "{lignes:?}");
        assert_eq!(lignes[0].instrument.as_deref(), Some("piano"));
    }

    #[test]
    fn qualificatif_n_est_pas_un_instrument() {
        let data = json!({"relations": [{
            "type": "instrument",
            "artist": {"name": "Guest Star"},
            "attributes": ["additional", "guest", "electric guitar"],
        }]});
        let lignes = lignes_relations(&data);
        assert_eq!(lignes.len(), 1, "{lignes:?}");
        assert_eq!(lignes[0].instrument.as_deref(), Some("guitar"));
    }

    #[test]
    fn types_de_relation_parasites_ignores() {
        let data = json!({"relations": [
            {"type": "misc", "artist": {"name": "Bruit"}},
            {"type": "phonographic copyright", "artist": {"name": "Label SA"}},
            {"type": "legal representation", "artist": {"name": "Cabinet X"}},
            {"type": "producer", "artist": {"name": "Teo Macero"}},
        ]});
        let lignes = lignes_relations(&data);
        assert_eq!(lignes.len(), 1, "{lignes:?}");
        assert_eq!(lignes[0].artist_name, "Teo Macero");
        assert_eq!(lignes[0].role, "producer");
    }

    #[test]
    fn roles_utiles_conserves_et_canonises() {
        let data = json!({"relations": [
            {"type": "mix", "artist": {"name": "M"}},
            {"type": "engineer", "artist": {"name": "E"}},
            {"type": "conductor", "artist": {"name": "C"}},
            {"type": "performing orchestra", "artist": {"name": "O"}},
        ]});
        let roles: Vec<String> = lignes_relations(&data)
            .iter()
            .map(|l| l.role.clone())
            .collect();
        assert_eq!(roles, ["mixer", "engineer", "conductor", "performer"]);
    }

    #[test]
    fn chant_sans_attribut_reste_du_chant() {
        let data = json!({"relations": [
            {"type": "vocal", "artist": {"name": "Billie"}},
        ]});
        let lignes = lignes_relations(&data);
        assert_eq!(lignes[0].instrument.as_deref(), Some("vocals"));
    }

    /// TÉMOIN ANTI-RÉGRESSION : `artist-credit` garde son contrat mot pour mot
    /// — rôle `artist`, ordre d'origine, repli `Unknown`.
    #[test]
    fn temoin_artist_credit_inchange() {
        let data = json!({"artist-credit": [
            {"name": "Miles Davis"},
            {"artist": {"name": "John Coltrane"}},
            {},
        ]});
        let lignes = lignes_artist_credit(&data);
        assert_eq!(lignes.len(), 3);
        assert_eq!(lignes[0].artist_name, "Miles Davis");
        assert_eq!(lignes[1].artist_name, "John Coltrane");
        assert_eq!(lignes[2].artist_name, "Unknown");
        assert!(lignes.iter().all(|l| l.role == "artist"));
        assert!(lignes.iter().all(|l| l.instrument.is_none()));
    }

    #[test]
    fn relation_sans_artiste_ignoree() {
        let data = json!({"relations": [
            {"type": "instrument", "attributes": ["piano"]},
            {"type": "instrument", "artist": {"name": "   "}, "attributes": ["piano"]},
        ]});
        assert!(lignes_relations(&data).is_empty());
    }
}
