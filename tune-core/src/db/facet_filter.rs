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
//! # Deux pièges, tous deux déjà rencontrés dans ce dépôt
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

use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

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
    pub fn or_like_ci(&mut self, expr: &str, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        if n == 1 {
            return Some(format!("LOWER({expr}) LIKE LOWER({})", self.take()));
        }
        let parts = (0..n)
            .map(|_| format!("LOWER({expr}) LIKE LOWER({})", self.take()))
            .collect::<Vec<_>>()
            .join(" OR ");
        Some(format!("({parts})"))
    }
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

/// Prédicat SQL d'une étiquette manquante. Liste FERMÉE : toute autre valeur
/// rend `None` et ne filtre rien, plutôt que d'injecter quoi que ce soit.
///
/// « Manquant » vaut ici NULL **ou** chaîne vide : un tag effacé par un éditeur
/// laisse souvent une chaîne vide, et l'utilisateur qui range sa bibliothèque
/// ne fait pas la différence entre les deux.
pub fn untagged_condition(field: &str) -> Option<&'static str> {
    match field {
        "genre" => Some("(t.genre IS NULL OR t.genre = '')"),
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
    pub artists: Vec<String>,
    pub countries: Vec<String>,
    pub moods: Vec<String>,
    pub source_medias: Vec<String>,
    pub ratings: Vec<i64>,
    pub original_years: Vec<i64>,
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
            || !self.artists.is_empty()
            || !self.countries.is_empty()
            || !self.moods.is_empty()
            || !self.source_medias.is_empty()
            || !self.ratings.is_empty()
            || !self.original_years.is_empty()
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
