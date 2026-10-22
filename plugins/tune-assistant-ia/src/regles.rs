//! Une phrase devient des RÈGLES du moteur de playlists et de collections
//! intelligentes existant (`tune-smart-http`). Aucun nouveau moteur.
//!
//! Le schéma imposé au modèle est TIRÉ du tableau des critères du moteur
//! (`tune_smart_http::criteres::criteres_exposes`, passé par l'hôte) : un
//! critère que le moteur ne connaît pas ne peut pas être proposé. La réponse
//! est ensuite VALIDÉE ici, règle par règle — critère, opérateur, forme de la
//! valeur. Une règle inconnue est refusée et rendue dans `refusees`, avec sa
//! raison ; elle n'entre jamais dans `rules`.
//!
//! # Les critères par référence (étape 2)
//!
//! `tag` (famille `tag_ref`) et `in_collection` (`collection_ref`) désignent
//! un objet de la base par son IDENTIFIANT (`7`, `classic:3`, `smart:5`). Le
//! modèle ne connaît pas ces identifiants : la consigne lui donne la liste
//! des NOMS existants ([`Referentiel`]), il répond par un nom, et la
//! validation le traduit en identifiant. Un nom qui n'existe pas est refusé
//! (`reference_inconnue`), un nom porté par deux objets aussi
//! (`reference_ambigue`). Sans étiquette (ou sans collection) dans la base,
//! le critère n'est pas offert du tout. `in_playlist` (`playlist_ref`) et
//! `credit` restent hors de l'assistant.
//!
//! Rien n'est créé ici : l'écran montre les règles, l'utilisateur les modifie,
//! puis les enregistre par la route EXISTANTE `POST /api/v1/library/smart-playlists`
//! (ou celle des collections intelligentes).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Un critère du moteur, tel que l'hôte le transmet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CritereMoteur {
    pub collection: Option<String>,
    pub piste: Option<String>,
    /// La famille dans les mots du client : `text`, `int`, `timestamp`…
    pub famille: String,
    pub operateurs: Vec<String>,
}

/// Le niveau visé : une playlist (pistes) ou une collection (albums).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cible {
    Playlist,
    Collection,
}

impl Cible {
    pub fn depuis(t: Option<&str>) -> Self {
        match t {
            Some("collection") => Self::Collection,
            _ => Self::Playlist,
        }
    }
    pub fn nom(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Collection => "collection",
        }
    }
}

/// Familles que le modèle ne peut pas remplir seul : elles désignent un objet
/// de la base par son identifiant (collection, playlist, étiquette) ou
/// demandent une saisie qu'aucun éditeur n'offre encore (crédit). Le moteur
/// les connaît ; l'assistant ne les PROPOSE pas — une règle qui les utilise
/// est refusée comme les autres.
const FAMILLES_HORS_ASSISTANT: &[&str] = &["credit", "playlist_ref"];

/// Les familles par référence que l'assistant offre, avec le [`Referentiel`].
const FAMILLES_PAR_REFERENCE: &[&str] = &["tag_ref", "collection_ref"];

/// Un objet de la base qu'une règle peut désigner : la valeur que le moteur
/// lit, et le nom que l'utilisateur (et le modèle) connaît.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub valeur: String,
    pub nom: String,
}

/// Les noms existants des étiquettes et des collections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Referentiel {
    pub etiquettes: Vec<Reference>,
    pub collections: Vec<Reference>,
}

/// Noms offerts au modèle, au plus, par famille.
pub const REFERENCES_MAX: usize = 200;

fn identifiant(v: &tune_core::db::backend::SqlValue) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_string().and_then(|s| s.trim().parse().ok()))
}

impl Referentiel {
    /// Lit les étiquettes (`tags`), les collections classiques (réglage
    /// `collections`, la même source que le résolveur du moteur) et les
    /// collections intelligentes (`smart_collections`).
    pub fn charger(db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Self {
        let lignes = |sql: &str| -> Vec<(i64, String)> {
            db.query_many(sql, &[])
                .unwrap_or_default()
                .iter()
                .filter_map(|l| {
                    let id = l.first().and_then(identifiant)?;
                    let nom = l.get(1).and_then(|v| v.as_string())?;
                    let nom = nom.trim().to_string();
                    (!nom.is_empty()).then_some((id, nom))
                })
                .take(REFERENCES_MAX)
                .collect()
        };
        let etiquettes = lignes("SELECT id, name FROM tags ORDER BY name")
            .into_iter()
            .map(|(id, nom)| Reference {
                valeur: id.to_string(),
                nom,
            })
            .collect();
        let mut collections: Vec<Reference> =
            tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
                .get("collections")
                .ok()
                .flatten()
                .and_then(|brut| serde_json::from_str::<Vec<Value>>(&brut).ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_i64)?;
                    let nom = c.get("name").and_then(Value::as_str)?.trim().to_string();
                    (!nom.is_empty()).then(|| Reference {
                        valeur: format!("classic:{id}"),
                        nom,
                    })
                })
                .take(REFERENCES_MAX)
                .collect();
        collections.extend(
            lignes("SELECT id, name FROM smart_collections ORDER BY name")
                .into_iter()
                .map(|(id, nom)| Reference {
                    valeur: format!("smart:{id}"),
                    nom,
                }),
        );
        collections.truncate(REFERENCES_MAX);
        Self {
            etiquettes,
            collections,
        }
    }

    fn de_famille(&self, famille: &str) -> &[Reference] {
        match famille {
            "tag_ref" => &self.etiquettes,
            "collection_ref" => &self.collections,
            _ => &[],
        }
    }

    /// La valeur moteur d'un NOM, ou la raison du refus.
    fn resoudre(&self, famille: &str, v: &Value) -> Result<Value, &'static str> {
        let nom = v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("valeur_invalide")?
            .to_lowercase();
        let trouves: Vec<&Reference> = self
            .de_famille(famille)
            .iter()
            .filter(|r| r.nom.to_lowercase() == nom)
            .collect();
        match trouves.as_slice() {
            [un] => Ok(json!(un.valeur)),
            [] => Err("reference_inconnue"),
            _ => Err("reference_ambigue"),
        }
    }
}

/// Les tris que le moteur des playlists sait faire (`smart_playlists.rs`).
pub const TRIS_PLAYLIST: &[&str] = &[
    "title",
    "artist",
    "album",
    "year",
    "duration",
    "added_at",
    "play_count",
    "random",
];

/// Un critère offert au modèle, à ce niveau.
#[derive(Clone, Debug)]
pub struct Offert {
    pub nom: String,
    pub famille: String,
    pub operateurs: Vec<String>,
}

pub fn criteres_offerts(
    criteres: &[CritereMoteur],
    cible: Cible,
    refs: &Referentiel,
) -> Vec<Offert> {
    criteres
        .iter()
        .filter(|c| !FAMILLES_HORS_ASSISTANT.contains(&c.famille.as_str()))
        // Un critère par référence n'est offert que si la base a de quoi le
        // remplir : sans étiquette, `tag` n'a aucune valeur possible.
        .filter(|c| {
            !FAMILLES_PAR_REFERENCE.contains(&c.famille.as_str())
                || !refs.de_famille(&c.famille).is_empty()
        })
        .filter_map(|c| {
            let nom = match cible {
                Cible::Playlist => c.piste.clone(),
                Cible::Collection => c.collection.clone(),
            }?;
            Some(Offert {
                nom,
                famille: c.famille.clone(),
                operateurs: c.operateurs.clone(),
            })
        })
        .collect()
}

fn forme_de_valeur(famille: &str, op: &str) -> &'static str {
    match (famille, op) {
        (_, "is_null" | "is_not_null" | "is_true" | "is_false") => "null",
        (_, "in_last") => "integer number of days >= 1",
        ("timestamp", "between") => "[\"YYYY-MM-DD\", \"YYYY-MM-DD\"]",
        (_, "between") => "[min, max] (two integers)",
        ("timestamp", _) => "\"YYYY-MM-DD\" or \"now-Nd\"",
        ("int" | "count", _) => "integer",
        ("favorite", _) => "\"track\" | \"album\" | \"artist\"",
        ("tag_ref", _) => "the exact NAME of one tag listed below",
        ("collection_ref", _) => "the exact NAME of one collection listed below",
        ("text", "in") => "comma-separated string, e.g. \"Jazz, Blues\"",
        _ => "non-empty string",
    }
}

/// La consigne : le schéma de réponse et la liste FERMÉE des critères.
pub fn consigne(
    offerts: &[Offert],
    cible: Cible,
    genres: &[String],
    refs: &Referentiel,
    langue: &str,
) -> String {
    let mut lignes = Vec::new();
    for o in offerts {
        let ops: Vec<String> = o
            .operateurs
            .iter()
            .map(|op| format!("{op} → value: {}", forme_de_valeur(&o.famille, op)))
            .collect();
        lignes.push(format!("- {} ({}): {}", o.nom, o.famille, ops.join("; ")));
    }
    let tri = match cible {
        Cible::Playlist => format!(
            "\"sort_by\": one of {} or null, \"sort_order\": \"asc\" | \"desc\" | null, \"max_tracks\": integer 1-10000 or null,",
            TRIS_PLAYLIST.join(", ")
        ),
        Cible::Collection => String::new(),
    };
    let genres = if genres.is_empty() {
        String::new()
    } else {
        format!(
            "\nGenres present in this library (use these exact spellings when relevant): {}.",
            genres.join(", ")
        )
    };
    let mut noms = String::new();
    for (famille, titre) in [("tag_ref", "Tags"), ("collection_ref", "Collections")] {
        let Some(o) = offerts.iter().find(|o| o.famille == famille) else {
            continue;
        };
        let liste: Vec<String> = refs
            .de_famille(famille)
            .iter()
            .map(|r| format!("{:?}", r.nom))
            .collect();
        noms.push_str(&format!(
            "\n{titre} in this library (the only valid values of \"{}\"): {}.",
            o.nom,
            liste.join(", ")
        ));
    }
    format!(
        "You turn a music request into rules for the smart {cible} engine of a music server. \
Reply with ONE JSON object and nothing else:\n\
{{\"name\": short {cible} name in the user's language ({langue}), \
\"match_mode\": \"all\" | \"any\", \
\"rules\": [{{\"field\": ..., \"op\": ..., \"value\": ...}}], \
{tri} \
\"untranslated\": [parts of the request that no rule below can express]}}\n\
Use ONLY these fields and operators. Any other field or operator is rejected:\n{}\n\
Never invent a field. If part of the request cannot be expressed (mood, tempo feel, \
\"no live recordings\" when no field says so), put it in \"untranslated\" instead of guessing. \
\"match_mode\" applies to all rules at once.{genres}{noms}",
        lignes.join("\n"),
        cible = cible.nom(),
    )
}

/// Une règle refusée, et pourquoi.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Refusee {
    pub regle: Value,
    pub raison: &'static str,
}

/// La proposition validée.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Proposition {
    pub name: String,
    pub match_mode: String,
    pub rules: Vec<Value>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub max_tracks: Option<i64>,
    pub untranslated: Vec<String>,
    pub refusees: Vec<Refusee>,
}

fn entier(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn date_valide(s: &str) -> bool {
    let s = s.trim();
    if let Some(reste) = s.strip_prefix("now-") {
        let n = reste.strip_suffix('d').unwrap_or(reste);
        return !n.is_empty() && n.chars().all(|c| c.is_ascii_digit());
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// La valeur, sous la forme que le moteur lit, ou `None` si elle ne convient
/// pas à cette famille et cet opérateur.
fn valeur_valide(famille: &str, op: &str, v: &Value) -> Option<Value> {
    match (famille, op) {
        (_, "is_null" | "is_not_null" | "is_true" | "is_false") => Some(Value::Null),
        (_, "in_last") => entier(v)
            .filter(|n| *n >= 1 && *n <= 36_500)
            .map(Value::from),
        ("timestamp", "between") => {
            let a = v.as_array().filter(|a| a.len() == 2)?;
            let (x, y) = (a[0].as_str()?, a[1].as_str()?);
            (date_valide(x) && date_valide(y)).then(|| json!([x.trim(), y.trim()]))
        }
        (_, "between") => {
            let a = v.as_array().filter(|a| a.len() == 2)?;
            let (x, y) = (entier(&a[0])?, entier(&a[1])?);
            (x <= y).then(|| json!([x, y]))
        }
        ("timestamp", _) => v
            .as_str()
            .filter(|s| date_valide(s))
            .map(|s| json!(s.trim())),
        ("int" | "count", _) => entier(v).map(Value::from),
        ("favorite", _) => v
            .as_str()
            .filter(|s| matches!(*s, "track" | "album" | "artist"))
            .map(|s| json!(s)),
        _ => v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().count() <= 200)
            .map(|s| json!(s)),
    }
}

/// Valide la réponse du modèle. `Err` si elle n'est pas lisible du tout.
pub fn valider(
    brut: &Value,
    offerts: &[Offert],
    cible: Cible,
    refs: &Referentiel,
) -> Result<Proposition, &'static str> {
    let obj = brut.as_object().ok_or("reponse_invalide")?;
    let regles_brutes = obj
        .get("rules")
        .and_then(Value::as_array)
        .ok_or("reponse_invalide")?;
    let mut rules = Vec::new();
    let mut refusees = Vec::new();
    for r in regles_brutes {
        let champ = r.get("field").and_then(Value::as_str).unwrap_or("");
        let op = r
            .get("op")
            .or_else(|| r.get("operator"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(critere) = offerts.iter().find(|o| o.nom == champ) else {
            refusees.push(Refusee {
                regle: r.clone(),
                raison: "critere_inconnu",
            });
            continue;
        };
        if !critere.operateurs.iter().any(|o| o == op) {
            refusees.push(Refusee {
                regle: r.clone(),
                raison: "operateur_inconnu",
            });
            continue;
        }
        let valeur = r.get("value").cloned().unwrap_or(Value::Null);
        if FAMILLES_PAR_REFERENCE.contains(&critere.famille.as_str()) {
            match refs.resoudre(&critere.famille, &valeur) {
                Ok(v) => rules.push(json!({ "field": champ, "op": op, "value": v })),
                Err(raison) => refusees.push(Refusee {
                    regle: r.clone(),
                    raison,
                }),
            }
            continue;
        }
        match valeur_valide(&critere.famille, op, &valeur) {
            Some(v) => rules.push(json!({ "field": champ, "op": op, "value": v })),
            None => refusees.push(Refusee {
                regle: r.clone(),
                raison: "valeur_invalide",
            }),
        }
    }
    let match_mode = match obj.get("match_mode").and_then(Value::as_str) {
        Some("any") => "any",
        _ => "all",
    };
    let name: String = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(100)
        .collect();
    let (sort_by, sort_order, max_tracks) = match cible {
        Cible::Playlist => (
            obj.get("sort_by")
                .and_then(Value::as_str)
                .filter(|s| TRIS_PLAYLIST.contains(s))
                .map(String::from),
            obj.get("sort_order")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "asc" | "desc"))
                .map(String::from),
            obj.get("max_tracks")
                .and_then(entier)
                .filter(|n| (1..=10_000).contains(n)),
        ),
        Cible::Collection => (None, None, None),
    };
    let untranslated = obj
        .get("untranslated")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().chars().take(200).collect::<String>())
                .filter(|s| !s.is_empty())
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    Ok(Proposition {
        name,
        match_mode: match_mode.into(),
        rules,
        sort_by,
        sort_order,
        max_tracks,
        untranslated,
        refusees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les VRAIS critères du moteur, comme l'hôte les transmet.
    pub(crate) fn criteres_reels() -> Vec<CritereMoteur> {
        tune_smart_http::criteres::criteres_exposes()
            .into_iter()
            .map(|c| CritereMoteur {
                collection: c.collection.map(String::from),
                piste: c.piste.map(String::from),
                famille: c.famille.to_string(),
                operateurs: c.operateurs.iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }

    #[test]
    fn une_regle_inconnue_est_refusee_et_n_entre_pas_dans_les_regles() {
        let offerts = criteres_offerts(&criteres_reels(), Cible::Playlist, &Referentiel::default());
        let brut = json!({
            "name": "Jazz calme 60s",
            "match_mode": "all",
            "rules": [
                {"field": "genre", "op": "contains", "value": "Jazz"},
                {"field": "year", "op": "between", "value": [1960, 1969]},
                // Un critère qui n'existe pas dans le moteur.
                {"field": "mood", "op": "=", "value": "calm"},
                // Un opérateur que la famille n'a pas.
                {"field": "year", "op": "contains", "value": "196"},
                // Une valeur de mauvaise forme.
                {"field": "year", "op": ">=", "value": "les sixties"},
                // Un critère du moteur que l'assistant ne propose pas.
                {"field": "in_collection", "op": "in", "value": "classic:1"}
            ],
            "untranslated": ["calme", "sans les live"]
        });
        let p = valider(&brut, &offerts, Cible::Playlist, &Referentiel::default()).unwrap();
        assert_eq!(
            p.rules,
            vec![
                json!({"field": "genre", "op": "contains", "value": "Jazz"}),
                json!({"field": "year", "op": "between", "value": [1960, 1969]}),
            ]
        );
        let raisons: Vec<(&str, &str)> = p
            .refusees
            .iter()
            .map(|r| (r.regle["field"].as_str().unwrap(), r.raison))
            .collect();
        assert_eq!(
            raisons,
            vec![
                ("mood", "critere_inconnu"),
                ("year", "operateur_inconnu"),
                ("year", "valeur_invalide"),
                ("in_collection", "critere_inconnu"),
            ]
        );
        assert_eq!(p.untranslated, vec!["calme", "sans les live"]);
    }

    #[test]
    fn la_consigne_ne_nomme_que_des_criteres_du_moteur() {
        let reels = criteres_reels();
        let offerts = criteres_offerts(&reels, Cible::Playlist, &Referentiel::default());
        assert!(offerts.iter().any(|o| o.nom == "genre"));
        assert!(offerts.iter().any(|o| o.nom == "artist"));
        // `title` est le titre de la PISTE : il n'existe qu'au niveau playlist.
        assert!(offerts.iter().any(|o| o.nom == "title"));
        assert!(!offerts.iter().any(|o| o.nom == "in_collection"));
        let c = consigne(
            &offerts,
            Cible::Playlist,
            &["Jazz".into()],
            &Referentiel::default(),
            "fr",
        );
        assert!(c.contains("- genre (text)"));
        assert!(!c.contains("in_collection"));
        let collection = criteres_offerts(&reels, Cible::Collection, &Referentiel::default());
        assert!(collection.iter().any(|o| o.nom == "artist_name"));
        assert!(!collection.iter().any(|o| o.nom == "comments"));
    }

    #[test]
    fn le_tri_et_la_limite_sont_bornes() {
        let offerts = criteres_offerts(&criteres_reels(), Cible::Playlist, &Referentiel::default());
        let p = valider(
            &json!({"rules": [], "sort_by": "DROP TABLE", "sort_order": "up", "max_tracks": 999999}),
            &offerts,
            Cible::Playlist,
            &Referentiel::default(),
        )
        .unwrap();
        assert_eq!((p.sort_by, p.sort_order, p.max_tracks), (None, None, None));
        assert!(
            valider(
                &json!({"pas": "de règles"}),
                &offerts,
                Cible::Playlist,
                &Referentiel::default()
            )
            .is_err()
        );
    }

    fn referentiel() -> Referentiel {
        Referentiel {
            etiquettes: vec![
                Reference {
                    valeur: "7".into(),
                    nom: "Coup de cœur".into(),
                },
                Reference {
                    valeur: "8".into(),
                    nom: "Vinyle".into(),
                },
            ],
            collections: vec![
                Reference {
                    valeur: "classic:3".into(),
                    nom: "Dimanche matin".into(),
                },
                Reference {
                    valeur: "smart:5".into(),
                    nom: "Jazz".into(),
                },
                Reference {
                    valeur: "classic:9".into(),
                    nom: "jazz".into(),
                },
            ],
        }
    }

    #[test]
    fn une_etiquette_et_une_collection_se_designent_par_leur_nom() {
        let refs = referentiel();
        let offerts = criteres_offerts(&criteres_reels(), Cible::Playlist, &refs);
        assert!(offerts.iter().any(|o| o.nom == "tag"));
        assert!(offerts.iter().any(|o| o.nom == "in_collection"));
        assert!(!offerts.iter().any(|o| o.nom == "in_playlist"));
        let c = consigne(&offerts, Cible::Playlist, &[], &refs, "fr");
        assert!(c.contains("\"Coup de cœur\""), "{c}");
        assert!(c.contains("\"Dimanche matin\""), "{c}");

        let p = valider(
            &json!({"rules": [
                {"field": "tag", "op": "is", "value": "coup de cœur"},
                {"field": "in_collection", "op": "not_in", "value": "Dimanche matin"},
                // Un nom qui n'existe pas.
                {"field": "tag", "op": "is", "value": "Préférés"},
                // Deux collections portent ce nom.
                {"field": "in_collection", "op": "in", "value": "Jazz"},
                // Un identifiant brut n'est pas un nom.
                {"field": "tag", "op": "is", "value": "7"}
            ]}),
            &offerts,
            Cible::Playlist,
            &refs,
        )
        .unwrap();
        assert_eq!(
            p.rules,
            vec![
                json!({"field": "tag", "op": "is", "value": "7"}),
                json!({"field": "in_collection", "op": "not_in", "value": "classic:3"}),
            ]
        );
        let raisons: Vec<&str> = p.refusees.iter().map(|r| r.raison).collect();
        assert_eq!(
            raisons,
            vec![
                "reference_inconnue",
                "reference_ambigue",
                "reference_inconnue"
            ]
        );
    }

    #[test]
    fn sans_etiquette_dans_la_base_le_critere_n_est_pas_offert() {
        let offerts = criteres_offerts(
            &criteres_reels(),
            Cible::Collection,
            &Referentiel::default(),
        );
        assert!(
            !offerts
                .iter()
                .any(|o| o.nom == "tag" || o.nom == "in_collection")
        );
    }

    #[test]
    fn le_referentiel_se_lit_dans_la_base() {
        let db = crate::suggestions::tests::base();
        db.execute_batch(
            "INSERT INTO tags (id, name) VALUES (4, 'Vinyle');
             INSERT INTO smart_collections (id, name, rules) VALUES (900, 'Années 60', '[]');",
        )
        .unwrap();
        tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .set(
                "collections",
                r#"[{"id": 1, "name": "Dimanche", "album_ids": [1]}]"#,
            )
            .unwrap();
        let r = Referentiel::charger(&db);
        assert_eq!(
            r.etiquettes,
            vec![Reference {
                valeur: "4".into(),
                nom: "Vinyle".into()
            }]
        );
        // Les migrations posent déjà des collections intelligentes : on
        // vérifie la nôtre, et que la classique vient en tête.
        assert_eq!(r.collections[0].valeur, "classic:1");
        assert!(
            r.collections
                .iter()
                .any(|c| c.valeur == "smart:900" && c.nom == "Années 60"),
            "{:?}",
            r.collections
        );
    }
}
