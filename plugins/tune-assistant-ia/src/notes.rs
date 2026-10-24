//! Les notes de pochette, à la demande (décision de Bertrand du 23/10/2026,
//! étude #6834 § 4).
//!
//! Le modèle ne reçoit QUE ce que la base contient déjà sur l'album : ses
//! champs (dont l'œuvre, le compositeur, le chef… posés par l'utilisateur ou
//! acceptés depuis l'assistant), ses identifiants MusicBrainz, ses pistes avec
//! leur œuvre et leur mouvement, les crédits, la biographie de l'album et de
//! l'artiste avec leur source et leur licence (Wikipédia, CC BY-SA), et les
//! autres enregistrements de la même œuvre présents dans la bibliothèque.
//! Aucun chemin, aucune donnée d'écoute, rien d'autre.
//!
//! # Ce que le validateur exige
//!
//! Chaque paragraphe cite au moins une SOURCE, et toutes ses sources sont des
//! blocs effectivement fournis ([`Contexte::sources`]) : un paragraphe sans
//! source, ou qui cite un bloc absent, est écarté (`ecartes`). Un
//! `versions_presentes` qui n'est pas un album fourni est écarté de même.
//! Sans aucun paragraphe valable, la réponse est refusée (`422`).
//!
//! Le texte n'écrit RIEN dans la fiche de l'album : il est rangé dans le
//! cache du greffon (une entrée par album), marqué IA, daté, avec le modèle
//! qui l'a écrit, et effaçable (`DELETE /albums/{id}/notes`). L'écran
//! l'affiche sous un bandeau « Écrit par l'IA à partir de vos métadonnées »,
//! avec l'attribution des sources sous licence.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::{DbBackend, ToSqlValue};

use crate::suggestions::{self, EtatAlbum, marque};

/// L'usage compté.
pub const USAGE_NOTES: &str = "notes";
/// Paragraphes rendus, au plus.
pub const PARAGRAPHES_MAX: usize = 6;
/// Caractères d'un paragraphe, au plus.
pub const TEXTE_MAX: usize = 1500;
/// Caractères d'une biographie envoyée, au plus.
pub const BIO_MAX: usize = 3000;
/// Autres enregistrements de la même œuvre, au plus.
pub const VERSIONS_MAX: usize = 10;
/// Crédits distincts envoyés, au plus.
pub const CREDITS_MAX: usize = 60;
/// Jetons de sortie d'un appel.
pub const MAX_JETONS: u32 = 4096;

/// Ce que le modèle lit : des blocs nommés, et ce qu'il a le droit de citer.
#[derive(Clone, Debug, Default)]
pub struct Contexte {
    pub blocs: Map<String, Value>,
    /// L'attribution des blocs sous licence (biographies).
    pub attributions: Vec<Value>,
    /// Les albums que `versions_presentes` peut désigner.
    pub albums_offerts: BTreeSet<i64>,
}

impl Contexte {
    /// Les sources qu'un paragraphe peut citer : les noms des blocs.
    pub fn sources(&self) -> BTreeSet<&str> {
        self.blocs.keys().map(String::as_str).collect()
    }

    /// Le message envoyé : les blocs, en JSON compact.
    pub fn message(&self) -> String {
        Value::Object(self.blocs.clone()).to_string()
    }
}

fn couper(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn texte(v: Option<&tune_core::db::backend::SqlValue>) -> Option<String> {
    v.and_then(|x| x.as_string())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Une biographie et sa provenance, lue dans `albums` ou `artists`.
fn bio(db: &Arc<dyn DbBackend>, table: &str, id: i64) -> Result<Option<(Value, Value)>, String> {
    let ligne = db.query_one(
        &format!(
            "SELECT bio, bio_source, bio_source_url, bio_license FROM {table} WHERE id = {}",
            marque(db, 1)
        ),
        &[&id as &dyn ToSqlValue],
    )?;
    let Some(l) = ligne else { return Ok(None) };
    let Some(b) = texte(l.first()) else {
        return Ok(None);
    };
    let source = texte(l.get(1));
    let url = texte(l.get(2));
    let licence = texte(l.get(3));
    Ok(Some((
        json!({ "text": couper(&b, BIO_MAX), "origin": source }),
        json!({ "source": source, "url": url, "licence": licence }),
    )))
}

/// Rassemble le contexte d'un album. `None` : l'album n'existe pas.
pub fn charger(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Option<Contexte>, String> {
    let Some(etat) = EtatAlbum::charger(db, album_id)? else {
        return Ok(None);
    };
    let mut ctx = Contexte::default();
    let a = &etat.album;
    // Les champs : un bloc par champ rempli.
    for c in suggestions::champs() {
        if let Some(v) = etat.valeur(c) {
            ctx.blocs.insert(format!("champ:{c}"), json!(v));
        }
    }
    for (nom, v) in [
        ("year", a.year.map(|y| json!(y))),
        ("original_year", a.original_year.map(|y| json!(y))),
        (
            "catalog_number",
            a.catalog_number.as_deref().map(|s| json!(s)),
        ),
        ("release_type", a.release_type.as_deref().map(|s| json!(s))),
    ] {
        if let Some(v) = v.filter(|v| !v.is_null() && v != "") {
            ctx.blocs.insert(format!("champ:{nom}"), v);
        }
    }
    if a.musicbrainz_release_id.is_some() || a.musicbrainz_release_group_id.is_some() {
        ctx.blocs.insert(
            "musicbrainz".into(),
            json!({
                "release": a.musicbrainz_release_id,
                "release_group": a.musicbrainz_release_group_id,
            }),
        );
    }
    // Les pistes, avec leur œuvre et leur mouvement.
    let lignes = db.query_many(
        &format!(
            "SELECT t.disc_number, t.track_number, t.title, \
             (SELECT value FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'work'), \
             (SELECT value FROM track_metadata m WHERE m.track_id = t.id AND m.key = 'movement') \
             FROM tracks t WHERE t.album_id = {} ORDER BY t.disc_number, t.track_number, t.id LIMIT 80",
            marque(db, 1)
        ),
        &[&album_id as &dyn ToSqlValue],
    )?;
    let pistes: Vec<Value> = lignes
        .iter()
        .map(|l| {
            json!({
                "disc": l.first().and_then(|v| v.as_i64()),
                "n": l.get(1).and_then(|v| v.as_i64()),
                "title": texte(l.get(2)),
                "work": texte(l.get(3)),
                "movement": texte(l.get(4)),
            })
        })
        .collect();
    if !pistes.is_empty() {
        ctx.blocs.insert("pistes".into(), json!(pistes));
    }
    // Les crédits distincts.
    let credits: Vec<Value> = db
        .query_many(
            &format!(
                "SELECT DISTINCT c.artist_name, c.role, c.instrument FROM track_credits c \
                 JOIN tracks t ON t.id = c.track_id WHERE t.album_id = {} \
                 ORDER BY c.role, c.artist_name LIMIT {CREDITS_MAX}",
                marque(db, 1)
            ),
            &[&album_id as &dyn ToSqlValue],
        )?
        .iter()
        .filter_map(|l| {
            Some(json!({
                "name": texte(l.first())?,
                "role": texte(l.get(1)),
                "instrument": texte(l.get(2)),
            }))
        })
        .collect();
    if !credits.is_empty() {
        ctx.blocs.insert("credits".into(), json!(credits));
    }
    // Les biographies et leur attribution.
    if let Some((b, attr)) = bio(db, "albums", album_id)? {
        ctx.blocs.insert("bio:album".into(), b);
        ctx.attributions
            .push(json!({ "bloc": "bio:album", "attribution": attr }));
    }
    if let Some(aid) = a.artist_id
        && let Some((b, attr)) = bio(db, "artists", aid)?
    {
        ctx.blocs.insert("bio:artiste".into(), b);
        ctx.attributions
            .push(json!({ "bloc": "bio:artiste", "attribution": attr }));
    }
    // Les autres enregistrements de la même œuvre dans la bibliothèque.
    let versions: Vec<Value> = db
        .query_many(
            &format!(
                "SELECT DISTINCT a.id, a.title, ar.name, a.year FROM track_metadata m1 \
                 JOIN tracks t1 ON t1.id = m1.track_id \
                 JOIN track_metadata m2 ON m2.key = 'work' AND m2.value = m1.value \
                 JOIN tracks t2 ON t2.id = m2.track_id \
                 JOIN albums a ON a.id = t2.album_id \
                 LEFT JOIN artists ar ON ar.id = a.artist_id \
                 WHERE m1.key = 'work' AND t1.album_id = {} AND t2.album_id <> {} \
                 ORDER BY a.id LIMIT {VERSIONS_MAX}",
                marque(db, 1),
                marque(db, 2)
            ),
            &[&album_id as &dyn ToSqlValue, &album_id as &dyn ToSqlValue],
        )?
        .iter()
        .filter_map(|l| {
            let id = l.first().and_then(|v| v.as_i64())?;
            ctx.albums_offerts.insert(id);
            Some(json!({
                "album_id": id,
                "title": texte(l.get(1)),
                "artist": texte(l.get(2)),
                "year": l.get(3).and_then(|v| v.as_i64()),
            }))
        })
        .collect();
    if !versions.is_empty() {
        ctx.blocs.insert("versions".into(), json!(versions));
    }
    Ok(Some(ctx))
}

/// La consigne.
pub fn consigne(langue: &str, sources: &BTreeSet<&str>) -> String {
    let liste: Vec<&str> = sources.iter().copied().collect();
    format!(
        "You write short liner notes for ONE album of a music library, in {langue}. \
You receive named blocks of metadata already stored in the library. Use ONLY these blocks: \
never add a fact, date, name or anecdote that is not written in them, even if you know it. \
Reply with ONE JSON object and nothing else:\n\
{{\"paragraphes\": [{{\"texte\": string, \"sources\": [block names]}}], \
\"oeuvre\": {{\"titre\": string, \"mouvements\": [string]}} or null, \
\"versions_presentes\": [album_id taken from the \"versions\" block]}}\n\
At most {PARAGRAPHES_MAX} paragraphs. Every paragraph lists the blocks it relies on; the only valid \
block names are: {}. When the blocks say little, write little: one short paragraph is a valid answer.",
        liste.join(", ")
    )
}

/// Un paragraphe retenu.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Paragraphe {
    pub texte: String,
    pub sources: Vec<String>,
}

/// Les notes validées.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Notes {
    pub paragraphes: Vec<Paragraphe>,
    pub oeuvre: Option<Value>,
    pub versions_presentes: Vec<i64>,
    /// Ce que le validateur a écarté, et pourquoi.
    pub ecartes: Vec<Value>,
}

/// Valide la réponse du modèle contre le contexte envoyé.
pub fn valider(brut: &Value, ctx: &Contexte) -> Result<Notes, &'static str> {
    let obj = brut.as_object().ok_or("reponse_invalide")?;
    let bruts = obj
        .get("paragraphes")
        .and_then(Value::as_array)
        .ok_or("reponse_invalide")?;
    let permises = ctx.sources();
    let mut paragraphes = Vec::new();
    let mut ecartes = Vec::new();
    for p in bruts {
        let t = p
            .get("texte")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let sources: Vec<String> = p
            .get("sources")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let raison = if t.is_empty() {
            Some("texte_vide")
        } else if t.chars().count() > TEXTE_MAX {
            Some("texte_trop_long")
        } else if sources.is_empty() {
            Some("sans_source")
        } else if sources.iter().any(|s| !permises.contains(s.as_str())) {
            Some("source_inconnue")
        } else if paragraphes.len() >= PARAGRAPHES_MAX {
            Some("trop_de_paragraphes")
        } else {
            None
        };
        match raison {
            Some(r) => ecartes.push(json!({ "paragraphe": p, "raison": r })),
            None => paragraphes.push(Paragraphe {
                texte: t.to_string(),
                sources,
            }),
        }
    }
    if paragraphes.is_empty() {
        return Err("aucun_paragraphe");
    }
    let oeuvre = match obj.get("oeuvre") {
        Some(o) if o.is_object() => {
            let titre = o.get("titre").and_then(Value::as_str).map(str::trim);
            match titre.filter(|t| !t.is_empty()) {
                Some(titre) => Some(json!({
                    "titre": couper(titre, 300),
                    "mouvements": o.get("mouvements").and_then(Value::as_array)
                        .map(|m| m.iter().filter_map(Value::as_str).take(40)
                            .map(|s| couper(s.trim(), 300)).collect::<Vec<_>>())
                        .unwrap_or_default(),
                })),
                None => None,
            }
        }
        _ => None,
    };
    let mut versions_presentes = Vec::new();
    for v in obj
        .get("versions_presentes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match v.as_i64() {
            Some(id) if ctx.albums_offerts.contains(&id) => {
                if !versions_presentes.contains(&id) {
                    versions_presentes.push(id);
                }
            }
            _ => ecartes.push(json!({ "version": v, "raison": "album_non_fourni" })),
        }
    }
    Ok(Notes {
        paragraphes,
        oeuvre,
        versions_presentes,
        ecartes,
    })
}

/// La clé, dans le cache du greffon, des dernières notes rangées d'un album.
pub fn cle_rangement(album_id: i64) -> String {
    crate::cache::cle(&["notes-album", &album_id.to_string()])
}

/// L'album existe-t-il encore ? (Une note rangée d'un album supprimé n'est
/// plus servie.)
pub fn album_existe(db: &Arc<dyn DbBackend>, album_id: i64) -> bool {
    AlbumRepo::with_backend(db.clone())
        .get(album_id)
        .ok()
        .flatten()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Contexte {
        let mut c = Contexte::default();
        c.blocs.insert("champ:work".into(), json!("Symphonie n° 3"));
        c.blocs.insert("bio:artiste".into(), json!({"text": "…"}));
        c.albums_offerts.insert(7);
        c
    }

    #[test]
    fn un_paragraphe_sans_source_ou_a_source_inconnue_est_ecarte() {
        let brut = json!({"paragraphes": [
            {"texte": "Bon.", "sources": ["champ:work"]},
            {"texte": "Sans source.", "sources": []},
            {"texte": "Inventé.", "sources": ["wikipedia"]},
        ], "versions_presentes": [7, 99]});
        let n = valider(&brut, &ctx()).unwrap();
        assert_eq!(n.paragraphes.len(), 1);
        assert_eq!(n.paragraphes[0].texte, "Bon.");
        assert_eq!(n.versions_presentes, vec![7], "album 99 non fourni");
        assert_eq!(n.ecartes.len(), 3);
    }

    #[test]
    fn sans_paragraphe_valable_la_reponse_est_refusee() {
        let brut = json!({"paragraphes": [{"texte": "x", "sources": ["inconnue"]}]});
        assert_eq!(valider(&brut, &ctx()), Err("aucun_paragraphe"));
        assert_eq!(valider(&json!({"x": 1}), &ctx()), Err("reponse_invalide"));
    }
}
