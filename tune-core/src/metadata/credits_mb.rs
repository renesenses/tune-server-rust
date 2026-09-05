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
//! **Descendu dans tune-core (CRD-1).** Le parseur est UNIQUE : les routes
//! l'empruntent par `routes::library::credits_mb`, et les passes de fond à
//! venir (CRD-3 appariement par score, CRD-5 passe automatique) l'emprunteront
//! d'ici. `credit_enricher`, l'ancien doublon que personne n'appelait, a
//! disparu avec ce déplacement.

use serde_json::Value;

use super::instruments::{canoniser_instrument, normaliser};

/// Une ligne de `track_credits` prête à écrire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LigneCredit {
    pub artist_name: String,
    pub role: String,
    /// Instrument CANONISÉ (§4 de l'issue), ou `None` pour les rôles qui n'en
    /// portent pas (producteur, ingénieur…).
    pub instrument: Option<String>,
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
pub fn lignes_artist_credit(data: &Value) -> Vec<LigneCredit> {
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
pub fn lignes_relations(data: &Value) -> Vec<LigneCredit> {
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
pub fn lignes_credits(data: &Value) -> Vec<LigneCredit> {
    let mut out = lignes_artist_credit(data);
    out.extend(lignes_relations(data));
    out
}

/// Un enregistrement MusicBrainz retenu pour une piste sans MBID (CRD-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatEnregistrement {
    pub mbid: String,
    pub score: i32,
}

/// En dessous de ce score, on ne retient RIEN : mieux vaut aucun crédit qu'un
/// crédit d'un autre morceau. Titre exact + artiste exact = 75 ; titre
/// approchant + artiste exact + durée à moins de 3 s = 65 ; titre approchant
/// + artiste exact sans durée = 45, refusé.
pub const SCORE_MINIMAL_ENREGISTREMENT: i32 = 60;

/// Score d'un enregistrement candidat contre la piste. Chaque composante est
/// nommée pour que le seuil se lise : titre (45 exact, 15 si l'un contient
/// l'autre), artiste-crédit (30 exact, 10 contenu), durée (20 à ±3 s, 8 à
/// ±10 s, −20 au-delà de 30 s, 0 sans durée), et le dixième du score que
/// MusicBrainz attribue lui-même à sa réponse.
pub fn score_enregistrement(
    titre: &str,
    artiste: &str,
    duree_ms: Option<i64>,
    enregistrement: &Value,
) -> i32 {
    let t = normaliser(titre);
    let a = normaliser(artiste);
    let rec_titre = normaliser(enregistrement["title"].as_str().unwrap_or(""));
    let mut score = 0;
    if !t.is_empty() && rec_titre == t {
        score += 45;
    } else if !t.is_empty()
        && !rec_titre.is_empty()
        && (rec_titre.contains(&t) || t.contains(&rec_titre))
    {
        score += 15;
    }
    if !a.is_empty() {
        let noms: Vec<String> = enregistrement["artist-credit"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|ac| {
                        ac["name"]
                            .as_str()
                            .or_else(|| ac["artist"]["name"].as_str())
                    })
                    .map(normaliser)
                    .collect()
            })
            .unwrap_or_default();
        if noms.contains(&a) {
            score += 30;
        } else if noms
            .iter()
            .any(|n| n.contains(&a) || a.contains(n.as_str()))
        {
            score += 10;
        }
    }
    if let (Some(d), Some(l)) = (
        duree_ms.filter(|d| *d > 0),
        enregistrement["length"].as_i64(),
    ) {
        let ecart = (d - l).abs();
        score += if ecart <= 3_000 {
            20
        } else if ecart <= 10_000 {
            8
        } else if ecart > 30_000 {
            -20
        } else {
            0
        };
    }
    score += enregistrement["score"].as_i64().unwrap_or(0).clamp(0, 100) as i32 / 10;
    score
}

/// Choisit, parmi les `recordings` d'une réponse de recherche MusicBrainz,
/// celui qui dépasse le seuil avec le meilleur score — le premier à égalité,
/// et `None` si aucun ne l'atteint. Le « premier résultat » d'avant prenait
/// un morceau homonyme d'un autre artiste sans sourciller.
pub fn choisir_l_enregistrement(
    titre: &str,
    artiste: &str,
    duree_ms: Option<i64>,
    reponse: &Value,
) -> Option<CandidatEnregistrement> {
    let recordings = reponse["recordings"].as_array()?;
    let mut meilleur: Option<CandidatEnregistrement> = None;
    for rec in recordings {
        let Some(mbid) = rec["id"].as_str() else {
            continue;
        };
        let score = score_enregistrement(titre, artiste, duree_ms, rec);
        if score >= SCORE_MINIMAL_ENREGISTREMENT
            && meilleur.as_ref().is_none_or(|m| score > m.score)
        {
            meilleur = Some(CandidatEnregistrement {
                mbid: mbid.to_string(),
                score,
            });
        }
    }
    meilleur
}

/// Cherche l'enregistrement d'une piste sans MBID et le retient seulement
/// au-dessus du seuil. Le client est fourni par l'appelant (couture HTTP
/// unique du serveur). Cinq résultats suffisent : le score tranche, pas la
/// position.
pub async fn rechercher_l_enregistrement(
    client: &reqwest::Client,
    titre: &str,
    artiste: &str,
    duree_ms: Option<i64>,
) -> Option<CandidatEnregistrement> {
    let titre = titre.trim();
    if titre.is_empty() {
        return None;
    }
    let echappe = |s: &str| s.replace('"', " ");
    let query = if artiste.trim().is_empty() {
        format!("recording:\"{}\"", echappe(titre))
    } else {
        format!(
            "recording:\"{}\" AND artist:\"{}\"",
            echappe(titre),
            echappe(artiste.trim())
        )
    };
    let resp = client
        .get("https://musicbrainz.org/ws/2/recording")
        .query(&[("query", query.as_str()), ("limit", "5"), ("fmt", "json")])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    choisir_l_enregistrement(titre, artiste, duree_ms, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reponse(recs: Vec<Value>) -> Value {
        json!({ "recordings": recs })
    }
    fn rec(id: &str, titre: &str, artiste: &str, length: Option<i64>, score_mb: i64) -> Value {
        let mut r = json!({ "id": id, "title": titre, "score": score_mb,
            "artist-credit": [{ "name": artiste, "artist": { "name": artiste } }] });
        if let Some(l) = length {
            r["length"] = json!(l);
        }
        r
    }

    /// CRD-3 : le premier résultat n'est plus roi. Un homonyme d'un autre
    /// artiste, mieux classé par MusicBrainz, est écarté au profit de
    /// l'enregistrement dont titre, artiste et durée concordent.
    #[test]
    fn le_score_prefere_la_concordance_au_premier_resultat() {
        let r = reponse(vec![
            rec(
                "mb-homonyme",
                "Hallelujah",
                "Jeff Buckley",
                Some(413_000),
                100,
            ),
            rec(
                "mb-le-bon",
                "Hallelujah",
                "Leonard Cohen",
                Some(274_000),
                90,
            ),
        ]);
        let choix =
            choisir_l_enregistrement("Hallelujah", "Leonard Cohen", Some(275_000), &r).unwrap();
        assert_eq!(choix.mbid, "mb-le-bon");
        assert!(choix.score >= 45 + 30 + 20 + 9, "{}", choix.score);
    }

    /// Sous le seuil, rien : un titre approchant chez le bon artiste mais sans
    /// durée ne suffit pas ; un titre exact du bon artiste suffit même sans
    /// durée.
    #[test]
    fn sous_le_seuil_aucun_enregistrement_n_est_retenu() {
        let approchant = reponse(vec![rec(
            "mb-approx",
            "Hallelujah (Live)",
            "Leonard Cohen",
            None,
            80,
        )]);
        assert_eq!(
            choisir_l_enregistrement("Hallelujah", "Leonard Cohen", None, &approchant),
            None
        );
        let exact = reponse(vec![rec(
            "mb-exact",
            "Hallelujah",
            "Leonard Cohen",
            None,
            80,
        )]);
        assert_eq!(
            choisir_l_enregistrement("Hallelujah", "Leonard Cohen", None, &exact).map(|c| c.mbid),
            Some("mb-exact".to_string())
        );
        assert_eq!(
            choisir_l_enregistrement("Hallelujah", "Leonard Cohen", None, &json!({})),
            None
        );
    }

    /// Les composantes du score, une à une : une durée à plus de 30 s pénalise,
    /// la casse et la ponctuation ne comptent pas.
    #[test]
    fn les_composantes_du_score_se_lisent_une_a_une() {
        let r = rec("x", "Don't Stop Me Now", "Queen", Some(209_000), 0);
        assert_eq!(
            score_enregistrement("don't stop me now", "queen", Some(210_000), &r),
            45 + 30 + 20
        );
        assert_eq!(
            score_enregistrement("Don't Stop Me Now", "Queen", Some(260_000), &r),
            45 + 30 - 20
        );
        assert_eq!(
            score_enregistrement("Don't Stop Me Now", "Queen", None, &r),
            45 + 30
        );
        assert_eq!(score_enregistrement("Stop Me", "Queen", None, &r), 15 + 30);
        assert_eq!(
            score_enregistrement("Autre chose", "Quelqu'un", None, &r),
            0
        );
    }

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
