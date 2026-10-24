//! L'assistant et les collections intelligentes (#6839, « go pour tout » de
//! Bertrand du 23/10/2026).
//!
//! Aucun nouveau moteur : tout ce que le modèle propose sont des RÈGLES du
//! moteur existant (`tune_smart_http::criteres`), validées par
//! [`crate::regles::valider`] exactement comme la playlist par phrase. Rien
//! n'est créé ni modifié ici : l'écran montre la proposition, l'écart avec
//! l'existant, et l'utilisateur l'enregistre lui-même par les routes
//! EXISTANTES des collections et playlists intelligentes (créer, modifier) —
//! l'annulation est le retour aux règles d'avant, que l'écran garde.
//!
//! | Usage | Ce qui part chez le fournisseur |
//! |---|---|
//! | affiner (1) | les règles actuelles (références traduites en NOMS) et la phrase |
//! | proposer (2) | des STATISTIQUES de la bibliothèque (genres, labels, décennies, formats, étiquettes, comptes) — jamais la liste des albums |
//! | nommer (3) | les règles |
//! | pourquoi (4) | les règles et les champs de CET album |
//! | écoute (5), Premium | des agrégats d'écoute (comptes par tranche), jamais l'historique |
//!
//! Les collections d'ambiance (6) ne sont pas faites : il n'existe pas de
//! regroupement CLAP en base à nommer (seules les ambiances enregistrées, des
//! requêtes de recherche acoustique, existent), et le moteur des règles n'a
//! aucun critère acoustique — voir `docs/chantier-assistant-ia.md`.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tune_core::db::backend::{DbBackend, ToSqlValue};

use crate::regles::{self, Cible, Offert, Proposition, Referentiel};
use crate::suggestions::marque;

pub const USAGE_AFFINER: &str = "collection_affiner";
pub const USAGE_PROPOSER: &str = "collection_proposer";
pub const USAGE_NOMMER: &str = "collection_nommer";
pub const USAGE_POURQUOI: &str = "collection_pourquoi";
pub const USAGE_ECOUTE: &str = "collection_ecoute";

/// Collections proposées, au plus et au moins attendu.
pub const PROPOSITIONS_MAX: usize = 10;
pub const PROPOSITIONS_MIN: usize = 5;
/// Règles envoyées ou reçues pour une collection, au plus.
pub const REGLES_MAX: usize = 30;
/// Longueur d'un nom, d'une description, d'une explication.
pub const NOM_MAX: usize = 60;
pub const DESCRIPTION_MAX: usize = 160;
pub const EXPLICATION_MAX: usize = 600;

fn couper(s: &str, max: usize) -> String {
    s.trim()
        .chars()
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn lignes(db: &Arc<dyn DbBackend>, sql: &str) -> Vec<(String, i64)> {
    db.query_many(sql, &[])
        .unwrap_or_default()
        .iter()
        .filter_map(|l| {
            let k = l.first()?;
            let k = k
                .as_string()
                .or_else(|| k.as_i64().map(|n| n.to_string()))?;
            let k = k.trim().to_string();
            let n = l.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            (!k.is_empty()).then_some((k, n))
        })
        .collect()
}

fn paires(v: Vec<(String, i64)>) -> Value {
    Value::Array(v.into_iter().map(|(k, n)| json!([k, n])).collect())
}

// ---------------------------------------------------------------------------
// (1) Affiner : les règles actuelles, lisibles par le modèle
// ---------------------------------------------------------------------------

/// Les règles enregistrées, avec leurs références (`7`, `smart:5`) traduites
/// en NOMS : le modèle ne connaît que les noms, et la validation les
/// retraduit. Une référence introuvable reste telle quelle (elle sera
/// refusée à la validation et l'écran le dira).
pub fn regles_lisibles(rules: &[Value], offerts: &[Offert], refs: &Referentiel) -> Vec<Value> {
    rules
        .iter()
        .take(REGLES_MAX)
        .map(|r| {
            let champ = r.get("field").and_then(Value::as_str).unwrap_or("");
            let famille = offerts
                .iter()
                .find(|o| o.nom == champ)
                .map(|o| o.famille.as_str())
                .unwrap_or("");
            let liste = match famille {
                "tag_ref" => &refs.etiquettes,
                "collection_ref" => &refs.collections,
                _ => return r.clone(),
            };
            let valeur = r.get("value").map(|v| match v {
                Value::String(s) => s.clone(),
                autre => autre.to_string(),
            });
            match valeur.and_then(|v| liste.iter().find(|x| x.valeur == v)) {
                Some(x) => {
                    let mut r = r.clone();
                    r["value"] = json!(x.nom);
                    r
                }
                None => r.clone(),
            }
        })
        .collect()
}

/// La consigne d'affinage : celle de la playlist par phrase, plus les règles
/// actuelles à MODIFIER.
pub fn consigne_affiner(base: &str) -> String {
    format!(
        "{base}\nYou receive the CURRENT rules of an existing smart list and a request to change them. \
Reply with the COMPLETE new set of rules (keep the current rules the request does not touch). \
Never drop every rule: a list without rules would match the whole library."
    )
}

/// L'écart entre deux jeux de règles, règle par règle.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EcartRegles {
    pub ajoutees: Vec<Value>,
    pub retirees: Vec<Value>,
    pub inchangees: usize,
}

fn cle_regle(r: &Value) -> String {
    let op = r
        .get("op")
        .or_else(|| r.get("operator"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "{}\u{1f}{}\u{1f}{}",
        r.get("field").and_then(Value::as_str).unwrap_or(""),
        op,
        r.get("value").cloned().unwrap_or(Value::Null)
    )
}

pub fn ecart(avant: &[Value], apres: &[Value]) -> EcartRegles {
    let a: BTreeSet<String> = avant.iter().map(cle_regle).collect();
    let b: BTreeSet<String> = apres.iter().map(cle_regle).collect();
    EcartRegles {
        ajoutees: apres
            .iter()
            .filter(|r| !a.contains(&cle_regle(r)))
            .cloned()
            .collect(),
        retirees: avant
            .iter()
            .filter(|r| !b.contains(&cle_regle(r)))
            .cloned()
            .collect(),
        inchangees: apres.iter().filter(|r| a.contains(&cle_regle(r))).count(),
    }
}

// ---------------------------------------------------------------------------
// (2) Proposer : les seules statistiques de la bibliothèque
// ---------------------------------------------------------------------------

/// Les statistiques envoyées : des COMPTES, jamais un titre d'album.
pub fn statistiques(db: &Arc<dyn DbBackend>, refs: &Referentiel) -> Value {
    let total = db
        .query_one("SELECT COUNT(*) FROM albums", &[])
        .ok()
        .flatten()
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    json!({
        "albums": total,
        "genres": paires(lignes(db, "SELECT genre, COUNT(*) AS n FROM albums WHERE genre IS NOT NULL AND genre <> '' GROUP BY genre ORDER BY n DESC LIMIT 40")),
        "labels": paires(lignes(db, "SELECT label, COUNT(*) AS n FROM albums WHERE label IS NOT NULL AND label <> '' GROUP BY label ORDER BY n DESC LIMIT 25")),
        "decennies": paires(lignes(db, "SELECT (year / 10) * 10 AS d, COUNT(*) AS n FROM albums WHERE year IS NOT NULL AND year > 0 GROUP BY d ORDER BY d")),
        "formats": paires(lignes(db, "SELECT format, COUNT(*) AS n FROM albums WHERE format IS NOT NULL AND format <> '' GROUP BY format ORDER BY n DESC LIMIT 10")),
        "haute_resolution": paires(lignes(db, "SELECT CASE WHEN sample_rate > 48000 OR bit_depth > 16 THEN 'hi-res' ELSE 'cd-or-less' END AS q, COUNT(*) AS n FROM albums WHERE sample_rate IS NOT NULL GROUP BY q")),
        "compilations": db.query_one("SELECT COUNT(*) FROM albums WHERE is_compilation = 1", &[]).ok().flatten()
            .and_then(|l| l.first().and_then(|v| v.as_i64())).unwrap_or(0),
        "etiquettes": refs.etiquettes.iter().map(|r| r.nom.clone()).collect::<Vec<_>>(),
        "collections_existantes": refs.collections.iter().map(|r| r.nom.clone()).collect::<Vec<_>>(),
    })
}

/// La consigne des propositions (la liste FERMÉE des critères en tête).
pub fn consigne_proposer(base: &str, langue: &str) -> String {
    format!(
        "{base}\nInstead of one request, you receive STATISTICS of a music library. Propose between \
{PROPOSITIONS_MIN} and {PROPOSITIONS_MAX} smart collections that would be useful for THIS library, \
built only from the fields above and values seen in the statistics. Do not repeat an existing \
collection. Reply with ONE JSON object and nothing else:\n\
{{\"collections\": [{{\"name\": short name in {langue}, \"description\": one line in {langue}, \
\"match_mode\": \"all\" | \"any\", \"rules\": [{{\"field\", \"op\", \"value\"}}]}}]}}"
    )
}

/// Une collection proposée et validée.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Proposee {
    pub name: String,
    pub description: String,
    pub match_mode: String,
    pub rules: Vec<Value>,
    pub refusees: Vec<regles::Refusee>,
    /// Le nom et la description sont écrits par l'IA.
    pub marque: &'static str,
}

/// Valide une liste de collections proposées : chacune passe par
/// [`regles::valider`] ; une collection sans règle exploitable est écartée
/// (#6731), un nom déjà pris aussi.
pub fn valider_propositions(
    brut: &Value,
    offerts: &[Offert],
    refs: &Referentiel,
) -> Result<(Vec<Proposee>, Vec<Value>), &'static str> {
    let liste = brut
        .get("collections")
        .and_then(Value::as_array)
        .ok_or("reponse_invalide")?;
    let pris: BTreeSet<String> = refs
        .collections
        .iter()
        .map(|r| r.nom.to_lowercase())
        .collect();
    let mut retenues: Vec<Proposee> = Vec::new();
    let mut ecartees = Vec::new();
    for c in liste.iter().take(PROPOSITIONS_MAX * 2) {
        let p = match regles::valider(c, offerts, Cible::Collection, refs) {
            Ok(p) => p,
            Err(r) => {
                ecartees.push(json!({ "collection": c, "raison": r }));
                continue;
            }
        };
        let raison = if p.rules.is_empty() {
            Some("aucune_regle")
        } else if p.name.trim().is_empty() {
            Some("sans_nom")
        } else if pris.contains(&p.name.to_lowercase())
            || retenues
                .iter()
                .any(|r| r.name.to_lowercase() == p.name.to_lowercase())
        {
            Some("nom_deja_pris")
        } else if retenues.len() >= PROPOSITIONS_MAX {
            Some("trop_de_collections")
        } else {
            None
        };
        if let Some(r) = raison {
            ecartees.push(json!({ "collection": c, "raison": r }));
            continue;
        }
        retenues.push(Proposee {
            name: couper(&p.name, NOM_MAX),
            description: couper(
                c.get("description").and_then(Value::as_str).unwrap_or(""),
                DESCRIPTION_MAX,
            ),
            match_mode: p.match_mode,
            rules: p.rules,
            refusees: p.refusees,
            marque: "ia",
        });
    }
    if retenues.is_empty() {
        return Err("aucune_collection");
    }
    Ok((retenues, ecartees))
}

// ---------------------------------------------------------------------------
// (3) Nommer et décrire
// ---------------------------------------------------------------------------

pub fn consigne_nommer(cible: Cible, langue: &str) -> String {
    format!(
        "You name a smart {} of a music library from its rules. Reply with ONE JSON object and nothing \
else: {{\"name\": at most {NOM_MAX} characters in {langue}, \"description\": one line, at most \
{DESCRIPTION_MAX} characters in {langue}}}. Describe only what the rules select; never claim \
anything the rules do not say.",
        cible.nom()
    )
}

/// Un nom et une description validés, marqués IA.
pub fn valider_nom(brut: &Value) -> Result<Value, &'static str> {
    let nom = brut
        .get("name")
        .and_then(Value::as_str)
        .map(|s| couper(s, NOM_MAX))
        .filter(|s| !s.is_empty())
        .ok_or("reponse_invalide")?;
    let description = brut
        .get("description")
        .and_then(Value::as_str)
        .map(|s| couper(s, DESCRIPTION_MAX))
        .unwrap_or_default();
    Ok(json!({ "name": nom, "description": description, "marque": "ia" }))
}

// ---------------------------------------------------------------------------
// (4) Pourquoi cet album est là
// ---------------------------------------------------------------------------

/// Les champs de l'album que les règles des collections peuvent lire.
pub fn champs_album(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<Value>, String> {
    let l = db.query_one(
        &format!(
            "SELECT a.title, ar.name, a.year, a.original_year, a.genre, a.label, a.format, \
             a.sample_rate, a.bit_depth, a.track_count, a.is_compilation \
             FROM albums a LEFT JOIN artists ar ON ar.id = a.artist_id WHERE a.id = {}",
            marque(db, 1)
        ),
        &[&album_id as &dyn ToSqlValue],
    )?;
    let Some(l) = l else { return Ok(None) };
    let t = |i: usize| l.get(i).and_then(|v| v.as_string());
    let n = |i: usize| l.get(i).and_then(|v| v.as_i64());
    Ok(Some(json!({
        "title": t(0), "artist_name": t(1), "year": n(2), "original_year": n(3),
        "genre": t(4), "label": t(5), "format": t(6), "sample_rate": n(7),
        "bit_depth": n(8), "track_count": n(9),
        "is_compilation": n(10).map(|x| x != 0),
    })))
}

pub fn consigne_pourquoi(langue: &str) -> String {
    format!(
        "You explain, in plain {langue}, why ONE album belongs to a smart collection. You receive the \
numbered rules of the collection, its match mode, and the album's fields. Reply with ONE JSON object \
and nothing else: {{\"explication\": at most {EXPLICATION_MAX} characters, \"regles\": [indices of \
the rules that select this album]}}. Cite only fields that are given; when the given fields cannot \
explain a rule (play counts, tags…), say so instead of guessing."
    )
}

pub fn valider_pourquoi(brut: &Value, nb_regles: usize) -> Result<Value, &'static str> {
    let explication = brut
        .get("explication")
        .and_then(Value::as_str)
        .map(|s| couper(s, EXPLICATION_MAX))
        .filter(|s| !s.is_empty())
        .ok_or("reponse_invalide")?;
    let mut regles = Vec::new();
    for v in brut
        .get("regles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match v.as_u64() {
            Some(i) if (i as usize) < nb_regles => {
                if !regles.contains(&i) {
                    regles.push(i);
                }
            }
            _ => return Err("regle_inconnue"),
        }
    }
    Ok(json!({ "explication": explication, "regles": regles, "marque": "ia" }))
}

// ---------------------------------------------------------------------------
// (5) D'après l'écoute (Premium)
// ---------------------------------------------------------------------------

/// Des AGRÉGATS d'écoute : la répartition des albums par nombre d'écoutes et
/// par ancienneté de la dernière écoute, et les genres les plus écoutés.
/// Aucun titre, aucun horodatage précis.
pub fn agregats_ecoute(db: &Arc<dyn DbBackend>, profil: Option<i64>) -> Value {
    // Le profil qui demande, et lui seul (un entier : rien à échapper).
    let filtre = profil
        .map(|p| format!(" AND h.profile_id = {p}"))
        .unwrap_or_default();
    json!({
        "albums_par_ecoutes": paires(lignes(db, &format!(
            "SELECT CASE WHEN n = 0 THEN '0' WHEN n < 3 THEN '1-2' WHEN n < 10 THEN '3-9' ELSE '10+' END AS b, COUNT(*) \
             FROM (SELECT a.id, (SELECT COUNT(*) FROM listen_history h JOIN tracks t ON CAST(t.id AS TEXT) = CAST(h.track_id AS TEXT) \
             WHERE t.album_id = a.id{filtre}) AS n FROM albums a) x GROUP BY b ORDER BY b"))),
        "genres_les_plus_ecoutes": paires(lignes(db, &format!(
            "SELECT a.genre, COUNT(*) AS n FROM listen_history h JOIN tracks t ON CAST(t.id AS TEXT) = CAST(h.track_id AS TEXT) \
             JOIN albums a ON a.id = t.album_id WHERE a.genre IS NOT NULL AND a.genre <> ''{filtre} GROUP BY a.genre ORDER BY n DESC LIMIT 15"))),
        "ecoutes_total": db.query_one(&format!("SELECT COUNT(*) FROM listen_history h WHERE 1 = 1{filtre}"), &[]).ok().flatten()
            .and_then(|l| l.first().and_then(|v| v.as_i64())).unwrap_or(0),
    })
}

pub fn consigne_ecoute(base: &str, langue: &str) -> String {
    format!(
        "{base}\nYou receive AGGREGATES of the user's listening (counts only). Propose between 3 and \
{PROPOSITIONS_MAX} smart collections based on listening habits — for example albums to rediscover \
(played several times, but not in the last year), favourites, recent additions never played — \
using only the fields above (play_count, last_played_at, added_at, favorite, rating, genre…). \
Reply with ONE JSON object and nothing else:\n\
{{\"collections\": [{{\"name\": short name in {langue}, \"description\": one line in {langue}, \
\"match_mode\": \"all\" | \"any\", \"rules\": [{{\"field\", \"op\", \"value\"}}]}}]}}"
    )
}

/// La proposition d'affinage validée, et son écart avec l'existant.
#[derive(Clone, Debug, Serialize)]
pub struct Affinage {
    pub proposition: Proposition,
    pub ecart: EcartRegles,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_ecart_dit_ce_qui_est_ajoute_et_retire() {
        let avant = vec![
            json!({"field": "genre", "op": "contains", "value": "Jazz"}),
            json!({"field": "year", "op": ">=", "value": 1960}),
        ];
        let apres = vec![
            json!({"field": "genre", "op": "contains", "value": "Jazz"}),
            json!({"field": "title", "op": "not_contains", "value": "Best of"}),
        ];
        let e = ecart(&avant, &apres);
        assert_eq!(e.inchangees, 1);
        assert_eq!(e.ajoutees.len(), 1);
        assert_eq!(e.retirees, vec![avant[1].clone()]);
    }

    #[test]
    fn pourquoi_refuse_une_regle_qui_n_existe_pas() {
        assert!(valider_pourquoi(&json!({"explication": "x", "regles": [0, 1]}), 2).is_ok());
        assert_eq!(
            valider_pourquoi(&json!({"explication": "x", "regles": [5]}), 2),
            Err("regle_inconnue")
        );
        assert_eq!(
            valider_pourquoi(&json!({"regles": []}), 2),
            Err("reponse_invalide")
        );
    }

    #[test]
    fn un_nom_vide_est_refuse_et_un_nom_long_coupe() {
        assert_eq!(valider_nom(&json!({"name": "  "})), Err("reponse_invalide"));
        let v = valider_nom(&json!({"name": "x".repeat(200)})).unwrap();
        assert_eq!(v["name"].as_str().unwrap().chars().count(), NOM_MAX);
        assert_eq!(v["marque"], "ia");
    }
}
