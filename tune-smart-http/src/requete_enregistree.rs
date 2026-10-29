//! #6287, étape 4 — enregistrer une requête IMBRIQUÉE comme playlist
//! intelligente.
//!
//! Le format des règles est plat (`rules` + `match_mode`) : une requête niée
//! ou parenthésée n'y a pas de forme (`requete_texte::regles_editeur` rend
//! `None`). Elle s'enregistre donc comme UNE règle de plus, dont la valeur est
//! le texte de la requête :
//!
//! ```json
//! {"field": "requete", "op": "is", "value": "genre:jazz (annee:1959 ou non credit(instrument: piano))"}
//! ```
//!
//! # Pourquoi une règle, et pas une colonne ou un arbre
//!
//! - **Aucune migration** : la colonne `rules` est déjà du texte JSON, sur
//!   SQLite comme sur PostgreSQL. L'enregistrement, la relecture et
//!   l'évaluation passent par le même chemin sur les deux bases.
//! - **Tous les lecteurs de `rules` la voient** : `build_smart_query_rapport`
//!   la traduit, donc les pistes, les albums, l'aperçu et une playlist qui
//!   référence celle-ci (`in_playlist`, `smart_refs`) l'évaluent pareil.
//! - **Échec fermé** : un lecteur qui ne la connaît pas (un serveur plus
//!   ancien) la traite comme une règle intraduisible, qui ne retient RIEN et
//!   se signale (#1231, #4467) — jamais toute la bibliothèque.
//! - **Le texte, pas un arbre** : l'éditeur « Requête avancée » relit ce que
//!   l'utilisateur a écrit, tel quel ; le serveur le relit à chaque
//!   évaluation avec la grammaire de `requete_texte`.
//!
//! Une requête PLATE garde son chemin : elle s'enregistre en règles
//! d'éditeur ordinaires (`enregistrement`).

use serde_json::{Value, json};

use crate::regles_sql;
use crate::requete_texte::{self, Noeud};
use crate::smart_refs::RefCtx;

/// Le champ de la règle qui porte une requête textuelle.
pub const CHAMP: &str = "requete";
/// Son seul opérateur : « la piste satisfait la requête ».
pub const OP: &str = "is";

/// La règle qui enregistre le texte `texte`.
pub fn regle(texte: &str) -> Value {
    json!({"field": CHAMP, "op": OP, "value": texte.trim()})
}

/// Ce qu'il faut enregistrer pour une requête lue : ses règles d'éditeur
/// quand elle est plate, sinon une seule règle `requete` en mode `all`.
pub fn enregistrement(texte: &str, arbre: &Noeud) -> (Vec<Value>, &'static str) {
    requete_texte::regles_editeur(arbre).unwrap_or_else(|| (vec![regle(texte)], "all"))
}

/// Pourquoi une règle `requete` ne s'évalue pas.
#[derive(Debug, Clone, PartialEq)]
pub enum Refus {
    /// L'opérateur n'est pas `is`, ou la valeur n'est pas un texte.
    Forme(String),
    /// Le texte ne se lit pas : position (en caractères) et message.
    Illisible(requete_texte::ErreurDeRequete),
    /// Un critère refusé sous une négation : FAUX nié vaudrait VRAI, la
    /// requête rendrait toute la bibliothèque.
    SousNegation(Vec<String>),
}

impl Refus {
    /// Le corps d'erreur de la route `/requete` (`requete_invalide`), pour
    /// que l'éditeur affiche un refus d'enregistrement comme un refus d'essai.
    pub fn corps(&self) -> Value {
        let (message, position) = match self {
            Refus::Forme(m) => (m.clone(), Value::Null),
            Refus::Illisible(e) => (e.message.clone(), json!(e.position)),
            Refus::SousNegation(r) => (
                format!(
                    "critère refusé sous une négation (il rendrait toute la bibliothèque) : {}",
                    r.join(", ")
                ),
                Value::Null,
            ),
        };
        json!({"error": "requete_invalide", "message": message, "position": position})
    }
}

/// Le texte d'une règle `requete`, ou pourquoi elle est mal formée.
fn texte_de(rule: &Value) -> Result<&str, Refus> {
    let op = regles_sql::normaliser_op(regles_sql::lire_op(rule));
    if op != OP {
        return Err(Refus::Forme(format!(
            "une règle « {CHAMP} » s'écrit avec l'opérateur « {OP} », pas « {op} »"
        )));
    }
    rule.get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| Refus::Forme(format!("une règle « {CHAMP} » porte un texte")))
}

/// La condition SQL d'une règle `requete` (sans `WHERE`, sur les alias du
/// moteur des playlists), et les critères refusés HORS négation, que
/// l'appelant ajoute à son rapport comme ceux d'une règle ordinaire.
pub(crate) fn condition(rule: &Value, ctx: &RefCtx) -> Result<(String, Vec<String>), Refus> {
    let texte = texte_de(rule)?;
    let arbre = requete_texte::analyser(texte).map_err(Refus::Illisible)?;
    let c = requete_texte::compiler(&arbre, ctx);
    if !c.refusees_sous_non.is_empty() {
        return Err(Refus::SousNegation(c.refusees_sous_non));
    }
    Ok((c.condition, c.refusees))
}

/// Les règles d'un corps `rules` tel que les routes le reçoivent : un
/// tableau, ou le même tableau encodé en chaîne.
fn regles_du_corps(rules: &Value) -> Vec<Value> {
    match rules {
        Value::Array(a) => a.clone(),
        Value::String(t) => serde_json::from_str(t).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// À l'enregistrement : chaque règle `requete` doit se lire et s'évaluer.
/// Une requête fautive est refusée AVANT d'être écrite, avec le corps de la
/// route `/requete` ; plus tard, elle ne retiendrait rien.
pub fn valider(rules: &Value, ctx: &RefCtx) -> Result<(), Refus> {
    for rule in regles_du_corps(rules) {
        if rule.get("field").and_then(Value::as_str) == Some(CHAMP) {
            condition(&rule, ctx)?;
        }
    }
    Ok(())
}

/// Les règles d'éditeur que contient une règle `requete` (les feuilles de son
/// arbre), pour qui cherche dans `rules` sans évaluer — la détection de
/// cycle des références (`smart_refs`). Vide si elle ne se lit pas.
pub(crate) fn feuilles(rule: &Value) -> Vec<Value> {
    fn cueillir(n: &Noeud, sortie: &mut Vec<Value>) {
        match n {
            Noeud::Regle(r) => sortie.push(r.clone()),
            Noeud::Et(t) | Noeud::Ou(t) => t.iter().for_each(|x| cueillir(x, sortie)),
            Noeud::Non(x) => cueillir(x, sortie),
        }
    }
    let mut sortie = Vec::new();
    if let Some(arbre) = texte_de(rule)
        .ok()
        .and_then(|t| requete_texte::analyser(t).ok())
    {
        cueillir(&arbre, &mut sortie);
    }
    sortie
}

#[cfg(test)]
#[path = "requete_enregistree_tests.rs"]
mod tests;
