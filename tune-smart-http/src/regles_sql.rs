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

/// #6124 — la VERSION d'une piste (balise `VERSION` : original, réédition,
/// remaster…), rangée au scan dans `track_metadata` et non dans `tracks`.
/// Sous-requête corrélée sur la clé primaire `(track_id, key)` ; alias `t`
/// imposé. Partagée par les playlists et les collections.
pub(crate) const SQL_VERSION_DE_LA_PISTE: &str =
    "(SELECT tmv.value FROM track_metadata tmv WHERE tmv.track_id = t.id AND tmv.key = 'version')";

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
        // #6121 (forum 2218) — la balise ALBUMARTIST de la piste, et à
        // défaut l'artiste de son ALBUM : une piste sans la balise appartient
        // quand même à l'album de quelqu'un. `ar` est ici l'interprète de la
        // piste (`t.artist_id`), d'où la sous-requête sur `al.artist_id`.
        "album_artist" => texte(
            "COALESCE(NULLIF(t.album_artist, ''), \
             (SELECT aal.name FROM artists aal WHERE aal.id = al.artist_id))",
        ),
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
        // #6123 — la note de la piste lue dans ses balises (0 à 5, sans
        // profil). `rating`, lui, reste la note de l'ALBUM du profil actif
        // (`condition_note`).
        "track_rating" => nombre("t.rating"),
        // #6124 — l'année de sortie ORIGINALE, celle de l'album de la piste
        // (`albums.original_year`, mêmes colonnes que `build_album_query`) ;
        // la VERSION, balise de la piste rangée dans `track_metadata`.
        "original_year" => nombre("al.original_year"),
        "version" => texte(SQL_VERSION_DE_LA_PISTE),
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

/// #6143 — l'instant que désigne une date de règle, calculé en RUST.
///
/// Les formes relatives `now-90d`, `90d`, `90` valent « il y a N jours » à
/// l'horloge `maintenant` (la collection « 🆕 Récents » enregistre la forme
/// nue `90d`). Sinon, une date ISO : `AAAA-MM-JJ`, avec ou sans heure, lue en
/// UTC comme le faisait `DATETIME(…, 'unixepoch')`. `None` si illisible.
///
/// Ces formes passaient par `DATETIME('now', '-N days')` et
/// `DATETIME(t.file_mtime, 'unixepoch')`, deux écritures propres à SQLite :
/// sur PostgreSQL, toute règle « après », « avant » ou « entre » faisait
/// tomber l'aperçu. Comme `in_last` (#6141), la borne est désormais un
/// LITTÉRAL que les deux moteurs comparent de la même façon.
/// Le N d'une date relative `now-Nd`, `Nd` ou `N` ; `None` pour toute autre
/// écriture. Un N hors d'échelle reste borné (30 jours s'il ne se lit pas).
fn jours_relatifs(input: &str) -> Option<i64> {
    let rest = input.strip_prefix("now-").unwrap_or(input);
    let digits = rest.strip_suffix('d').unwrap_or(rest);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<i64>().unwrap_or(30).min(JOURS_MAX))
}

pub(crate) fn instant_de(
    input: &str,
    maintenant: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let brut = input.trim();
    if let Some(jours) = jours_relatifs(brut) {
        return Some(maintenant - chrono::Duration::days(jours));
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(brut) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%SZ",
    ] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(brut, format) {
            return Some(d.and_utc());
        }
    }
    chrono::NaiveDate::parse_from_str(brut, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

/// Une date de règle en secondes depuis l'époque : la forme de
/// `tracks.file_mtime` (REAL sur SQLite, DOUBLE PRECISION sur PostgreSQL).
/// `None` si la date est illisible — l'appelant pose alors une condition
/// FAUSSE.
pub(crate) fn horodatage_epoch(input: &str) -> Option<i64> {
    instant_de(input, chrono::Utc::now()).map(|d| d.timestamp())
}

/// Les deux bornes d'une règle « entre » sur `file_mtime`, en secondes. Même
/// lecture que [`bornes_de_dates`] ; `None` si la valeur n'est pas un tableau
/// ou si une borne est illisible.
pub(crate) fn bornes_epoch(valeur: Option<&serde_json::Value>) -> Option<(i64, i64)> {
    let arr = valeur?.as_array()?;
    let lo = arr.first().and_then(|v| v.as_str()).unwrap_or("2000-01-01");
    let hi = arr.get(1).and_then(|v| v.as_str()).unwrap_or("2099-01-01");
    Some((horodatage_epoch(lo)?, horodatage_epoch(hi)?))
}

/// Une date de règle en SQL TEXTE, pour `listen_history.listened_at` (ISO
/// 8601 sur les deux moteurs) : une forme relative devient le littéral ISO
/// d'« il y a N jours » (#6143, plus de `DATETIME('now', …)`) ; tout le reste
/// est une date littérale, comparée telle quelle.
///
/// Sortie de `smart_collections` (#5547), pour que les playlists lisent une
/// date comme les collections.
pub(crate) fn horodatage_sql(input: &str) -> String {
    if let Some(jours) = jours_relatifs(input) {
        let d = chrono::Utc::now() - chrono::Duration::days(jours);
        return format!("'{}'", d.format("%Y-%m-%dT%H:%M:%SZ"));
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

/// #6122 (forum 2217) — l'opérateur « dans les N derniers jours » des dates :
/// une durée GLISSANTE, là où « après » attend une date fixe.
///
/// La règle enregistre N, jamais une date : l'instant « il y a N jours » se lit
/// à l'horloge à CHAQUE traduction, donc à chaque requête. Une collection
/// « ajoutés dans les 30 derniers jours » avance seule avec le calendrier.
pub(crate) const DANS_LES_DERNIERS_JOURS: &str = "in_last";

/// Au-delà, la durée n'a plus de sens (et la date ne se calcule plus).
const JOURS_MAX: i64 = 36_500;

/// Le N d'une règle « dans les N derniers jours » : un entier STRICTEMENT
/// positif, en nombre JSON ou en chiffres. `None` sinon — l'appelant pose
/// alors une condition FAUSSE, jamais « pas de condition ».
pub(crate) fn jours_de(valeur: Option<&serde_json::Value>) -> Option<i64> {
    let n = match valeur? {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    (1..=JOURS_MAX).contains(&n).then_some(n)
}

/// L'instant « il y a N jours », sous les deux formes que la base compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seuil {
    /// En secondes depuis l'époque : la forme de `tracks.file_mtime` (REAL
    /// sur SQLite, DOUBLE PRECISION sur PostgreSQL).
    pub epoch: i64,
    /// En texte ISO 8601 UTC, entre apostrophes : la forme de
    /// `listen_history.listened_at` (TEXT sur les deux moteurs).
    pub iso: String,
}

/// Le seuil de N jours avant `maintenant`.
///
/// Des LITTÉRAUX, pas `DATETIME('now', …)` : cette écriture est propre à
/// SQLite, et PostgreSQL la refuse. Un nombre et une chaîne ISO se comparent
/// de la même façon sur les deux moteurs.
pub(crate) fn seuil_jours(jours: i64, maintenant: chrono::DateTime<chrono::Utc>) -> Seuil {
    let instant = maintenant - chrono::Duration::days(jours);
    Seuil {
        epoch: instant.timestamp(),
        iso: format!("'{}'", instant.format("%Y-%m-%dT%H:%M:%SZ")),
    }
}

/// Le seuil d'une règle « dans les N derniers jours », lu à l'horloge MAINTENANT.
pub(crate) fn seuil_de(valeur: Option<&serde_json::Value>) -> Option<Seuil> {
    jours_de(valeur).map(|n| seuil_jours(n, chrono::Utc::now()))
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
        // #6250 — « ne contient pas ». `not_contains` est la graphie du vieux
        // moteur de tune-core (`Operator::NotContains`).
        "not_contains" | "does_not_contain" | "not_like" => NE_CONTIENT_PAS,
        autre => autre,
    }
}

/// #6250 — l'opérateur « ne contient pas », dans sa graphie canonique.
pub(crate) const NE_CONTIENT_PAS: &str = "not_contains";

/// #6250 — « ne contient pas » : le complément EXACT de « contient ».
///
/// 🔴 Une valeur ABSENTE ne contient rien, donc elle « ne contient pas » :
/// une piste sans genre passe « genre ne contient pas Rock ». Sans le
/// `IS NULL`, `NOT (NULL LIKE …)` vaut NULL en SQL et la piste disparaissait
/// des DEUX listes — ni « contient », ni « ne contient pas ». La chaîne vide
/// n'a pas besoin de cas : `'' LIKE '%x%'` est faux.
pub(crate) fn ne_contient_pas(sql: &str, contient: &str) -> String {
    format!("({sql} IS NULL OR NOT ({contient}))")
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
        // #6250 — « ne contient pas » : le complément de « contient », sur le
        // texte comme sur la forme textuelle d'un nombre. Les deux moteurs le
        // perdaient : FAUX ici, règle ignorée aux collections.
        NE_CONTIENT_PAS => ne_contient_pas(sql, &condition(col, "contains", valeur)?),
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

/// La NOTE d'un album pour le profil actif — « Note » (`rating`), #5547.
///
/// Les deux moteurs compilaient `t.rating`, colonne qui n'existe pas : la
/// requête échouait (`no such column: t.rating`) et l'aperçu rendait une
/// erreur 500, aux collections comme aux playlists (.18, 0.9.169). Les notes
/// vivent dans `album_ratings`, une ligne par album ET par profil.
///
/// Décision de Bertrand (30/09/2026) : la note de l'ALBUM pour le profil
/// actif ; dans une playlist, celle de l'album de chaque piste. `album` est
/// donc `al.id` (collections) ou `t.album_id` (playlists).
///
/// Opérateurs de la famille numérique. Un album SANS note donne `NULL` : il ne
/// passe aucune comparaison — ni « ≥ N », ni « ≠ N », ni « entre ». La famille
/// n'a pas d'opérateur « n'est pas noté », et aucun n'est ajouté.
///
/// `None` : opérateur hors de la famille. Sans profil, rien ne passe (`FAUX`),
/// comme pour les favoris : sans profil, la note est inconnue.
pub(crate) fn condition_note(
    album: &str,
    op: &str,
    valeur: Option<&serde_json::Value>,
    profile_id: Option<i64>,
) -> Option<String> {
    let entier = || texte_de(valeur).trim().parse::<i64>().unwrap_or(0);
    let comparaison = match op {
        "=" | "!=" | ">=" | ">" | "<=" | "<" => format!("{op} {}", entier()),
        "between" => {
            let (lo, hi) = bornes_de_nombres(valeur)?;
            format!("BETWEEN {lo} AND {hi}")
        }
        _ => return None,
    };
    let Some(pid) = profile_id else {
        return Some(FAUX.to_string());
    };
    Some(format!(
        "(SELECT arn.rating FROM album_ratings arn \
         WHERE arn.album_id = {album} AND arn.profile_id = {pid}) {comparaison}"
    ))
}

/// La condition à poser quand rien ne se traduit : FAUX.
pub(crate) const FAUX: &str = "1 = 0";

/// #5530 — « Généré par IA » sur une ligne de la BIBLIOTHÈQUE.
///
/// Le marquage est celui que Qobuz pose sur un album de son catalogue ; un
/// fichier local n'en porte aucun. « non » retient donc toute la bibliothèque,
/// « oui » n'en retient rien. Ce ne sont pas des règles REFUSÉES : la réponse
/// est connue, elle est simplement constante — d'où des littéraux distincts de
/// [`FAUX`], que les rapports d'aperçu lisent comme « intraduisible ».
pub(crate) fn condition_marquage_ia_bibliotheque(op: &str) -> Option<&'static str> {
    match op {
        "is_false" => Some("1 = 1"),
        "is_true" => Some("0 = 1"),
        _ => None,
    }
}

/// #5530 — la même règle sur une colonne `ai_generated` d'un favori de
/// service (TEXT : `'1'` marqué, `'0'` non, NULL inconnu). Un marquage
/// inconnu n'est PAS un marquage : « non » retient la ligne.
pub(crate) fn condition_marquage_ia_colonne(col: &str, op: &str) -> Option<String> {
    match op {
        "is_false" => Some(format!("({col} IS NULL OR {col} != '1')")),
        "is_true" => Some(format!("{col} = '1'")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #6122 — N se lit en nombre ou en chiffres ; zéro, négatif, illisible
    /// ou démesuré : rien (la règle rendra FAUX).
    #[test]
    fn le_nombre_de_jours_se_lit_ou_se_refuse() {
        use serde_json::json;
        assert_eq!(jours_de(Some(&json!(30))), Some(30));
        assert_eq!(jours_de(Some(&json!("30"))), Some(30));
        assert_eq!(jours_de(Some(&json!(" 7 "))), Some(7));
        for v in [
            json!(0),
            json!(-5),
            json!("abc"),
            json!(""),
            json!(2.5),
            json!(["30"]),
            json!(36_501),
            serde_json::Value::Null,
        ] {
            assert_eq!(jours_de(Some(&v)), None, "{v}");
        }
        assert_eq!(jours_de(None), None);
    }

    /// #6122 — le seuil suit l'horloge : la même règle (N = 30) lue un jour
    /// plus tard recule son seuil d'un jour. Rien n'est figé à l'enregistrement.
    #[test]
    fn le_seuil_glisse_avec_l_horloge() {
        use chrono::TimeZone;
        let t0 = chrono::Utc
            .with_ymd_and_hms(2026, 10, 11, 12, 0, 0)
            .unwrap();
        let s = seuil_jours(30, t0);
        assert_eq!(s.iso, "'2026-09-11T12:00:00Z'");
        assert_eq!(s.epoch, t0.timestamp() - 30 * 86_400);
        let lendemain = seuil_jours(30, t0 + chrono::Duration::days(1));
        assert_eq!(lendemain.iso, "'2026-09-12T12:00:00Z'");
        assert_eq!(lendemain.epoch - s.epoch, 86_400);
        // Et `seuil_de` lit bien l'horloge courante.
        let maintenant = chrono::Utc::now().timestamp();
        let vivant = seuil_de(Some(&serde_json::json!(1))).unwrap();
        assert!(
            (maintenant - 86_400 - vivant.epoch).abs() <= 5,
            "{vivant:?}"
        );
    }

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
