//! Prédicats SQL des facettes à **plusieurs valeurs** (issue #2168).
//!
//! # Sémantique
//!
//! * plusieurs valeurs **dans une même facette** se combinent en **OU**
//!   (`format = aiff OU flac`) ;
//! * deux **facettes différentes** se combinent en **ET**
//!   (`format = flac ET genre = jazz`).
//!
//! C'est la convention de tous les navigateurs à facettes (Audirvana, Helium,
//! les moteurs de recherche à facettes) et la seule qui rende l'ajout d'une
//! valeur *élargissant* et l'ajout d'une facette *restreignant* — sans quoi
//! cocher une seconde case ne pourrait que vider la liste.
//!
//! # Trois pièges, tous déjà rencontrés dans ce dépôt
//!
//! 1. **Une liste vide ne doit produire AUCUN prédicat.** Ni `IN ()` (erreur de
//!    syntaxe sur les deux moteurs), ni un `1 = 1` de complaisance qui rendrait
//!    la bibliothèque ENTIÈRE alors que l'interface affiche un filtre actif.
//!    Tous les constructeurs ci-dessous rendent `None` sur une liste vide : le
//!    prédicat est alors simplement absent, et l'appelant n'a rien à lier.
//!
//! 2. **En SQLite, seul l'ORDRE de liaison compte.**
//!    [`SqliteDialect::placeholder`](super::engine::SqliteDialect) ignore son
//!    indice et rend toujours `?`. Un `IN (…)` bâti à la main qui oublie
//!    d'avancer le compteur donne donc un SQL parfaitement correct en SQLite et
//!    FAUX en PostgreSQL (`$1` répété, les valeurs suivantes décalées d'autant).
//!    [`Placeholders`] tient ce compteur une fois pour toutes, et l'appelant
//!    n'a qu'une règle à respecter : **empiler les valeurs dans l'ordre exact
//!    où les marqueurs ont été demandés**.
//!
//! 3. **Un `OU` de mille termes est refusé par SQLite et accepté par
//!    PostgreSQL.** La chaîne plate `a OR b OR c …` a une profondeur d'arbre
//!    égale à son nombre de termes, et SQLite plafonne à 1 000. Le même filtre
//!    rendait donc la bonne liste sur PostgreSQL et une liste VIDE sur SQLite.
//!    `ou_equilibre` ramène la profondeur à `log2(n)`.

use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Les champs que compare le TEXTE LIBRE d'Oxygen (#5192), dans l'ordre du
/// prédicat de [`condition_texte_libre`] — et, mot pour mot, ceux que le
/// client web compare dans sa fenêtre chargée (`OxygenView.svelte`) : le
/// titre, l'artiste, l'album, le label, et les termes de chemin
/// (`path_terms`, que `/library/tracks` rend calculés, voir
/// [`crate::library::full_text_search::termes_de_chemin`]).
///
/// Avant #5192, le serveur ne comparait que le titre et l'artiste, le
/// navigateur les quatre premiers : les deux ne rendaient pas la même liste.
pub const CHAMPS_DU_TEXTE_LIBRE: [&str; 5] =
    ["title", "artist_name", "album_title", "label", "path_terms"];

/// Le prédicat du texte libre d'Oxygen, pour la piste d'alias `t` : sous-
/// chaîne, insensible à la casse et aux accents, de l'un des
/// [`CHAMPS_DU_TEXTE_LIBRE`].
///
/// UNE rédaction pour les trois jumeaux — la liste (`list_filtered`), le
/// tirage par répertoire (`random_ids_in_folder`) et les effectifs du rail
/// (`facets::build_conditions`) — qui en tenaient chacun une copie. L'artiste
/// et l'album passent par sous-requête : le compteur du rail n'a aucune
/// jointure.
///
/// La saisie est LITTÉRALE : ses `%` et `_` sont échappés, comme le navigateur
/// qui fait un `includes`. Les doubles guillemets sont ôtés (`motif_like`).
/// Rend le prédicat et les valeurs à lier, dans l'ordre des marqueurs.
pub fn condition_texte_libre(
    ph: &mut Placeholders,
    query: &str,
) -> (String, Vec<super::backend::SqlValue>) {
    let motif = format!(
        "%{}%",
        super::track_repo::echapper_jokers_like(query.replace('"', "").trim())
    );
    let esc = super::track_repo::like_escape_clause();
    let like = |ph: &mut Placeholders| format!("LIKE LOWER(unaccent({})){esc}", ph.take());
    let titre = like(ph);
    let artiste = like(ph);
    let album = like(ph);
    let label = like(ph);
    let chemin = like(ph);
    // SQLite : la fonction Rust enregistrée, linéaire — l'expression pure
    // coûte ~60 µs par piste, et ce prédicat parcourt la bibliothèque entière.
    let termes = match ph.engine() {
        Engine::Sqlite => {
            crate::library::full_text_search::sql_sqlite_termes_de_chemin_de_piste("t")
        }
        Engine::Postgres => crate::library::full_text_search::sql_termes_de_chemin_de_piste("t"),
    };
    let sql = format!(
        "(LOWER(unaccent(t.title)) {titre} \
         OR t.artist_id IN (SELECT id FROM artists WHERE LOWER(unaccent(name)) {artiste}) \
         OR t.album_id IN (SELECT id FROM albums WHERE LOWER(unaccent(title)) {album}) \
         OR LOWER(unaccent(t.label)) {label} \
         OR LOWER(unaccent({termes})) {chemin})"
    );
    let valeurs = (0..CHAMPS_DU_TEXTE_LIBRE.len())
        .map(|_| super::backend::SqlValue::Text(motif.clone()))
        .collect();
    (sql, valeurs)
}

/// Générateur de marqueurs positionnels, partagé par les DEUX constructeurs de
/// prédicats de facettes (`TrackRepo::list_filtered` et
/// `routes::library::facets::build_conditions`), pour que la liste filtrée et
/// les effectifs affichés ne puissent pas diverger.
pub struct Placeholders {
    engine: Engine,
    next: usize,
}

impl Placeholders {
    /// Compteur neuf, premier marqueur en position 1 (convention PostgreSQL).
    pub fn new(engine: Engine) -> Self {
        Self { engine, next: 1 }
    }

    /// Reprend un compteur déjà entamé (les prédicats précédents ont consommé
    /// `next - 1` marqueurs).
    pub fn resuming_at(engine: Engine, next: usize) -> Self {
        Self {
            engine,
            next: next.max(1),
        }
    }

    /// Le moteur pour lequel ces marqueurs sont écrits.
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// Indice du PROCHAIN marqueur — pour rendre la main à du code qui tient
    /// encore son propre compteur.
    pub fn next_index(&self) -> usize {
        self.next
    }

    /// Un marqueur : `?` en SQLite, `$n` en PostgreSQL. Avance le compteur.
    pub fn take(&mut self) -> String {
        let s = match self.engine {
            Engine::Sqlite => SqliteDialect.placeholder(self.next),
            Engine::Postgres => PostgresDialect.placeholder(self.next),
        };
        self.next += 1;
        s
    }

    /// `n` marqueurs séparés par « , ». Utilisé par les constructeurs ci-dessous
    /// seulement, qui ont déjà écarté le cas `n == 0`.
    fn take_joined(&mut self, n: usize, sep: &str) -> String {
        (0..n).map(|_| self.take()).collect::<Vec<_>>().join(sep)
    }

    /// `expr IN (…)` — OU entre les valeurs, comparaison brute.
    /// `None` si la liste est vide (voir le piège n°1).
    pub fn in_list(&mut self, expr: &str, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        // Une seule valeur : `= ?` plutôt que `IN (?)`. Même résultat, mais le
        // SQL reste celui d'avant #2168 — les plans d'exécution et les traces
        // ne changent pas pour la sélection simple, qui reste le cas courant.
        if n == 1 {
            return Some(format!("{expr} = {}", self.take()));
        }
        Some(format!("{expr} IN ({})", self.take_joined(n, ", ")))
    }

    /// `LOWER(expr) IN (LOWER(…), …)` — OU entre les valeurs, insensible à la
    /// casse. `None` si la liste est vide.
    pub fn in_list_ci(&mut self, expr: &str, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        if n == 1 {
            return Some(format!("LOWER({expr}) = LOWER({})", self.take()));
        }
        let markers = (0..n)
            .map(|_| format!("LOWER({})", self.take()))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("LOWER({expr}) IN ({markers})"))
    }

    /// `(LOWER(expr) LIKE LOWER(…) OR …)` — pour les facettes « contient »
    /// (label, compositeur), où la valeur liée est déjà un motif `%…%`.
    /// Parenthésé : sans quoi le `OU` interne se ferait manger par le `ET`
    /// entre facettes. `None` si la liste est vide.
    ///
    /// Les termes sont assemblés par `ou_equilibre` — voir le piège n°3.
    pub fn or_like_ci(&mut self, expr: &str, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        if n == 1 {
            return Some(format!("LOWER({expr}) LIKE LOWER({})", self.take()));
        }
        // ⚠️ Les marqueurs sont pris ICI, dans l'ordre 1..n, AVANT tout
        // réassemblage : `ou_equilibre` ne fait que reparenthéser une suite
        // qu'il conserve de gauche à droite. L'ordre de liaison attendu par
        // l'appelant est donc exactement celui d'avant.
        let parts: Vec<String> = (0..n)
            .map(|_| format!("LOWER({expr}) LIKE LOWER({})", self.take()))
            .collect();
        Some(ou_equilibre(parts))
    }
}

/// Assemble des prédicats en **OU** sous forme d'arbre ÉQUILIBRÉ —
/// `((a OU b) OU (c OU d))` — au lieu de la chaîne plate `a OU b OU c OU d`.
///
/// # Le piège n°3 : SQLite compte la PROFONDEUR, pas le nombre de termes
///
/// `a OR b OR c OR …` s'analyse en un arbre binaire qui penche à gauche : sa
/// profondeur vaut le nombre de termes. Au-delà de **1000**, SQLite refuse la
/// requête à la préparation :
///
/// ```text
/// prepare: Expression tree is too large (maximum depth 1000)
/// ```
///
/// PostgreSQL, lui, avale la même chaîne sans broncher (mesuré jusqu'à 12 000
/// termes). Une facette « contient » à mille valeurs rendait donc **deux
/// résultats différents selon le moteur** : la bonne liste sur PostgreSQL, et
/// sur SQLite une requête en échec — avalée par `ou_defaut_journalise` (#2861)
/// et servie en `200 OK` avec `total = 0`. C'est-à-dire le pire cas que ce
/// module combat : un filtre actif qui rend une liste VIDE en silence.
///
/// L'arbre équilibré ramène la profondeur à `log2(n)` : 6 549 termes — le
/// maximum qu'une URL puisse porter, `http::Uri` refusant au-delà de 64 Kio —
/// tiennent en profondeur 13. Mesuré sur SQLite : la chaîne plate échoue dès
/// 1 000 termes, l'arbre équilibré passe jusqu'à 32 000, où c'est l'autre
/// limite qui parle (`too many SQL variables`, 32 766). Le maximum atteignable
/// par une requête HTTP étant de 13 098 marqueurs, la marge est de 2,5×.
///
/// **L'ordre est conservé.** Le réassemblage ne fait que déplacer des
/// parenthèses : le i-ème terme reste le i-ème, donc les marqueurs `$1..$n`
/// sortent toujours dans l'ordre et la pile de valeurs de l'appelant n'a pas à
/// changer — la règle de liaison de SQLite (voir le piège n°2) est intacte.
///
/// Pour `n <= 2` le résultat est mot pour mot celui d'avant.
fn ou_equilibre(mut parts: Vec<String>) -> String {
    debug_assert!(!parts.is_empty(), "appelé sur une liste non vide seulement");
    while parts.len() > 1 {
        let mut etage = Vec::with_capacity(parts.len().div_ceil(2));
        let mut it = parts.into_iter();
        while let Some(gauche) = it.next() {
            match it.next() {
                Some(droite) => etage.push(format!("({gauche} OR {droite})")),
                // Terme orphelin de l'étage : il remonte tel quel, ce qui
                // garde l'arbre équilibré et l'ordre inchangé.
                None => etage.push(gauche),
            }
        }
        parts = etage;
    }
    parts.pop().unwrap_or_default()
}

/// Assemble en **OU** des prédicats déjà écrits, sans marqueur — pour les
/// facettes à vocabulaire FERMÉ (`favorite`, `untagged`), dont le SQL est un
/// littéral choisi dans une liste et jamais une entrée de la requête.
///
/// Rend `None` sur une liste vide : là encore, pas de prédicat plutôt qu'un
/// prédicat qui laisserait tout passer.
pub fn any_of(conds: Vec<String>) -> Option<String> {
    match conds.len() {
        0 => None,
        1 => conds.into_iter().next(),
        _ => Some(format!("({})", conds.join(" OR "))),
    }
}

/// Normalise les valeurs reçues pour UNE facette : les vides tombent, les
/// doublons aussi (ordre d'apparition conservé).
///
/// Le rejet des vides est le garde-fou du piège n°1 côté entrée : `?format=`
/// ne doit pas devenir un filtre — ni un filtre qui ne rend rien, ni un filtre
/// qui rend tout. Il ne doit pas devenir un filtre du tout.
pub fn normalize(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        let v = v.trim();
        if v.is_empty() || out.iter().any(|k| k == v) {
            continue;
        }
        out.push(v.to_string());
    }
    out
}

/// Idem pour les facettes numériques.
pub fn normalize_ints(values: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::with_capacity(values.len());
    for v in values {
        if !out.contains(v) {
            out.push(*v);
        }
    }
    out
}

/// Prédicat « cette piste est en favori » pour le profil 1. Vocabulaire FERMÉ :
/// toute autre valeur ne filtre RIEN plutôt que de tout exclure.
///
/// Vit ici, et non dans chacun des deux constructeurs de prédicats, parce que
/// c'est exactement le genre de littéral qu'on recopie une fois de trop : une
/// facette qui compterait autrement que la liste qu'elle filtre serait pire
/// qu'une facette absente.
pub fn favorite_condition(kind: &str) -> Option<&'static str> {
    match kind {
        "album" => Some(
            "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
             AND f.item_type = 'album' AND f.item_id = t.album_id)",
        ),
        "track" => Some(
            "EXISTS (SELECT 1 FROM favorites f WHERE f.profile_id = 1 \
             AND f.item_type = 'track' AND f.item_id = t.id)",
        ),
        _ => None,
    }
}

/// La règle de lecture du tag **Dynamic Range d'album** (#2144), écrite UNE
/// fois pour les trois lieux qui s'en servent : la grille d'albums
/// (`AlbumRepo::dr_album_join`), le rail de facettes (`facets::dr_facet`) et la
/// liste filtrée (`TrackRepo::list_filtered`). Une facette qui compterait
/// autrement que la liste qu'elle filtre serait pire qu'une facette absente.
///
/// Le DR vit dans le magasin ouvert `track_metadata`, sous **deux** clés — le
/// tag d'album `dr_album` et le tag de piste `dr_track` — en **TEXTE** déjà
/// normalisé au scan (« DR12 », « DR 12 » → « 12 », commit 7cdc93ff). Trois
/// gardes avant le `CAST`, parce qu'un magasin ouvert accepte n'importe quelle
/// chaîne : non vide, trois caractères au plus, et **rien que des chiffres**.
/// Sans le troisième, `CAST('abc' AS INTEGER)` vaut `0` en SQLite (donc
/// « DR0 », une valeur de facette inventée) et **lève** en PostgreSQL (donc la
/// requête entière échoue) — la 17ᵉ divergence PG/SQLite qu'on refuse
/// d'ajouter.
///
/// Les deux clés passent ensemble parce que [`DR_ALBUM_VALUE`] les départage
/// lui-même : le tag d'album prime, les tags de piste ne servent qu'à défaut.
/// Les filtrer ici obligerait chaque appelant à refaire ce choix.
///
/// L'alias de `track_metadata` est `tm`, celui de `tracks` est `tdr` : jamais
/// `t`, qui est déjà pris par la requête ENGLOBANTE côté pistes.
pub fn dr_tag_where(engine: Engine) -> String {
    let only_digits = match engine {
        // SQLite n'a pas de `~` ; GLOB est son motif sensible à la casse, et
        // `[^0-9]` y est une classe de caractères niée.
        Engine::Sqlite => "tm.value NOT GLOB '*[^0-9]*'",
        Engine::Postgres => "tm.value ~ '^[0-9]+$'",
    };
    format!(
        "tm.key IN ('dr_album', 'dr_track') AND tm.value <> '' \
         AND LENGTH(tm.value) <= 3 AND {only_digits}"
    )
}

/// Le DR d'un album à partir de ses pistes — sur un groupe `GROUP BY album_id`
/// filtré par [`dr_tag_where`].
///
/// Deux étages, dans cet ordre (#1388) :
///
/// 1. **Le tag d'album**, `ALBUM DYNAMIC RANGE`, quand une piste au moins le
///    porte. C'est un tag d'ALBUM recopié sur chaque piste ; en théorie toutes
///    portent la même valeur, et quand elles divergent (album ré-tagué à
///    moitié) le `MAX` donne une valeur stable et reproductible plutôt qu'une
///    valeur au hasard du plan d'exécution.
/// 2. **La moyenne des DR de piste**, arrondie, quand aucune piste ne porte le
///    tag d'album mais que des pistes portent `DYNAMIC RANGE`. C'est la façon
///    dont les mesureurs eux-mêmes composent la valeur d'album à partir des
///    pistes, et c'est le cas de Patatorz à l'envers : lui ne tague que
///    l'album ; foobar2000, lui, écrit le tag par piste et rien pour l'album,
///    et la fiche restait alors muette (« DR » nulle part) alors que chaque
///    piste affichait le sien.
///
/// `COALESCE` et non `MAX` des deux : un tag d'album explicite doit gagner,
/// même quand il est plus bas que la moyenne des pistes. C'est la valeur que
/// le mesureur a écrite.
///
/// Une seule expression pour la grille, le tri, la tranche, le rail de
/// facettes et la fiche : deux règles auraient fini par afficher un DR que la
/// facette ne connaît pas.
pub const DR_ALBUM_VALUE: &str = "COALESCE(\
     MAX(CASE WHEN tm.key = 'dr_album' THEN CAST(tm.value AS INTEGER) END), \
     CAST(ROUND(AVG(CASE WHEN tm.key = 'dr_track' THEN CAST(tm.value AS INTEGER) END)) AS INTEGER))";

/// « D'où vient la valeur ? », en SQL : `1` quand c'est le tag d'album, `0`
/// quand c'est la moyenne des pistes.
///
/// Colonne séparée plutôt que devinée après coup : depuis la valeur seule, on
/// ne peut pas distinguer un album tagué DR12 d'un album dont les pistes
/// moyennent 12. Et l'écran ne dit pas la même chose des deux.
pub const DR_ALBUM_FROM_TAG: &str = "MAX(CASE WHEN tm.key = 'dr_album' THEN 1 ELSE 0 END)";

/// La table dérivée « un album, son DR » — à joindre sur `album_id`.
///
/// Elle ne balaie que les lignes `dr_album` et `dr_track` de `track_metadata`,
/// que `idx_track_metadata_key` sert directement : c'est ce qui la rend
/// jointe-able sans coût, là où une sous-requête CORRÉLÉE par piste referait le
/// travail des centaines de milliers de fois.
pub fn dr_album_source(engine: Engine) -> String {
    format!(
        "SELECT tdr.album_id AS album_id, {DR_ALBUM_VALUE} AS dr \
           FROM track_metadata tm JOIN tracks tdr ON tdr.id = tm.track_id \
          WHERE {} GROUP BY tdr.album_id",
        dr_tag_where(engine)
    )
}

/// Prédicat « l'album de CETTE piste porte l'un des DR sélectionnés » — sur
/// l'alias `t` des requêtes de pistes (#2144).
///
/// `having` est le `IN (…)` déjà construit par [`Placeholders::in_list`] sur
/// [`DR_ALBUM_VALUE`] : l'appelant tient son compteur de marqueurs, ce module
/// n'en fabrique aucun.
///
/// Sous-requête NON corrélée : le moteur l'évalue une fois. Un album sans tag
/// n'en fait jamais partie — la facette est toujours RESTRICTIVE, comme la
/// tranche `DrRange` de la grille d'albums.
pub fn dr_album_in(engine: Engine, having: &str) -> String {
    format!(
        "t.album_id IN (SELECT tdr.album_id FROM track_metadata tm \
           JOIN tracks tdr ON tdr.id = tm.track_id \
          WHERE {} GROUP BY tdr.album_id HAVING {having})",
        dr_tag_where(engine)
    )
}

/// Prédicat « cet album n'est PAS masqué » (#1391), pour les requêtes
/// d'ALBUMS — alias `a`, celui de `AlbumRepo::sql::select_album()`.
///
/// Vit ici, à côté de [`favorite_condition`], et pour la même raison : c'est
/// le littéral qu'on recopierait une fois de trop. La liste d'albums, son
/// compteur de pagination et la recherche doivent exclure EXACTEMENT le même
/// ensemble, sinon la grille pagine faux.
///
/// `hidden_items` référence l'album par rowid, réconcilié après chaque scan
/// (`hidden_repo::reconcile`) — même mécanique que `favorites`. `profile_id`
/// n'est volontairement PAS lu : le masquage est global (aucune vue
/// bibliothèque ne connaît le profil aujourd'hui, cf. `active_profile.rs`).
pub fn hidden_albums_excluded() -> &'static str {
    "NOT EXISTS (SELECT 1 FROM hidden_items h \
     WHERE h.item_type = 'album' AND h.item_id = a.id)"
}

/// Prédicat « la piste n'appartient PAS à un album masqué » (#1391), pour les
/// requêtes de PISTES — alias `t`, celui de `TrackRepo::sql::select_track()`.
///
/// Une piste SANS album (`t.album_id` NULL) reste visible : le `NOT EXISTS`
/// est vrai quand la sous-requête ne trouve rien, NULL compris — pas besoin
/// du détour `COALESCE` qu'imposerait un `LEFT JOIN albums`.
pub fn hidden_tracks_excluded() -> &'static str {
    "NOT EXISTS (SELECT 1 FROM hidden_items h \
     WHERE h.item_type = 'album' AND h.item_id = t.album_id)"
}

/// Prédicat « la piste n'est PAS bannie pour ce profil » (#4806), pour les
/// requêtes de SÉLECTION AUTOMATIQUE — alias `t`, comme ci-dessus.
///
/// À poser sur l'aléatoire, les smart playlists, l'enchaînement de fin de
/// file, la radio d'artiste, les recommandations — et NULLE PART sur une
/// liste d'affichage (album, playlist, recherche) : un titre banni reste
/// visible, grisé ; c'est le drapeau `banned` des routes qui le dit, pas un
/// filtre. Le profil est un `i64` de confiance inscrit en clair, comme les
/// ids de `TrackMetadataRepo::sql::get_key_for_tracks`.
pub fn banned_tracks_excluded(profile_id: i64) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM hidden_items hb \
         WHERE hb.profile_id = {profile_id} AND hb.item_type = 'track' AND hb.item_id = t.id)"
    )
}

/// Prédicat « ce titre de SERVICE n'est PAS banni pour ce profil » (#4806) —
/// la jumelle de [`banned_tracks_excluded`] pour la paire `(source,
/// source_id)`, table `streaming_hidden_items`. `source_col` et `id_col` sont
/// les colonnes de la requête englobante (`sf.service`, `sf.service_id` pour
/// les favoris de service) — des NOMS écrits par le code, jamais une saisie.
///
/// La provenance est comparée en minuscules : c'est la forme sous laquelle
/// `hidden_repo::source_normalisee` l'écrit.
pub fn banned_streaming_excluded(profile_id: i64, source_col: &str, id_col: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM streaming_hidden_items hsb \
         WHERE hsb.profile_id = {profile_id} AND hsb.item_type = 'track' \
         AND hsb.source = LOWER(TRIM({source_col})) AND hsb.source_id = {id_col})"
    )
}

/// Le nom de l'artiste d'un album, en sous-requête corrélée.
///
/// Volontairement PAS une jointure : le prédicat de doublon ci-dessous doit
/// tenir dans un `WHERE` seul, et il est employé aussi bien par la liste
/// (`... FROM albums a LEFT JOIN artists ar ...`) que par son compteur
/// (`SELECT COUNT(*) FROM albums a WHERE ...`), qui n'a AUCUNE jointure. Un
/// prédicat qui supposerait l'alias `ar` marcherait dans la liste et
/// exploserait dans le compteur — donc la pagination mentirait sur PostgreSQL
/// comme sur SQLite.
fn nom_d_artiste(alias_album: &str, alias_artiste: &str) -> String {
    format!(
        "COALESCE((SELECT {alias_artiste}.name FROM artists {alias_artiste} \
         WHERE {alias_artiste}.id = {alias_album}.artist_id), '')"
    )
}

/// « Cette source n'est pas le disque local. » `source` vaut `'local'` par
/// défaut mais une base migrée porte des NULL et des chaînes vides — le même
/// `COALESCE(NULLIF(...), 'local')` que `AlbumRepo::sql::count_by_source`.
fn source_est(alias: &str, operateur: &str) -> String {
    format!("COALESCE(NULLIF({alias}.source, ''), 'local') {operateur} 'local'")
}

/// « Cet album DISTANT a une contrepartie LOCALE. » Cœur du masquage #4146.
///
/// Les règles de rapprochement sont celles de
/// [`super::favorites_reconcile::find_album_by_identity`], et pour la même
/// raison qu'ailleurs : un album distant et son équivalent local n'ont AUCUN
/// identifiant commun — ni `source_id`, ni MBID garanti —, seule l'identité
/// (titre, artiste) les rapproche. Transposées en SQL ensembliste, car un
/// appel par ligne sur 4 255 albums ferait 4 255 requêtes :
///
/// * artiste connu : titre + artiste, en `LOWER` des deux côtés ;
/// * artiste inconnu : titre seul, **uniquement si non ambigu** — un seul
///   album local porte ce titre. Sans cette garde, un « Live » distant sans
///   artiste serait masqué par le « Live » de n'importe qui.
///
/// Un album LOCAL n'est jamais masqué : le premier terme le sort d'emblée.
fn double_par_un_local(engine: Engine, alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM albums loc WHERE {})",
        condition_de_doublon(engine, alias)
    )
}

/// « Ces deux TITRES d'album sont égaux à la casse près » — écrit pour que
/// l'index serve (#4800).
///
/// `LOWER(loc.title) = LOWER(a.title)` rendait `idx_albums_title` inutile :
/// aucun index ne sert une expression, et SQLite parcourait `albums` en
/// entier pour CHAQUE ligne de la requête englobante — 9 427 × 9 427 sur la
/// base du .18, 28 s par `COUNT`, exécuté deux fois par page de la grille.
/// C'est le piège déjà mesuré par [`super::home_queries::HISTORIQUE_VERS_ALBUM`]
/// (19 ms contre 83 s), retombé ici une seconde fois.
///
/// * SQLite : `idx_albums_title ON albums(title COLLATE NOCASE)` existe
///   depuis la migration 16. Une égalité `COLLATE NOCASE` la prend, et sa
///   sémantique est EXACTEMENT celle de `LOWER()` : les deux ne replient que
///   les 26 lettres ASCII (le `lower()` de SQLite sans ICU ne fait pas plus).
///   Même ensemble masqué, donc, au caractère près.
/// * PostgreSQL n'a pas de `COLLATE NOCASE` ; son `LOWER()`, lui, replie tout
///   l'Unicode — et c'est le texte d'origine qui reste, inchangé. Là-bas le
///   planificateur transforme le `NOT EXISTS` corrélé en anti-jointure par
///   hachage sur cette égalité : il ne rejoue pas la sous-requête par ligne.
fn titres_egaux_sans_casse(engine: Engine, gauche: &str, droit: &str) -> String {
    match engine {
        Engine::Sqlite => format!("{gauche} = {droit} COLLATE NOCASE"),
        Engine::Postgres => format!("LOWER({gauche}) = LOWER({droit})"),
    }
}

/// Le CORPS du rapprochement #4146 : l'album distant `{alias}` et l'album
/// local `loc` sont le même disque. Sorti de [`double_par_un_local`] pour que
/// la mention réciproque « aussi sur … » (phase 5 du chantier UPnP,
/// [`super::album_repo::AlbumRepo::aussi_sur`]) rapproche EXACTEMENT ce que la
/// grille masque — deux copies de la règle divergeraient au premier
/// correctif, et un album serait masqué sans être signalé, ou l'inverse.
///
/// `engine` ne change que l'écriture de l'égalité des titres
/// ([`titres_egaux_sans_casse`]), jamais la règle.
pub(crate) fn condition_de_doublon(engine: Engine, alias: &str) -> String {
    let nom_distant = nom_d_artiste(alias, "ar_dist");
    let nom_local = nom_d_artiste("loc", "ar_loc");
    let distant = source_est(alias, "<>");
    let local = source_est("loc", "=");
    let ambigu = source_est("amb", "=");
    let titre_local = titres_egaux_sans_casse(engine, "loc.title", &format!("{alias}.title"));
    let titre_ambigu = titres_egaux_sans_casse(engine, "amb.title", &format!("{alias}.title"));
    format!(
        "{distant} AND {local} \
         AND {titre_local} \
         AND (LOWER({nom_local}) = LOWER({nom_distant}) \
              OR ({nom_distant} = '' \
                  AND (SELECT COUNT(*) FROM albums amb \
                       WHERE {titre_ambigu} AND {ambigu}) = 1))"
    )
}

/// Prédicat « cet album distant n'est PAS doublé par un local » (#4146), pour
/// les requêtes d'ALBUMS — alias `a`, celui de `AlbumRepo::sql::select_album()`.
///
/// Arbitrage de Bertrand du 14/09/2026 : quand un album existe des deux côtés,
/// **seul le local est rendu**. Le distant reste en base, indexé, jouable par
/// `GET /library/albums/{id}` — il n'est qu'absent des LISTES, et il y revient
/// dès que sa contrepartie locale disparaît. **Masquer n'est pas effacer.**
///
/// Vit ici pour la même raison que [`hidden_albums_excluded`] : la liste, son
/// compteur de pagination et le compteur de tranche DR doivent exclure
/// EXACTEMENT le même ensemble, sinon la grille saute des pages.
pub fn album_distant_double_exclu(engine: Engine, alias: &str) -> String {
    format!("NOT {}", double_par_un_local(engine, alias))
}

/// Prédicat jumeau pour les requêtes de PISTES (#4146) — alias `t`, celui de
/// `TrackRepo::sql::select_track()`.
///
/// La piste suit son album : si l'album distant est masqué par un local, ses
/// pistes le sont aussi. Une piste SANS album reste visible — le `NOT EXISTS`
/// est vrai quand la sous-requête ne trouve rien, NULL compris, exactement
/// comme [`hidden_tracks_excluded`].
pub fn pistes_album_distant_double_exclu(engine: Engine) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM albums dist \
         WHERE dist.id = t.album_id AND {})",
        double_par_un_local(engine, "dist")
    )
}

/// Prédicat « cette ligne n'est pas la copie de MOINDRE qualité d'une piste
/// que la vue montre déjà » (#4101) — alias `t`, celui de
/// `TrackRepo::sql::select_track()`.
///
/// ## Ce que JeromeQ voyait (forum 1781, 13/09/2026)
///
/// Led Zeppelin IV, huit pistes, **seize lignes** dans la vue Oxygen : chaque
/// titre deux fois, avec le même numéro de piste. La fiche de l'album, elle,
/// en montrait huit, et la file d'attente en enfilait huit.
///
/// ## Où naît le doublon, et pourquoi il ne se voyait QUE là
///
/// Pas dans une jointure : les trois jointures de `select_track()` portent sur
/// des clés primaires et ne peuvent pas multiplier une ligne. Pas dans la
/// pagination : le client écarte déjà les identifiants qu'il connaît. Ce sont
/// **deux lignes `tracks` bien réelles**, une par fichier — le cas #1362 de
/// Cyrille Moutia, un CD rippé posé à côté d'une copie récupérée ailleurs,
/// dans le même dossier donc dans le même album.
///
/// Le repli de ces copies EXISTE depuis #1362, et il est posé sur les trois
/// autres surfaces : la fiche d'un album
/// ([`crate::db::track_repo::dedup_display_tracks`]), la file
/// (`resoudre_pistes_d_album`) et le compteur `albums.track_count`
/// ([`crate::db::track_repo::sql_compte_pistes_visibles`]). Il n'a jamais été
/// posé sur `GET /library/tracks` — **la seule route que la vue Oxygen
/// appelle**. Un dédoublonnage qui n'agit que sur un chemin : voilà le défaut.
///
/// ## Pourquoi en SQL et pas en repliant la page rendue
///
/// Parce que la vue est PAGINÉE. Replier la page après coup rendrait 195
/// lignes pour une fenêtre de 200 en annonçant un `total` qui, lui, compterait
/// les 200 : la fenêtre suivante partirait d'un mauvais décalage et sauterait
/// des pistes. Même raison que [`album_distant_double_exclu`] — la liste et
/// son compteur excluent EXACTEMENT le même ensemble.
///
/// ## La clé, et le survivant
///
/// La clé est celle de [`crate::db::track_repo::dedup_display_tracks`] mot
/// pour mot : album, disque, numéro, titre en minuscules sans blancs de bord.
/// Deux morceaux réellement distincts n'y collisionnent pas.
///
/// Le survivant est la copie de **meilleure qualité** (#1362 : l'AIFF, pas
/// l'AAC), barème [`crate::library::quality::score_qualite`] transcrit par son
/// jumeau SQL. À score égal, la ligne de plus petit `id` — un départage
/// TOTAL, sans lequel deux copies jumelles se masqueraient l'une l'autre et la
/// piste disparaîtrait entièrement.
///
/// ## Deux écarts DÉLIBÉRÉS avec le repli en mémoire
///
/// * une piste **sans album** (`t.album_id` NUL) n'est jamais repliée. En
///   Rust la clé est un `Option<i64>` et `None == None` : deux pistes sans
///   album qui partagent un numéro et un titre se replient l'une sur l'autre,
///   à travers toute la bibliothèque. Ici `mieux.album_id = t.album_id` est
///   NUL-sûr et ne rapproche rien — c'est la portée de
///   [`crate::db::track_repo::sql_compte_pistes_visibles`], qui compte déjà
///   par album ;
/// * `LOWER` de SQLite ne replie que l'ASCII, là où `to_lowercase` de Rust
///   replie tout l'Unicode : deux lignes qui ne diffèrent QUE par la casse
///   d'une lettre accentuée restent deux lignes ici. Même écart, déjà
///   documenté, que le compteur de pistes visibles — il ne fait pas
///   apparaître de doublon qui n'existait pas, il en laisse passer un que
///   l'autre chemin repliait.
pub fn copie_de_moindre_qualite_exclue() -> String {
    use crate::library::quality::{sql_meme_score, sql_strictement_meilleure};
    format!(
        "NOT EXISTS (SELECT 1 FROM tracks mieux \
         WHERE t.album_id IS NOT NULL AND mieux.album_id = t.album_id \
         AND mieux.id <> t.id \
         AND COALESCE(mieux.disc_number, 1) = COALESCE(t.disc_number, 1) \
         AND COALESCE(mieux.track_number, 0) = COALESCE(t.track_number, 0) \
         AND LOWER(TRIM(COALESCE(mieux.title, ''))) = LOWER(TRIM(COALESCE(t.title, ''))) \
         AND ({} OR ({} AND mieux.id < t.id)))",
        sql_strictement_meilleure("mieux", "t"),
        sql_meme_score("mieux", "t"),
    )
}

/// Prédicat SQL d'une étiquette manquante. Liste FERMÉE : toute autre valeur
/// rend `None` et ne filtre rien, plutôt que d'injecter quoi que ce soit.
///
/// « Manquant » vaut ici NULL **ou** chaîne vide : un tag effacé par un éditeur
/// laisse souvent une chaîne vide, et l'utilisateur qui range sa bibliothèque
/// ne fait pas la différence entre les deux.
pub fn untagged_condition(field: &str) -> Option<&'static str> {
    untagged_condition_for_engine(field, Engine::Sqlite)
}

/// Le rail et la liste partagent la même définition des genres absents (#3979).
/// Un tableau vide, malformé ou qui ne contient pas exclusivement des chaînes
/// ne fournit aucun genre, comme le décodage Vec<String> de la facette.
pub fn untagged_condition_for_engine(field: &str, engine: Engine) -> Option<&'static str> {
    match field {
        "genre" => Some(match engine {
            Engine::Sqlite => {
                "((t.genre IS NULL OR TRIM(t.genre) = '') AND NOT EXISTS (\
                 SELECT COUNT(*) FROM json_each(CASE WHEN json_valid(t.genres) THEN \
                 CASE WHEN json_type(t.genres) = 'array' THEN t.genres ELSE '[]' END \
                 ELSE '[]' END) g HAVING COUNT(*) > 0 \
                 AND SUM(CASE WHEN g.type <> 'text' THEN 1 ELSE 0 END) = 0 \
                 AND SUM(CASE WHEN TRIM(CAST(g.value AS TEXT)) <> '' THEN 1 ELSE 0 END) > 0))"
            }
            Engine::Postgres => {
                "((t.genre IS NULL OR TRIM(t.genre) = '') AND NOT EXISTS (\
                 SELECT COUNT(*) FROM json_array_elements((CASE WHEN t.genres IS JSON ARRAY \
                 THEN t.genres ELSE '[]' END)::json) g(value) HAVING COUNT(*) > 0 \
                 AND SUM(CASE WHEN json_typeof(g.value) <> 'string' THEN 1 ELSE 0 END) = 0 \
                 AND SUM(CASE WHEN TRIM(g.value #>> '{}') <> '' THEN 1 ELSE 0 END) > 0))"
            }
        }),
        "year" => Some("(t.year IS NULL OR t.year = 0)"),
        "artist" => Some("t.artist_id IS NULL"),
        "album" => Some("t.album_id IS NULL"),
        // La pochette vit sur l'album : une piste sans album n'en a pas non plus.
        "cover" => Some(
            "(t.album_id IS NULL OR EXISTS (SELECT 1 FROM albums al \
              WHERE al.id = t.album_id AND (al.cover_path IS NULL OR al.cover_path = '')))",
        ),
        _ => None,
    }
}

/// Les valeurs actives de CHAQUE facette d'Oxygen, pour un listage de pistes.
///
/// Une structure nommée plutôt que vingt-trois arguments positionnels : c'est
/// exactement ainsi qu'une facette avait déjà été perdue en route
/// (`original_year` ajouté à la suite d'un `;`, donc dans une fermeture créée
/// et jetée — le tri par année d'enregistrement partait sur le chemin NON
/// filtré). Ici, un champ oublié à l'appel reste à `Vec::new()`, c'est-à-dire
/// « facette inactive », et ne peut plus se confondre avec un filtre.
///
/// Chaque `Vec` porte les valeurs d'UNE facette, combinées en **OU** ; les
/// champs entre eux se combinent en **ET**. Un `Vec` vide = facette inactive.
#[derive(Default, Clone, Debug)]
pub struct TrackFilter {
    pub genres: Vec<String>,
    pub years: Vec<i64>,
    pub formats: Vec<String>,
    pub sample_rates: Vec<i64>,
    pub bit_depths: Vec<i64>,
    pub sources: Vec<String>,
    pub labels: Vec<String>,
    pub composers: Vec<String>,
    /// Instruments joués sur la piste, lus dans `track_credits.instrument`
    /// (CRD-6). Une facette de jointure : le prédicat vit dans
    /// [`instrument_exists`], jumeau de `dr_album_in`.
    pub instruments: Vec<String>,
    pub artists: Vec<String>,
    pub countries: Vec<String>,
    pub moods: Vec<String>,
    pub source_medias: Vec<String>,
    pub ratings: Vec<i64>,
    pub original_years: Vec<i64>,
    /// Dynamic Range de l'ALBUM (#2144), une valeur entière par pastille.
    ///
    /// **C'est là que vivent les « tranches » du ticket.** Plusieurs valeurs
    /// cochées se combinent en OU comme toute facette : cocher 12, 13 et 14
    /// EST la tranche « DR12 à DR14 ». Le serveur ne grave donc aucune borne
    /// nommée — l'issue n'en fixe aucune, les bornes de MinimServer citées en
    /// modèle n'ont jamais été relevées, et un découpage inventé ici
    /// survivrait dans le contrat HTTP à la mesure qui le contredirait.
    pub dynamic_ranges: Vec<i64>,
    /// `track` et/ou `album` (profil 1).
    pub favorites: Vec<String>,
    pub playlists: Vec<String>,
    /// `genre`, `year`, `artist`, `album` ou `cover`.
    pub untagged: Vec<String>,
    /// Fil d'Ariane des Répertoires : MONOVALUÉ par nature (une position dans
    /// un arbre, pas une valeur parmi d'autres).
    pub folder: Option<String>,
    /// Collection manuelle résolue en identifiants d'albums. MONOVALUÉE :
    /// une collection n'est pas une valeur de métadonnée mais un ensemble
    /// enregistré, résolu par deux moteurs distincts (JSON manuel / règles
    /// intelligentes) — leur union appelle un autre chantier.
    pub collection_ids: Option<Vec<i64>>,
    /// Collection intelligente résolue en identifiants de pistes.
    pub collection_track_ids: Option<Vec<i64>>,
    /// Recherche libre — ce n'est pas une facette, elle reste monovaluée.
    pub q: Option<String>,
}

/// `track_credits.track_id` s'écrit comme `t.id` selon le moteur : entier en
/// SQLite, mais `TEXT` sur le miroir PostgreSQL (copie SQLite→PG). Comparer
/// sans conversion y donne `text = bigint`, que PG refuse — le même écueil que
/// l'écriture des crédits, liée en chaîne pour cette raison.
pub fn track_id_pour_track_credits(engine: Engine) -> &'static str {
    match engine {
        Engine::Sqlite => "t.id",
        Engine::Postgres => "CAST(t.id AS TEXT)",
    }
}

/// Le prédicat de la facette « instrument » (CRD-6) : la piste porte au moins
/// un crédit dont l'instrument est dans la sélection. `inner` est la liste
/// produite par [`Placeholders::in_list_ci`] sur `tc.instrument`, pour que le
/// compteur de marqueurs reste unique. Le JUMEAU de ce prédicat est appelé
/// depuis `track_repo::list_filtered` et depuis les facettes du serveur.
pub fn instrument_exists(engine: Engine, inner: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = {} AND {inner})",
        track_id_pour_track_credits(engine)
    )
}

impl TrackFilter {
    /// Au moins une facette est-elle active ?
    ///
    /// ⚠️ C'est le garde-fou du piège n°1 vu du dessus : la route ne doit
    /// emprunter le chemin filtré QUE si ce chemin va produire au moins un
    /// prédicat. Sinon `?format=` (valeur vide) prendrait le chemin filtré,
    /// n'y trouverait aucun prédicat, et rendrait la bibliothèque ENTIÈRE en
    /// silence.
    ///
    /// Réciproquement, `false` ici garantit qu'aucun prédicat n'aurait été
    /// produit : le chemin non filtré rend alors exactement la même chose.
    pub fn is_active(&self) -> bool {
        !self.genres.is_empty()
            || !self.years.is_empty()
            || !self.formats.is_empty()
            || !self.sample_rates.is_empty()
            || !self.bit_depths.is_empty()
            || !self.sources.is_empty()
            || !self.labels.is_empty()
            || !self.composers.is_empty()
            || !self.instruments.is_empty()
            || !self.artists.is_empty()
            || !self.countries.is_empty()
            || !self.moods.is_empty()
            || !self.source_medias.is_empty()
            || !self.ratings.is_empty()
            || !self.original_years.is_empty()
            || !self.dynamic_ranges.is_empty()
            // Vocabulaires FERMÉS : une valeur inconnue ne produit aucun
            // prédicat, donc elle ne doit pas non plus activer le chemin filtré.
            || self.favorites.iter().any(|k| favorite_condition(k).is_some())
            || !self.playlists.is_empty()
            || self.untagged.iter().any(|k| untagged_condition(k).is_some())
            || self.folder.as_deref().is_some_and(|s| !s.is_empty())
            || self.collection_ids.is_some()
            || self.collection_track_ids.is_some()
            || self.q.as_deref().is_some_and(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRD-6 : le prédicat d'instrument est un EXISTS sur `track_credits`,
    /// insensible à la casse, et compare `track_id` à `t.id` selon le moteur.
    #[test]
    fn le_predicat_d_instrument_suit_le_moteur() {
        let mut sq = Placeholders::new(Engine::Sqlite);
        let inner = sq.in_list_ci("tc.instrument", 2).unwrap();
        assert_eq!(
            instrument_exists(Engine::Sqlite, &inner),
            "EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = t.id AND LOWER(tc.instrument) IN (LOWER(?), LOWER(?)))"
        );
        let mut pg = Placeholders::new(Engine::Postgres);
        let inner = pg.in_list_ci("tc.instrument", 1).unwrap();
        assert_eq!(
            instrument_exists(Engine::Postgres, &inner),
            "EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = CAST(t.id AS TEXT) AND LOWER(tc.instrument) = LOWER($1))"
        );
        assert!(
            TrackFilter {
                instruments: vec!["oud".into()],
                ..Default::default()
            }
            .is_active()
        );
    }

    /// Le piège n°2, écrit noir sur blanc : en SQLite tous les marqueurs se
    /// ressemblent, en PostgreSQL ils sont numérotés — et le compteur doit
    /// avancer d'autant de crans qu'il y a de valeurs, sinon les paramètres
    /// SUIVANTS sont liés au mauvais numéro.
    #[test]
    fn in_list_numerote_les_marqueurs_par_moteur() {
        let mut sq = Placeholders::new(Engine::Sqlite);
        assert_eq!(
            sq.in_list("t.sample_rate", 3).unwrap(),
            "t.sample_rate IN (?, ?, ?)"
        );
        assert_eq!(sq.next_index(), 4, "le compteur doit avancer de 3 crans");
        assert_eq!(sq.in_list("t.year", 1).unwrap(), "t.year = ?");
        assert_eq!(sq.next_index(), 5);

        let mut pg = Placeholders::new(Engine::Postgres);
        assert_eq!(
            pg.in_list("t.sample_rate", 3).unwrap(),
            "t.sample_rate IN ($1, $2, $3)"
        );
        assert_eq!(pg.in_list("t.year", 1).unwrap(), "t.year = $4");
        assert_eq!(pg.next_index(), 5);
    }

    #[test]
    fn in_list_ci_et_or_like_ci_avancent_le_meme_compteur() {
        let mut pg = Placeholders::new(Engine::Postgres);
        assert_eq!(
            pg.in_list_ci("t.format", 2).unwrap(),
            "LOWER(t.format) IN (LOWER($1), LOWER($2))"
        );
        assert_eq!(
            pg.or_like_ci("t.label", 2).unwrap(),
            "(LOWER(t.label) LIKE LOWER($3) OR LOWER(t.label) LIKE LOWER($4))"
        );
        assert_eq!(pg.next_index(), 5);

        let mut sq = Placeholders::new(Engine::Sqlite);
        assert_eq!(
            sq.in_list_ci("t.format", 2).unwrap(),
            "LOWER(t.format) IN (LOWER(?), LOWER(?))"
        );
        assert_eq!(
            sq.or_like_ci("t.label", 2).unwrap(),
            "(LOWER(t.label) LIKE LOWER(?) OR LOWER(t.label) LIKE LOWER(?))"
        );
        assert_eq!(sq.next_index(), 5);
    }

    /// Le piège n°3, à la source : la PROFONDEUR du `OU` reste logarithmique.
    ///
    /// C'est ce que SQLite compte, et ce qu'il plafonne à 1 000. On mesure ici
    /// l'imbrication maximale de parenthèses du prédicat rendu ; le fait que la
    /// requête s'exécute vraiment est prouvé un cran plus haut, par
    /// `facettes_multivaleurs::une_facette_a_mille_valeurs_rend_encore_lunion`.
    #[test]
    fn le_ou_dune_facette_reste_peu_profond() {
        fn profondeur(sql: &str) -> usize {
            let (mut cour, mut max) = (0usize, 0usize);
            for c in sql.chars() {
                match c {
                    '(' => {
                        cour += 1;
                        max = max.max(cour);
                    }
                    ')' => cour = cour.saturating_sub(1),
                    _ => {}
                }
            }
            max
        }
        // 6 549 : le plus grand nombre de valeurs qu'une URL puisse porter pour
        // une facette (`http::Uri` refuse au-delà de 64 Kio).
        let mut ph = Placeholders::new(Engine::Sqlite);
        let sql = ph.or_like_ci("t.genres", 6549).unwrap();
        // `LOWER(…)` pose déjà 2 niveaux par terme ; l'arbre lui-même en ajoute
        // ceil(log2(6549)) = 13. Une chaîne plate en aurait ajouté 6 549.
        assert!(
            profondeur(&sql) < 40,
            "profondeur {} : le OU n'est plus équilibré, SQLite refusera au-delà de 1 000",
            profondeur(&sql)
        );
        // Tous les marqueurs sont bien là, une fois chacun.
        assert_eq!(sql.matches("LIKE").count(), 6549);
        assert_eq!(ph.next_index(), 6550);

        // Rétrocompatibilité stricte : à une et deux valeurs, mot pour mot le
        // SQL d'avant.
        let mut pg = Placeholders::new(Engine::Postgres);
        assert_eq!(
            pg.or_like_ci("t.label", 1).unwrap(),
            "LOWER(t.label) LIKE LOWER($1)"
        );
        assert_eq!(
            pg.or_like_ci("t.label", 2).unwrap(),
            "(LOWER(t.label) LIKE LOWER($2) OR LOWER(t.label) LIKE LOWER($3))"
        );
        // Trois valeurs : l'arbre penche à gauche, l'ORDRE des marqueurs suit
        // toujours 1, 2, 3 — c'est la seule chose que SQLite regarde.
        assert_eq!(
            pg.or_like_ci("t.label", 3).unwrap(),
            "((LOWER(t.label) LIKE LOWER($4) OR LOWER(t.label) LIKE LOWER($5)) \
             OR LOWER(t.label) LIKE LOWER($6))"
        );
        assert_eq!(pg.next_index(), 7);
    }

    /// Le piège n°1 : une facette sans valeur ne produit RIEN. Pas de `IN ()`
    /// (erreur SQL), pas de `1 = 1` (bibliothèque entière rendue en silence).
    #[test]
    fn une_liste_vide_ne_produit_aucun_predicat() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            let mut ph = Placeholders::new(engine);
            assert!(ph.in_list("t.format", 0).is_none());
            assert!(ph.in_list_ci("t.format", 0).is_none());
            assert!(ph.or_like_ci("t.label", 0).is_none());
            assert_eq!(
                ph.next_index(),
                1,
                "une liste vide ne doit consommer aucun marqueur"
            );
        }
        assert!(any_of(Vec::new()).is_none());
    }

    /// Le OU d'une facette est parenthésé : sinon `a OU b ET c` se lit
    /// `a OU (b ET c)` et la facette suivante cesse de restreindre.
    #[test]
    fn le_ou_interne_est_parenthese() {
        assert_eq!(
            any_of(vec!["x = 1".into(), "y = 2".into()]).unwrap(),
            "(x = 1 OR y = 2)"
        );
        // Une seule valeur : pas de parenthèses inutiles, le SQL reste celui
        // d'avant #2168.
        assert_eq!(any_of(vec!["x = 1".into()]).unwrap(), "x = 1");
    }

    /// Le piège n°1 vu de la ROUTE : un filtre qui ne produira aucun prédicat
    /// ne doit pas faire croire à un filtre actif. Sinon `?format=` emprunte le
    /// chemin filtré, n'y trouve rien à filtrer, et rend toute la bibliothèque
    /// alors que l'interface annonce un filtre.
    #[test]
    fn is_active_suit_exactement_ce_qui_produira_un_predicat() {
        assert!(!TrackFilter::default().is_active());

        // Une valeur vide a déjà été écartée par `normalize` : la facette est
        // alors inactive, et le chemin non filtré rend la même chose.
        let vide = TrackFilter {
            formats: normalize(&["".to_string(), "  ".to_string()]),
            ..Default::default()
        };
        assert!(!vide.is_active());

        // Vocabulaire FERMÉ : une valeur inconnue ne produit aucun prédicat,
        // donc elle ne doit pas non plus activer le chemin filtré.
        let inconnu = TrackFilter {
            untagged: vec!["mbid".to_string()],
            favorites: vec!["playlist".to_string()],
            ..Default::default()
        };
        assert!(!inconnu.is_active());

        let connu = TrackFilter {
            untagged: vec!["cover".to_string()],
            ..Default::default()
        };
        assert!(connu.is_active());

        let multi = TrackFilter {
            formats: vec!["aiff".to_string(), "flac".to_string()],
            ..Default::default()
        };
        assert!(multi.is_active());
    }

    /// Les deux vocabulaires fermés vivent ICI, une seule fois : c'est le seul
    /// moyen que la facette compte comme la liste filtre.
    #[test]
    fn les_vocabulaires_fermes_refusent_tout_le_reste() {
        for k in ["track", "album"] {
            assert!(favorite_condition(k).is_some(), "{k}");
        }
        for f in ["artist", "album", "genre", "year", "cover"] {
            assert!(untagged_condition(f).is_some(), "{f}");
        }
        for hostile in ["", "id", "t.genre", "1=1", "genre; DROP TABLE tracks--"] {
            assert!(untagged_condition(hostile).is_none(), "{hostile:?}");
            assert!(favorite_condition(hostile).is_none(), "{hostile:?}");
        }
    }

    /// #1391 — les deux prédicats de masquage sont des littéraux SANS
    /// marqueur (ils n'avancent aucun compteur) et visent chacun l'alias de
    /// SA famille de requêtes : `a` pour les albums, `t` pour les pistes. Une
    /// piste sans album reste visible par construction (`NOT EXISTS` sur un
    /// `album_id` NULL est vrai).
    #[test]
    fn les_predicats_de_masquage_visent_le_bon_alias() {
        let a = hidden_albums_excluded();
        assert!(a.contains("h.item_id = a.id"), "{a}");
        let t = hidden_tracks_excluded();
        assert!(t.contains("h.item_id = t.album_id"), "{t}");
        for cond in [a, t] {
            assert!(cond.starts_with("NOT EXISTS"), "{cond}");
            assert!(cond.contains("h.item_type = 'album'"), "{cond}");
            assert!(
                !cond.contains('?') && !cond.contains('$'),
                "littéral sans marqueur : {cond}"
            );
        }
    }

    /// Le piège n°2, éprouvé sur la facette la plus jeune (#2144).
    ///
    /// En SQLite tous les marqueurs s'écrivent `?` et seul l'ORDRE compte : un
    /// `IN (…)` qui oublie d'avancer le compteur y passe inaperçu et se
    /// trompe de valeurs en PostgreSQL. On l'éprouve donc en PostgreSQL, le
    /// seul moteur où l'indice se VOIT.
    #[test]
    fn la_facette_dr_numerote_ses_marqueurs_en_postgresql() {
        // Le compteur est déjà entamé, comme dans le vrai WHERE où trois
        // facettes précèdent le DR.
        let mut ph = Placeholders::resuming_at(Engine::Postgres, 4);
        let having = ph.in_list(DR_ALBUM_VALUE, 3).expect("liste non vide");
        let sql = dr_album_in(Engine::Postgres, &having);
        assert!(
            sql.contains(&format!("HAVING {DR_ALBUM_VALUE} IN ($4, $5, $6)")),
            "les trois marqueurs se suivent : {sql}"
        );
        assert_eq!(
            ph.next_index(),
            7,
            "trois marqueurs consommés, pas un de plus"
        );
        // Chaque moteur garde son dialecte de « que des chiffres ».
        assert!(sql.contains("tm.value ~ '^[0-9]+$'"), "{sql}");
        let sqlite = dr_album_in(Engine::Sqlite, "1 = 1");
        assert!(sqlite.contains("NOT GLOB"), "{sqlite}");
        assert!(!sqlite.contains('~'), "{sqlite}");
        // L'alias interne n'est JAMAIS `t` : c'est celui de la requête
        // englobante, et le masquer rendrait le prédicat toujours vrai.
        assert!(!sql.contains("tracks t "), "{sql}");
        assert!(sql.contains("tracks tdr"), "{sql}");
    }

    /// La règle de lecture du tag n'est écrite QU'UNE fois : la table dérivée
    /// de la grille d'albums et le prédicat des pistes la partagent.
    #[test]
    fn la_grille_et_le_rail_lisent_le_dr_de_la_meme_facon() {
        for engine in [Engine::Sqlite, Engine::Postgres] {
            let regle = dr_tag_where(engine);
            assert!(dr_album_source(engine).contains(&regle));
            assert!(dr_album_in(engine, "1 = 1").contains(&regle));
            // Les trois gardes AVANT le CAST, sur les deux moteurs.
            assert!(regle.contains("tm.key IN ('dr_album', 'dr_track')"));
            assert!(regle.contains("tm.value <> ''"));
            assert!(regle.contains("LENGTH(tm.value) <= 3"));
        }
    }

    /// Une facette DR sans valeur ne produit AUCUN prédicat et n'active pas le
    /// chemin filtré — le piège n°1, appliqué au dernier arrivé.
    #[test]
    fn une_facette_dr_vide_nactive_rien() {
        let mut ph = Placeholders::new(Engine::Sqlite);
        assert!(ph.in_list(DR_ALBUM_VALUE, 0).is_none());
        let vide = TrackFilter {
            dynamic_ranges: normalize_ints(&[]),
            ..Default::default()
        };
        assert!(!vide.is_active());
        let actif = TrackFilter {
            dynamic_ranges: vec![14],
            ..Default::default()
        };
        assert!(actif.is_active());
    }

    #[test]
    fn normalize_ecarte_les_vides_et_les_doublons() {
        let v = vec![
            "flac".to_string(),
            "".to_string(),
            "  ".to_string(),
            "aiff".to_string(),
            "flac".to_string(),
        ];
        assert_eq!(normalize(&v), vec!["flac".to_string(), "aiff".to_string()]);
        assert!(normalize(&[]).is_empty());
        assert_eq!(normalize_ints(&[44100, 96000, 44100]), vec![44100, 96000]);
    }
}
