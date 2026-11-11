//! #8325 — chef d'orchestre, ensemble / orchestre et œuvre : trois facettes de
//! la Collection (Oxygen), et les colonnes de la liste des pistes.
//!
//! # Où vivent ces valeurs
//!
//! Aucune n'a de colonne dans `tracks`. Chacune est lue dans les magasins
//! ouverts qui la portent déjà, sans migration :
//!
//! | champ        | `track_credits` (rôle) | `track_metadata` (clé)  | `album_metadata` (clé)  |
//! |--------------|------------------------|-------------------------|-------------------------|
//! | `conductor`  | `conductor`            | `conductor`             | `conductor`             |
//! | `ensemble`   | `performer`, sans instrument, nom d'ensemble ([`MOTS_D_ENSEMBLE`]) | `ensemble`, `orchestra` | `ensemble`, `orchestra` |
//! | `work`       | —                      | `work`                  | `work`                  |
//! | `movement`   | —                      | `movement`              | —                       |
//!
//! - `track_credits` rôle `conductor` : balises (CONDUCTOR, TPE3, MP4
//!   `CONDUCTOR`), MusicBrainz (`conductor`, `chorus master`), Qobuz.
//! - `track_metadata` : écrit au scan par `read_extended_metadata` —
//!   `conductor` (lofty `Conductor`), `ensemble` (ENSEMBLE / ORCHESTRA, Vorbis,
//!   `TXXX:` ID3, atome MP4 libre), et depuis #8325 `work` (WORK, `TXXX:WORK`,
//!   `©wrk`, APE WORKTITLE) et `movement` (MOVEMENTNAME, MVNM, `©mvn`). Les
//!   mêmes clés `work` / `movement` sont posées par l'assistant IA quand le
//!   fichier n'en dit rien.
//! - `album_metadata` : la fiche d'album (Vademecum, assistant IA) ; une
//!   valeur d'album vaut pour toutes ses pistes.
//!
//! L'ensemble crédité par MusicBrainz (« performing orchestra ») ou par
//! Qobuz (« Orchestra », « Ensemble ») est écrit dans `track_credits` avec le
//! rôle `performer` et sans instrument (`credits_mb::role_canonique`) : rien
//! ne l'y distingue par le rôle d'un soliste dont l'instrument n'est pas dit.
//! La facette le reconnaît donc à son NOM ([`MOTS_D_ENSEMBLE`] : orchestre,
//! philharmonie, chœur, quatuor…), sans changer le vocabulaire des rôles.
//!
//! # Une valeur, plusieurs personnes
//!
//! Une balise répétée est rangée en UNE ligne, jointe par « ; »
//! (`ENSEMBLE=Chœur…` + `ORCHESTRA=Orchestre…` → « Chœur…; Orchestre… »).
//! La facette compte chaque entrée à part ([`entrees`]) et le filtre trouve
//! une entrée au milieu de la liste ([`bloc`]) : la même découpe des deux
//! côtés, sans quoi le rail annoncerait des pistes que la liste ne rend pas.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::Engine;
use super::facet_filter::{Placeholders, Predicat};

/// Les champs « classiques », par leur nom de facette et de paramètre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Champ {
    /// Chef d'orchestre — facette et paramètre `conductor`.
    Chef,
    /// Ensemble ou orchestre — facette et paramètre `ensemble`.
    Ensemble,
    /// Œuvre — facette et paramètre `work`.
    Oeuvre,
    /// Mouvement — colonne seulement (`movement`), pas de facette.
    Mouvement,
}

impl Champ {
    /// Les trois champs servis en facette.
    pub const FACETTES: [Champ; 3] = [Champ::Chef, Champ::Ensemble, Champ::Oeuvre];
    /// Les quatre champs servis en colonne de la liste des pistes.
    pub const COLONNES: [Champ; 4] = [
        Champ::Chef,
        Champ::Ensemble,
        Champ::Oeuvre,
        Champ::Mouvement,
    ];

    /// Nom de la facette, du paramètre de requête et de la clé JSON.
    pub fn nom(self) -> &'static str {
        match self {
            Champ::Chef => "conductor",
            Champ::Ensemble => "ensemble",
            Champ::Oeuvre => "work",
            Champ::Mouvement => "movement",
        }
    }

    /// Le champ servi en facette sous ce nom.
    pub fn facette(nom: &str) -> Option<Champ> {
        Champ::FACETTES.into_iter().find(|c| c.nom() == nom)
    }

    fn roles_de_credit(self) -> &'static [&'static str] {
        match self {
            Champ::Chef => &["conductor"],
            _ => &[],
        }
    }

    fn cles_de_piste(self) -> &'static [&'static str] {
        match self {
            Champ::Chef => &["conductor"],
            Champ::Ensemble => &["ensemble", "orchestra"],
            Champ::Oeuvre => &["work"],
            Champ::Mouvement => &["movement"],
        }
    }

    fn cles_d_album(self) -> &'static [&'static str] {
        match self {
            Champ::Chef => &["conductor"],
            Champ::Ensemble => &["ensemble", "orchestra"],
            Champ::Oeuvre => &["work"],
            Champ::Mouvement => &[],
        }
    }
}

/// Les fragments (en minuscules) qui font d'un crédit `performer` sans
/// instrument un ENSEMBLE : orchestres, chœurs, formations de chambre, dans
/// les langues des étiquettes classiques. Un fragment trop court (« chor »,
/// qui prendrait l'organiste Daniel Chorzempa) n'y figure pas.
pub const MOTS_D_ENSEMBLE: [&str; 22] = [
    "orchest",
    "orquest",
    "orkest",
    "philharm",
    "filharm",
    "filarm",
    "symphon",
    "sinfoni",
    "ensemble",
    "choir",
    "chœur",
    "choeur",
    "chorus",
    "coro ",
    "kammerchor",
    "rundfunkchor",
    "consort",
    "quartet",
    "quatuor",
    "quartett",
    "trio ",
    "camerata",
];

/// Une liste SQL de littéraux écrits par le code (`'a', 'b'`).
fn litteraux(cles: &[&str]) -> String {
    cles.iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// L'identifiant de piste d'un magasin, comparable à celui de `tracks`.
///
/// Sur PostgreSQL, `track_credits.track_id` (et les identifiants des autres
/// magasins ouverts) sont du TEXTE sur un miroir copié depuis SQLite, des
/// BIGINT sur une base neuve : les deux côtés sont projetés en texte, ce qui
/// tient dans les deux cas — la règle de `condition_texte_libre`.
fn id(engine: Engine, expr: &str) -> String {
    match engine {
        Engine::Sqlite => expr.to_string(),
        Engine::Postgres => format!("CAST({expr} AS TEXT)"),
    }
}

/// L'identifiant de la piste d'alias `t`, sous la forme que rend [`paires`].
pub fn piste_de_t(engine: Engine) -> String {
    id(engine, "t.id")
}

/// La sous-requête des couples `(piste, v)` d'un champ, tous magasins
/// confondus. `v` est la valeur BRUTE du magasin, élaguée : elle peut porter
/// plusieurs entrées jointes par « ; » (voir [`entrees`]).
///
/// Aucune entrée de la requête n'y figure : rôles et clés sont des littéraux
/// du code.
pub fn paires(engine: Engine, champ: Champ) -> String {
    paires_sur(engine, champ, None)
}

/// [`paires`], restreinte aux pistes de `liste` (littéraux déjà écrits) DANS
/// chaque branche, sur la colonne nue : les index `track_id` / `tracks.id`
/// servent, y compris sur PostgreSQL où la projection en texte de la sortie
/// les rendrait inutilisables (`… s WHERE s.piste IN (…)`).
fn paires_sur(engine: Engine, champ: Champ, liste: Option<&str>) -> String {
    let sur = |col: &str| match liste {
        Some(l) => format!(" AND {col} IN ({l})"),
        None => String::new(),
    };
    let mut morceaux = Vec::new();
    let roles = champ.roles_de_credit();
    if !roles.is_empty() {
        morceaux.push(format!(
            "SELECT {} AS piste, TRIM(tc.artist_name) AS v FROM track_credits tc \
             WHERE LOWER(tc.role) IN ({}) AND tc.artist_name IS NOT NULL{}",
            id(engine, "tc.track_id"),
            litteraux(roles),
            sur("tc.track_id")
        ));
    }
    if champ == Champ::Ensemble {
        // L'orchestre crédité par MusicBrainz ou Qobuz : `performer` sans
        // instrument, reconnu à son nom (voir l'en-tête du module).
        let noms = MOTS_D_ENSEMBLE
            .iter()
            .map(|m| format!("LOWER(tc.artist_name) LIKE '%{m}%'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        morceaux.push(format!(
            "SELECT {} AS piste, TRIM(tc.artist_name) AS v FROM track_credits tc \
             WHERE LOWER(tc.role) = 'performer' \
             AND (tc.instrument IS NULL OR TRIM(tc.instrument) = '') \
             AND tc.artist_name IS NOT NULL AND ({noms}){}",
            id(engine, "tc.track_id"),
            sur("tc.track_id")
        ));
    }
    let cles = champ.cles_de_piste();
    if !cles.is_empty() {
        morceaux.push(format!(
            "SELECT {} AS piste, TRIM(tm.value) AS v FROM track_metadata tm \
             WHERE tm.key IN ({}) AND tm.value IS NOT NULL{}",
            id(engine, "tm.track_id"),
            litteraux(cles),
            sur("tm.track_id")
        ));
    }
    let cles = champ.cles_d_album();
    if !cles.is_empty() {
        morceaux.push(format!(
            "SELECT {} AS piste, TRIM(am.value) AS v FROM album_metadata am \
             JOIN tracks ta ON {} = {} \
             WHERE am.key IN ({}) AND am.value IS NOT NULL{}",
            id(engine, "ta.id"),
            id(engine, "ta.album_id"),
            id(engine, "am.album_id"),
            litteraux(cles),
            sur("ta.id")
        ));
    }
    morceaux.join(" UNION ")
}

/// La valeur d'un magasin sous la forme que compare le filtre :
/// `;a;b;`, les séparateurs « ; » débarrassés de leurs espaces.
fn liste_normalisee(expr: &str) -> String {
    format!("(';' || REPLACE(REPLACE({expr}, ' ;', ';'), '; ', ';') || ';')")
}

/// Les entrées d'une valeur de magasin : découpée à « ; », élaguée, sans
/// entrée vide. Le JUMEAU Rust de [`liste_normalisee`].
pub fn entrees(valeur: &str) -> impl Iterator<Item = &str> {
    valeur.split(';').map(str::trim).filter(|e| !e.is_empty())
}

/// Le prédicat de la facette `champ` pour la piste d'alias `t` : la piste
/// porte, dans l'un de ses magasins, une entrée égale (sans la casse) à l'une
/// des valeurs. Un `IN` sur une sous-requête NON corrélée : elle n'est
/// évaluée qu'une fois, quel que soit le nombre de pistes.
///
/// Le JUMEAU de ce prédicat est appelé par la liste
/// (`TrackRepo::conditions_du_filtre`) et par les effectifs du rail
/// (`facets::build_facet_conditions`).
pub fn bloc(
    engine: Engine,
    champ: Champ,
) -> impl Fn(&mut Placeholders, &[String]) -> Option<Predicat> {
    move |ph, valeurs| {
        if valeurs.is_empty() {
            return None;
        }
        let liste = liste_normalisee("s.v");
        let esc = super::track_repo::like_escape_clause();
        let termes: Vec<String> = valeurs
            .iter()
            .map(|_| format!("LOWER({liste}) LIKE LOWER({}){esc}", ph.take()))
            .collect();
        let liees = valeurs
            .iter()
            .map(|v| {
                SqlValue::Text(format!(
                    "%;{};%",
                    super::track_repo::echapper_jokers_like(v.trim())
                ))
            })
            .collect();
        Some((
            format!(
                "{} IN (SELECT s.piste FROM ({}) s WHERE {})",
                piste_de_t(engine),
                paires(engine, champ),
                super::facet_filter::ou_equilibre(termes)
            ),
            liees,
        ))
    }
}

/// Cumule des couples `(piste, valeur brute)` en effectifs par entrée :
/// une piste compte une fois par entrée, quel que soit le nombre de magasins
/// qui la portent. Les entrées qui ne diffèrent que par la casse sont
/// réunies ; la graphie retenue est la plus petite, pour une sortie stable.
///
/// Ordre : effectif décroissant, puis valeur croissante. `limit` tronque.
pub fn cumuler(rows: &[(i64, String)], limit: Option<i64>) -> Vec<(String, i64)> {
    let mut par_cle: HashMap<String, (String, HashSet<i64>)> = HashMap::new();
    for (piste, brut) in rows {
        for entree in entrees(brut) {
            let e = par_cle
                .entry(entree.to_lowercase())
                .or_insert_with(|| (entree.to_string(), HashSet::new()));
            if entree < e.0.as_str() {
                e.0 = entree.to_string();
            }
            e.1.insert(*piste);
        }
    }
    let mut sortie: Vec<(String, i64)> = par_cle
        .into_values()
        .map(|(v, pistes)| (v, pistes.len() as i64))
        .collect();
    sortie.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(n) = limit {
        sortie.truncate(n.max(0) as usize);
    }
    sortie
}

/// Les effectifs de la facette `champ` sur les pistes que retiennent
/// `conds` (alias `t`, marqueurs déjà numérotés, valeurs dans `params`).
pub fn effectifs(
    backend: &Arc<dyn DbBackend>,
    champ: Champ,
    conds: &[String],
    params: &[SqlValue],
    limit: Option<i64>,
) -> Result<Vec<(String, i64)>, String> {
    let engine = backend.engine();
    let restreint = if conds.is_empty() {
        String::new()
    } else {
        format!(
            " WHERE s.piste IN (SELECT {} FROM tracks t WHERE {})",
            piste_de_t(engine),
            conds.join(" AND ")
        )
    };
    let sql = format!(
        "SELECT s.piste, s.v FROM ({}) s{restreint}",
        paires(engine, champ)
    );
    let bound: Vec<&dyn ToSqlValue> = params.iter().map(|v| v as &dyn ToSqlValue).collect();
    let rows: Vec<(i64, String)> = backend
        .query_many(&sql, &bound)?
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let piste = it.next()?.as_i64()?;
            let v = it.next()?.as_string()?;
            Some((piste, v))
        })
        .collect();
    Ok(cumuler(&rows, limit))
}

/// Les valeurs de `champ` pour chaque piste de `pistes`, jointes par « ; »
/// dans l'ordre alphabétique, sans doublon (casse ignorée). Une piste qui
/// n'en porte aucune est ABSENTE de la table — la colonne reste vide, comme
/// `dynamic_range`.
pub fn valeurs_des_pistes(
    backend: &Arc<dyn DbBackend>,
    champ: Champ,
    pistes: &[i64],
) -> Result<HashMap<i64, String>, String> {
    if pistes.is_empty() {
        return Ok(HashMap::new());
    }
    let engine = backend.engine();
    // Nos propres `i64` : inlinés sans risque, comme les ids d'une collection.
    let liste = pistes
        .iter()
        .map(|p| match engine {
            Engine::Sqlite => p.to_string(),
            Engine::Postgres => format!("'{p}'"),
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT s.piste, s.v FROM ({}) s",
        paires_sur(engine, champ, Some(&liste))
    );
    let mut par_piste: HashMap<i64, BTreeMap<String, String>> = HashMap::new();
    for row in backend.query_many(&sql, &[])? {
        let mut it = row.into_iter();
        let (Some(piste), Some(v)) = (
            it.next().and_then(|v| v.as_i64()),
            it.next().and_then(|v| v.as_string()),
        ) else {
            continue;
        };
        let valeurs = par_piste.entry(piste).or_default();
        for entree in entrees(&v) {
            valeurs
                .entry(entree.to_lowercase())
                .or_insert_with(|| entree.to_string());
        }
    }
    Ok(par_piste
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(piste, v)| (piste, v.into_values().collect::<Vec<_>>().join("; ")))
        .collect())
}

#[cfg(test)]
#[path = "facettes_classiques_8325_tests.rs"]
mod tests;
