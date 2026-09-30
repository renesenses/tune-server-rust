//! La traduction d'une règle en condition SQL — **une seule fois**, pour les
//! playlists comme pour les collections.
//!
//! ## Pourquoi ce module existe
//!
//! Mesuré sur le .18 le 19/09/2026, après la présentation du 18 (Bertrand :
//! *« Smart playlist — qobuz idem collection »*) :
//!
//! | règle, par la route | pistes rendues |
//! |---|---|
//! | `artist` = John Coltrane | 401 ✅ |
//! | `composer` = Mozart | **47 118** ❌ |
//! | `title` = n'importe quoi | **47 118** ❌ |
//!
//! `build_smart_query` était une liste de paires `(champ, opérateur)` écrites à
//! la main, terminée par `_ => continue`. **Soixante-six** combinaisons que
//! l'éditeur propose n'y figuraient pas — dont le champ `composer` en entier,
//! et `title` avec tout autre opérateur que « contient ». Chacune faisait
//! disparaître la règle **en silence**, et une playlist restrictive rendait
//! alors la bibliothèque entière.
//!
//! Les collections, elles, marchaient : `build_album_query` traduit déjà par
//! table (champ → colonne + type) et par opérateur générique. Ce module est ce
//! mécanisme-là, sorti pour que les deux s'en servent — et pour que le
//! prochain champ ajouté à l'éditeur n'ait plus à être écrit neuf fois.
//!
//! ## La règle d'or
//!
//! 🔴 Une règle qu'on ne sait pas traduire rend **FAUX**, jamais « pas de
//! condition ». Le `continue` d'avant valait « vrai pour tout », c'est-à-dire
//! le contraire de ce que l'utilisateur demandait : il voulait restreindre, il
//! obtenait tout. Faux est le repli sûr — et il se voit.

/// Ce qu'une colonne accepte comme opérateurs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Genre {
    /// Comparaisons insensibles à la casse, motifs `LIKE`.
    Texte,
    /// Comparaisons arithmétiques.
    Nombre,
}

/// Une colonne SQL et son genre.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Colonne {
    pub sql: &'static str,
    pub genre: Genre,
}

const fn texte(sql: &'static str) -> Colonne {
    Colonne {
        sql,
        genre: Genre::Texte,
    }
}
const fn nombre(sql: &'static str) -> Colonne {
    Colonne {
        sql,
        genre: Genre::Nombre,
    }
}

/// Le vocabulaire des PISTES (playlists intelligentes).
///
/// Les alias sont délibérés : l'éditeur web écrit `artist`, d'anciennes
/// playlists et le semis portent `artist_name`. Les deux doivent marcher —
/// c'est le décalage de vocabulaire qui a créé le défaut.
pub(crate) fn colonne_piste(champ: &str) -> Option<Colonne> {
    Some(match champ {
        "title" | "track_title" => texte("t.title"),
        "artist" | "artist_name" => texte("ar.name"),
        "album" | "album_title" => texte("al.title"),
        "album_artist" => texte("t.album_artist"),
        "genre" => texte("t.genre"),
        // 🔴 Le champ que l'éditeur proposait et que le moteur ignorait
        // ENTIÈREMENT : neuf opérateurs, aucun bras.
        "composer" => texte("t.composer"),
        "comments" | "comment" => texte("t.comments"),
        "format" => texte("t.format"),
        "isrc" => texte("t.isrc"),
        "label" => texte("t.label"),
        "folder" | "file_path" => texte("t.file_path"),
        // `COALESCE` : une piste sans provenance écrite est locale (#4299).
        // Une source de SERVICE ne rend rien ici — c'est `source_streaming`
        // qui ajoute les favoris du service au résultat.
        "source" => texte("COALESCE(NULLIF(t.source, ''), 'local')"),
        "year" => nombre("t.year"),
        "sample_rate" => nombre("t.sample_rate"),
        "bit_depth" => nombre("t.bit_depth"),
        "duration_ms" | "duration" => nombre("t.duration_ms"),
        "track_number" => nombre("t.track_number"),
        "disc_number" => nombre("t.disc_number"),
        "bpm" => nombre("t.bpm"),
        "rating" => nombre("t.rating"),
        // #5547 — deux critères d'ALBUM que les collections offraient et que
        // les playlists refusaient (condition FAUSSE). Au niveau de la piste,
        // c'est l'album de la piste qui répond : « albums de 12 pistes » rend
        // les pistes de ces albums, « sans pochette » celles d'un album sans
        // pochette. Mêmes colonnes que `build_album_query`.
        "track_count" => nombre("al.track_count"),
        "cover_path" => texte("al.cover_path"),
        _ => return None,
    })
}

/// La valeur d'une règle, en texte : une chaîne telle quelle, un NOMBRE écrit
/// en chiffres.
///
/// #5547 — l'éditeur des collections envoie un champ numérique comme nombre
/// JSON (`{"value": 1969}`), et celui des playlists le fait désormais aussi.
/// Le moteur des playlists ne lisait que les chaînes : `1969` y devenait `""`,
/// donc `year = 0`. Une liste, un objet ou l'absence restent `""`, comme avant.
pub(crate) fn texte_de(valeur: Option<&serde_json::Value>) -> String {
    match valeur {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Une date de règle en SQL : les formes relatives `now-90d`, `90d`, `90`
/// valent « il y a N jours » ; tout le reste est une date littérale.
///
/// Sortie de `smart_collections` (#5547) sans un caractère changé, pour que
/// les playlists lisent une date comme les collections.
pub(crate) fn horodatage_sql(input: &str) -> String {
    // Relative forms: "now-90d", "90d", "90" — N days ago. The seeded
    // "🆕 Récents" collection stores the bare "90d" form, which used to fall
    // through to a literal string ('90d') that no date ever compares against.
    let rest = input.strip_prefix("now-").unwrap_or(input);
    let digits = rest.strip_suffix('d').unwrap_or(rest);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        let days: i64 = digits.parse().unwrap_or(30);
        return format!("DATETIME('now', '-{days} days')");
    }
    format!("'{}'", input.replace('\'', "''"))
}

/// Les deux bornes d'une règle « entre » portant sur des DATES, en SQL.
///
/// Même lecture que les collections : un tableau de deux chaînes, bornes par
/// défaut larges.
pub(crate) fn bornes_de_dates(valeur: Option<&serde_json::Value>) -> Option<(String, String)> {
    let arr = valeur?.as_array()?;
    let lo = arr.first().and_then(|v| v.as_str()).unwrap_or("2000-01-01");
    let hi = arr.get(1).and_then(|v| v.as_str()).unwrap_or("2099-01-01");
    Some((horodatage_sql(lo), horodatage_sql(hi)))
}

/// Les deux bornes d'une règle « entre » portant sur des NOMBRES.
///
/// Même lecture que `build_album_query` : un tableau `[lo, hi]`, ou la chaîne
/// `"lo,hi"`. `None` si la valeur n'a pas deux bornes.
pub(crate) fn bornes_de_nombres(valeur: Option<&serde_json::Value>) -> Option<(i64, i64)> {
    let nombre = |v: &serde_json::Value| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
    };
    match valeur? {
        serde_json::Value::Array(arr) if arr.len() == 2 => Some((
            nombre(&arr[0]).unwrap_or(0),
            nombre(&arr[1]).unwrap_or(i64::MAX),
        )),
        serde_json::Value::String(s) => {
            let (lo, hi) = s.split_once(',')?;
            Some((
                lo.trim().parse::<i64>().unwrap_or(0),
                hi.trim().parse::<i64>().unwrap_or(i64::MAX),
            ))
        }
        _ => None,
    }
}

/// Les sous-conditions d'une règle « crédit » (`{"role", "artist_name",
/// "instrument"}`), sur l'alias `tc` de `track_credits`.
///
/// Sortie de `build_album_query` (#5547) sans un caractère changé : la piste
/// et l'album lisent un crédit de la même façon. Vide si l'objet ne nomme
/// rien.
pub(crate) fn conditions_credit(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut sub_conds = Vec::new();
    if let Some(role) = obj
        .get("role")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        sub_conds.push(format!(
            "LOWER(tc.role) LIKE LOWER('%{}%')",
            role.replace('\'', "''")
        ));
    }
    if let Some(artist) = obj
        .get("artist_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        sub_conds.push(format!(
            "LOWER(tc.artist_name) LIKE LOWER('%{}%')",
            artist.replace('\'', "''")
        ));
    }
    if let Some(instr) = obj
        .get("instrument")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        // MÊME canonisation qu'à l'écriture des crédits
        // (#2799 §4). L'enrichissement range désormais
        // « grand piano » / « electric piano » sous `piano` ;
        // si la règle cherchait le libellé brut saisi par
        // l'utilisateur, une collection `instrument: Grand
        // Piano` ne trouverait plus rien alors que les lignes
        // existent. Deux normalisations, deux résultats.
        let canon = tune_core::metadata::instruments::canoniser_instrument(instr);
        let motif = if canon.is_empty() { instr } else { &canon };
        sub_conds.push(format!(
            "LOWER(tc.instrument) LIKE LOWER('%{}%')",
            motif.replace('\'', "''")
        ));
    }
    sub_conds
}

/// L'opérateur BRUT d'une règle, quelle que soit la clé qui le porte.
///
/// 🔴 #4467 — **deux clés désignent la même chose et coexistent en base** :
/// l'éditeur de collections écrit `op`, celui des playlists `operator`, et le
/// type canonique du client (`SmartRule`) déclare `op`. Sur le .18, les
/// collections 32, 33, 34, 36, 37 portent `op`, les 5 et 10 `operator`.
///
/// Trois analyseurs sur quatre lisaient déjà les deux (`catalogue`,
/// `smart_collections`, `source_streaming`). Le quatrième —
/// `smart_playlists::build_smart_query` — ne lisait que `op` : une règle
/// écrite `{"operator": "!="}` y retombait sur le défaut `contains` et rendait
/// le **contraire** de ce qu'elle demandait, sans une ligne de journal.
///
/// Mesuré sur le .18 en v0.9.162 le 23/09/2026,
/// `POST /api/v1/library/smart-playlists/preview`, `artist` = « Miles Davis »
/// sur 42 844 pistes :
///
/// | règle | `total` |
/// |---|---|
/// | `{"op":"="}` | 337 |
/// | `{"operator":"="}` | 338 — lu `contains` |
/// | `{"op":"!="}` | 42 507 |
/// | `{"operator":"!="}` | **338** — l'exact contraire |
/// | `{"op":"is_empty"}` | 0 |
/// | `{"operator":"is_empty"}` | **42 844** — toute la bibliothèque |
///
/// Une seule définition, ici, pour que le prochain analyseur n'ait plus à
/// choisir. `op` d'abord : c'est la clé du type canonique du client.
pub(crate) fn lire_op(rule: &serde_json::Value) -> &str {
    // La première clé qui porte une CHAÎNE gagne : une clé présente mais d'un
    // autre type (un nombre recopié par un éditeur) ne doit pas masquer
    // l'autre, sans quoi l'opérateur écrit disparaît une seconde fois.
    rule.get("op")
        .and_then(|v| v.as_str())
        .or_else(|| rule.get("operator").and_then(|v| v.as_str()))
        .unwrap_or("contains")
}

/// Le nom canonique d'un opérateur.
///
/// Reprise de `build_album_query`, qui l'avait déjà : l'éditeur écrit
/// `equals`, le semis `=`, d'anciennes règles `greater_than`. Les trois
/// désignent la même chose, et n'en reconnaître qu'un faisait disparaître la
/// règle.
pub(crate) fn normaliser_op(brut: &str) -> &str {
    match brut {
        "=" | "eq" | "equals" => "=",
        "!=" | "ne" | "neq" | "not_equals" => "!=",
        ">=" | "gte" | "greater_than" | "greater_equal" => ">=",
        ">" | "gt" => ">",
        "<=" | "lte" | "less_than" | "less_equal" => "<=",
        "<" | "lt" => "<",
        "is_empty" | "empty" | "is_null" => "is_null",
        "is_not_empty" | "not_empty" | "is_not_null" => "is_not_null",
        autre => autre,
    }
}

/// La condition SQL d'une règle, ou `None` si elle n'a pas de sens.
///
/// `None` veut dire « je ne sais pas traduire » — l'appelant doit alors poser
/// une condition FAUSSE, jamais rien.
///
/// ⚠️ Deux échappements, et les confondre casse dans les deux sens : `=`
/// compare des chaînes entières, où `%` n'est pas un joker ; `LIKE` en fait
/// un. Même raisonnement que `build_album_query`.
pub(crate) fn condition(col: Colonne, op: &str, valeur: &str) -> Option<String> {
    let sql = col.sql;
    let esc = valeur.replace('\'', "''");
    let esc_like = tune_core::db::track_repo::echapper_jokers_like(valeur).replace('\'', "''");
    let clause = tune_core::db::track_repo::like_escape_clause();
    let texte = col.genre == Genre::Texte;
    let entier = || valeur.trim().parse::<i64>().unwrap_or(0);

    // 🔴 L'insensibilité aux ACCENTS, que l'ancienne traduction tenait pour le
    // genre, l'artiste, l'album et le titre — et qu'il ne faut pas perdre en
    // généralisant : « Beyonce » doit trouver « Beyoncé ». On ajoute la
    // variante sans accents seulement quand la valeur en porte, pour ne pas
    // doubler toutes les conditions pour rien.
    let sans = crate::smart_playlists::strip_accents(&esc);
    let sans_like = crate::smart_playlists::strip_accents(&esc_like);
    let ou_sans = |motif: String, variante: String| {
        if sans_like == esc_like && sans == esc {
            motif
        } else {
            format!("({motif} OR {variante})")
        }
    };

    Some(match op {
        "is_null" => format!("({sql} IS NULL OR {sql} = '')"),
        "is_not_null" => format!("({sql} IS NOT NULL AND {sql} != '')"),
        "contains" if texte => ou_sans(
            format!("LOWER({sql}) LIKE LOWER('%{esc_like}%'){clause}"),
            format!("LOWER({sql}) LIKE LOWER('%{sans_like}%'){clause}"),
        ),
        "starts_with" if texte => ou_sans(
            format!("LOWER({sql}) LIKE LOWER('{esc_like}%'){clause}"),
            format!("LOWER({sql}) LIKE LOWER('{sans_like}%'){clause}"),
        ),
        "ends_with" if texte => ou_sans(
            format!("LOWER({sql}) LIKE LOWER('%{esc_like}'){clause}"),
            format!("LOWER({sql}) LIKE LOWER('%{sans_like}'){clause}"),
        ),
        "=" if texte => ou_sans(
            format!("LOWER({sql}) = LOWER('{esc}')"),
            format!("LOWER({sql}) = LOWER('{sans}')"),
        ),
        "!=" if texte => format!("LOWER({sql}) != LOWER('{esc}')"),
        // Un nombre « contient » : sur sa forme textuelle, ce qui laisse
        // « 202 » trouver une décennie (#1008).
        "contains" => format!("CAST({sql} AS TEXT) LIKE '%{esc_like}%'{clause}"),
        "=" => format!("{sql} = {}", entier()),
        "!=" => format!("{sql} != {}", entier()),
        ">=" => format!("{sql} >= {}", entier()),
        ">" => format!("{sql} > {}", entier()),
        "<=" => format!("{sql} <= {}", entier()),
        "<" => format!("{sql} < {}", entier()),
        _ => return None,
    })
}

/// La condition d'une règle dont la valeur n'est pas qu'un texte : « entre »
/// (deux bornes) et « parmi » (une liste). Les autres opérateurs passent par
/// [`condition`].
///
/// #5547 — l'éditeur des collections propose les deux, celui des playlists
/// désormais aussi, et `condition` ne sait pas les lire : ils y rendaient
/// FAUX.
///
/// « Parmi » compare sans tenir compte de la casse, des deux côtés — « Rock,
/// Jazz » trouve « rock ». (La traduction des collections abaisse la colonne
/// et pas la liste : elle ne trouve qu'une valeur écrite en minuscules. Elle
/// n'est pas touchée ici, pour ne rien changer aux collections enregistrées ;
/// voir tune-server-rust#5547.)
pub(crate) fn condition_valeur(
    col: Colonne,
    op: &str,
    valeur: Option<&serde_json::Value>,
) -> Option<String> {
    let sql = col.sql;
    match op {
        "between" if col.genre == Genre::Nombre => {
            let (lo, hi) = bornes_de_nombres(valeur)?;
            Some(format!("{sql} BETWEEN {lo} AND {hi}"))
        }
        "in" => {
            let items: Vec<String> = match valeur? {
                serde_json::Value::Array(arr) => arr.iter().map(|v| texte_de(Some(v))).collect(),
                serde_json::Value::String(s) => {
                    s.split(',').map(|x| x.trim().to_string()).collect()
                }
                _ => return None,
            };
            let items: Vec<String> = items.into_iter().filter(|x| !x.is_empty()).collect();
            if items.is_empty() {
                return None;
            }
            Some(if col.genre == Genre::Texte {
                let liste: Vec<String> = items
                    .iter()
                    .map(|x| format!("LOWER('{}')", x.replace('\'', "''")))
                    .collect();
                format!("LOWER({sql}) IN ({})", liste.join(","))
            } else {
                let liste: Vec<String> = items
                    .iter()
                    .map(|x| x.parse::<i64>().unwrap_or(0).to_string())
                    .collect();
                format!("{sql} IN ({})", liste.join(","))
            })
        }
        _ => condition(col, op, &texte_de(valeur)),
    }
}

/// La condition à poser quand rien ne se traduit : FAUX.
pub(crate) const FAUX: &str = "1 = 0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_compositeur_est_une_colonne_texte() {
        // 🔴 Le champ que l'éditeur proposait et que le moteur ignorait
        // entièrement : « composer = Mozart » rendait 47 118 pistes sur le .18.
        let c = colonne_piste("composer").expect("composer doit exister");
        assert_eq!(c.sql, "t.composer");
        assert_eq!(c.genre, Genre::Texte);
    }

    #[test]
    fn les_deux_graphies_d_un_champ_mènent_a_la_meme_colonne() {
        // L'éditeur écrit `artist`, d'anciennes règles `artist_name`. C'est ce
        // décalage de vocabulaire qui faisait disparaître la règle.
        assert_eq!(colonne_piste("artist").unwrap().sql, "ar.name");
        assert_eq!(colonne_piste("artist_name").unwrap().sql, "ar.name");
        assert_eq!(colonne_piste("album").unwrap().sql, "al.title");
        assert_eq!(colonne_piste("album_title").unwrap().sql, "al.title");
    }

    #[test]
    fn un_champ_inconnu_ne_rend_pas_de_colonne() {
        assert!(colonne_piste("zzz_inexistant").is_none());
        assert!(colonne_piste("").is_none());
    }

    /// 🔴 #4467 — une seule lecture des deux clés, pour les quatre analyseurs.
    #[test]
    fn l_operateur_se_lit_sous_ses_deux_cles() {
        use serde_json::json;
        assert_eq!(lire_op(&json!({"field": "artist", "op": "!="})), "!=");
        assert_eq!(lire_op(&json!({"field": "artist", "operator": "!="})), "!=");
        // Sans opérateur du tout : le repli historique, `contains`.
        assert_eq!(lire_op(&json!({"field": "artist"})), "contains");
        // `op` l'emporte s'il est là : c'est la clé du type canonique du client.
        assert_eq!(lire_op(&json!({"op": "=", "operator": "!="})), "=");
        // Une clé présente mais non textuelle ne doit pas masquer l'autre.
        assert_eq!(lire_op(&json!({"op": 3, "operator": "!="})), "!=");
    }

    #[test]
    fn les_trois_graphies_d_un_operateur_se_rejoignent() {
        for g in ["=", "eq", "equals"] {
            assert_eq!(normaliser_op(g), "=", "graphie {g}");
        }
        for g in ["!=", "ne", "neq", "not_equals"] {
            assert_eq!(normaliser_op(g), "!=", "graphie {g}");
        }
        for g in [">=", "gte", "greater_than"] {
            assert_eq!(normaliser_op(g), ">=", "graphie {g}");
        }
        for g in ["is_empty", "empty", "is_null"] {
            assert_eq!(normaliser_op(g), "is_null", "graphie {g}");
        }
        // Un opérateur inconnu passe tel quel : c'est `condition` qui refuse.
        assert_eq!(normaliser_op("branch_of"), "branch_of");
    }

    #[test]
    fn le_texte_se_compare_sans_tenir_compte_de_la_casse() {
        let c = colonne_piste("title").unwrap();
        let sql = condition(c, "=", "Kind of Blue").expect("=");
        assert!(
            sql.contains("LOWER(t.title) = LOWER('Kind of Blue')"),
            "{sql}"
        );
        let sql = condition(c, "contains", "blue").expect("contains");
        assert!(sql.contains("LOWER(t.title) LIKE LOWER('%blue%')"), "{sql}");
    }

    #[test]
    fn une_apostrophe_ne_casse_pas_la_requete() {
        let c = colonne_piste("title").unwrap();
        let sql = condition(c, "=", "L'été").expect("=");
        assert!(sql.contains("L''été"), "{sql}");
    }

    #[test]
    fn un_nombre_se_compare_arithmetiquement() {
        let c = colonne_piste("year").unwrap();
        assert!(
            condition(c, ">=", "1969")
                .unwrap()
                .contains("t.year >= 1969")
        );
        assert!(condition(c, "<", "1969").unwrap().contains("t.year < 1969"));
        // « Contient » sur un nombre porte sur sa forme textuelle : « 202 »
        // trouve une décennie (#1008).
        assert!(
            condition(c, "contains", "202")
                .unwrap()
                .contains("CAST(t.year AS TEXT)"),
            "le contains d'un nombre doit passer par le texte"
        );
    }

    #[test]
    fn vide_et_non_vide_valent_pour_les_deux_genres() {
        for champ in ["title", "year"] {
            let c = colonne_piste(champ).unwrap();
            assert!(
                condition(c, "is_null", "").unwrap().contains("IS NULL"),
                "{champ}"
            );
            assert!(
                condition(c, "is_not_null", "")
                    .unwrap()
                    .contains("IS NOT NULL"),
                "{champ}"
            );
        }
    }

    #[test]
    fn un_operateur_sans_sens_refuse_au_lieu_de_deviner() {
        let c = colonne_piste("title").unwrap();
        // « ≥ » sur un titre : l'éditeur le propose, il ne veut rien dire.
        assert!(condition(c, "branch_of", "x").is_none());
        // Et un opérateur de texte sur un nombre ne s'invente pas non plus.
        let n = colonne_piste("year").unwrap();
        assert!(condition(n, "starts_with", "19").is_none());
    }

    #[test]
    fn les_accents_ne_sont_pas_perdus_en_generalisant() {
        // L'ancienne traduction tenait cette variante pour genre, artiste,
        // album et titre. La perdre aurait été une régression silencieuse :
        // « Beyonce » ne trouverait plus « Beyoncé ».
        let c = colonne_piste("artist").unwrap();
        let sql = condition(c, "contains", "Beyoncé").expect("contains");
        assert!(sql.contains("beyoncé") || sql.contains("Beyoncé"), "{sql}");
        assert!(
            sql.contains("Beyonce'") || sql.contains("Beyonce%"),
            "variante sans accents absente : {sql}"
        );
        // Une valeur sans accents ne double pas la condition pour rien.
        let simple = condition(c, "contains", "Coltrane").expect("contains");
        assert!(
            !simple.contains(" OR "),
            "condition doublée sans raison : {simple}"
        );
    }
}
