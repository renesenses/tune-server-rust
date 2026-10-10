//! LA liste des critères d'une règle intelligente — collections ET playlists.
//!
//! ## Pourquoi ce module existe — #5547
//!
//! Bertrand, 30/09/2026, sur le .18 en 0.9.169 : « l'éditeur des playlists
//! intelligentes n'offre pas les mêmes critères de sélection que celui des
//! collections intelligentes. Il manque notamment les ÉTIQUETTES. Harmonise
//! avec les smart collections ! »
//!
//! Les deux moteurs — `smart_collections::build_album_query` (albums) et
//! `smart_playlists::build_smart_query` (pistes) — avaient chacun leur liste
//! implicite, écrite dans un `match`. Rien ne les obligeait à se ressembler,
//! et elles ne se ressemblaient pas : `added_at`, `last_played_at`, `credit`,
//! `track_count`, `cover_path` et les opérateurs `in` / `between` rendaient
//! FAUX côté playlists ; « entre » sur les écoutes et la dernière écoute
//! disparaissait en silence côté collections.
//!
//! Ce tableau est la définition UNIQUE. Il ne traduit rien lui-même — chaque
//! moteur garde sa traduction, parce qu'un critère ne se dit pas pareil d'un
//! album et d'une piste — mais les gardes ci-dessous jouent CHAQUE critère
//! avec CHAQUE opérateur dans les DEUX moteurs, sur une vraie base, et
//! échouent dès qu'un seul ne le traduit pas. C'est ce qui empêche les deux
//! listes de diverger à nouveau.
//!
//! Le client web porte le même tableau (`src/lib/smartRegles.ts`, `CHAMPS`) :
//! mêmes noms, mêmes familles, mêmes opérateurs. Sa propre garde
//! (`criteresPartages5547.test.ts`) vérifie que ses deux éditeurs le lisent.

// Le tableau est la RÉFÉRENCE contre laquelle les deux moteurs sont joués ;
// hors des tests, aucun code ne le lit.
#![cfg_attr(not(test), allow(dead_code))]

/// La famille d'un critère : elle décide des opérateurs qu'on lui propose.
///
/// Mêmes noms de familles que le client (`TypeChamp` de `smartRegles.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Famille {
    Texte,
    Nombre,
    Source,
    Repertoire,
    Vide,
    Horodatage,
    Compte,
    Credit,
    RefCollection,
    RefPlaylist,
    Favori,
    Etiquette,
    /// Un marquage du SERVICE, oui ou non, sans valeur à saisir — #5530,
    /// « généré par IA » (Qobuz).
    Marquage,
}

impl Famille {
    /// Les opérateurs de la famille, dans les graphies de l'éditeur.
    pub(crate) const fn operateurs(self) -> &'static [&'static str] {
        match self {
            Famille::Nombre => &["=", "!=", ">=", ">", "<=", "<", "between"],
            Famille::Texte => &[
                "=",
                "!=",
                "contains",
                "starts_with",
                "in",
                "is_null",
                "is_not_null",
            ],
            Famille::Source => &["=", "!="],
            Famille::Repertoire => &["starts_with", "contains"],
            Famille::Vide => &["is_null", "is_not_null"],
            Famille::Horodatage => &[">", "<", "between", "is_null"],
            Famille::Credit => &["has"],
            Famille::RefCollection | Famille::RefPlaylist => &["in", "not_in"],
            Famille::Favori | Famille::Etiquette => &["is", "is_not"],
            Famille::Compte => &[">=", ">", "<", "=", "between"],
            // « non » d'abord : c'est la règle demandée (« pas d'IA »), et
            // l'éditeur part sur le premier opérateur.
            Famille::Marquage => &["is_false", "is_true"],
        }
    }
}

/// Un critère, et le nom qu'il porte à chaque niveau.
///
/// `None` : le critère n'existe pas à ce niveau. Deux critères seulement sont
/// dans ce cas, et tous deux propres aux pistes (voir [`PROPRES_AUX_PISTES`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Critere {
    /// Le nom dans une règle de COLLECTION (albums).
    pub collection: Option<&'static str>,
    /// Le nom dans une règle de PLAYLIST (pistes).
    pub piste: Option<&'static str>,
    pub famille: Famille,
}

const fn deux(nom: &'static str, famille: Famille) -> Critere {
    Critere {
        collection: Some(nom),
        piste: Some(nom),
        famille,
    }
}

/// Les critères, dans l'ordre du client.
pub(crate) const CRITERES: &[Critere] = &[
    // Deux noms diffèrent d'un niveau à l'autre, et ce sont les noms que les
    // règles enregistrées portent déjà : `artist` dans les playlists, et
    // `album` pour le titre de l'album — dans une playlist, `title` est le
    // titre de la PISTE. Les deux moteurs lisent aussi l'autre graphie.
    Critere {
        collection: Some("artist_name"),
        piste: Some("artist"),
        famille: Famille::Texte,
    },
    Critere {
        collection: Some("title"),
        piste: Some("album"),
        famille: Famille::Texte,
    },
    deux("genre", Famille::Texte),
    deux("composer", Famille::Texte),
    deux("label", Famille::Texte),
    deux("format", Famille::Texte),
    deux("source", Famille::Source),
    deux("folder", Famille::Repertoire),
    deux("year", Famille::Nombre),
    deux("sample_rate", Famille::Nombre),
    deux("bit_depth", Famille::Nombre),
    deux("track_count", Famille::Nombre),
    deux("duration", Famille::Nombre),
    deux("track_number", Famille::Nombre),
    deux("disc_number", Famille::Nombre),
    deux("bpm", Famille::Nombre),
    deux("rating", Famille::Nombre),
    deux("cover_path", Famille::Vide),
    deux("added_at", Famille::Horodatage),
    deux("credit", Famille::Credit),
    deux("play_count", Famille::Compte),
    deux("last_played_at", Famille::Horodatage),
    deux("in_collection", Famille::RefCollection),
    deux("in_playlist", Famille::RefPlaylist),
    deux("favorite", Famille::Favori),
    deux("tag", Famille::Etiquette),
    // #5530 — « Généré par IA » : le marquage que Qobuz pose sur un ALBUM
    // (`album/get` → `ai_generated`). Dans une playlist, celui de l'album de
    // la piste. Seuls les contenus de SERVICE le portent : une piste de la
    // bibliothèque n'est jamais marquée.
    deux("ai_generated", Famille::Marquage),
    // --- Propres aux PISTES ---
    Critere {
        collection: None,
        piste: Some("title"),
        famille: Famille::Texte,
    },
    Critere {
        collection: None,
        piste: Some("comments"),
        famille: Famille::Texte,
    },
];

/// Les SEULS critères qui n'existent qu'à un niveau. Les nommer ici est une
/// décision, qui se lit dans la revue.
pub(crate) const PROPRES_AUX_PISTES: &[&str] = &["title", "comments"];

// « Note » (`rating`) a été la seule entrée d'une liste `CASSES` : les deux
// moteurs compilaient `t.rating`, colonne absente, et l'aperçu rendait une
// erreur 500. Réparée par décision de Bertrand (30/09/2026, #5547) : la note
// de l'ALBUM pour le profil actif (`regles_sql::condition_note`). La liste a
// disparu avec son dernier cas ; la garde joue désormais TOUS les critères.

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::smart_collections::build_album_query;
    use crate::smart_playlists::build_smart_query_rapport;
    use crate::smart_refs::{EmptyResolver, RefCtx};
    use serde_json::{Value, json};
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;

    /// Une valeur plausible pour chaque famille et chaque opérateur.
    fn valeur(f: Famille, op: &str) -> Value {
        match (f, op) {
            (_, "is_null" | "is_not_null") => Value::Null,
            (Famille::Horodatage, "between") => json!(["2020-01-01", "2030-01-01"]),
            (_, "between") => json!([1, 5]),
            (Famille::Horodatage, _) => json!("now-30d"),
            (Famille::Nombre | Famille::Compte, _) => json!(3),
            (Famille::Texte, "in") => json!("Jazz, Rock"),
            (Famille::Source, _) => json!("local"),
            (Famille::Repertoire, _) => json!("/m"),
            (Famille::Credit, _) => json!({"role": "producer"}),
            (Famille::RefCollection | Famille::RefPlaylist, _) => json!("classic:1"),
            (Famille::Favori, _) => json!("track"),
            (Famille::Etiquette, _) => json!("7"),
            (Famille::Marquage, _) => Value::Null,
            (Famille::Texte | Famille::Vide, _) => json!("x"),
        }
    }

    /// Une base migrée, et une petite bibliothèque où chaque critère a
    /// quelque chose à retenir ET quelque chose à écarter.
    ///
    /// | piste | album (pistes, label, pochette) | artiste | particularités |
    /// |---|---|---|---|
    /// | 1 `Blue in Green` | 1 Kind of Blue (2, Columbia, oui) | 1 Miles | étiquetée 7 ; crédit producer Teo Macero ; 3 écoutes, dernière 2024-06 ; fichier 2024-01 |
    /// | 2 `So What` | 1 | 1 | 1 écoute, 2019-03 |
    /// | 3 `Giant Steps` | 2 Giant Steps (1, Atlantic, non) | 2 Coltrane | album étiqueté 8 ; fichier 2019-01 |
    /// | 4 `Solo` | 3 Seul (1, —, non) | 3 Autre | artiste étiqueté 9 ; genre Rock |
    pub(crate) fn bibliotheque() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1,'Miles Davis'),(2,'John Coltrane'),(3,'Autre'); \
             INSERT INTO albums (id, title, artist_id, track_count, label, cover_path) VALUES \
               (1,'Kind of Blue',1,2,'Columbia','/c/1.jpg'), \
               (2,'Giant Steps',2,1,'Atlantic',NULL), \
               (3,'Seul',3,1,NULL,NULL); \
             INSERT INTO tracks (id, album_id, artist_id, title, genre, year, file_path, file_mtime, label) VALUES \
               (1,1,1,'Blue in Green','Jazz',1959,'/m/jazz/1.flac',1704067200,'Columbia'), \
               (2,1,1,'So What','Jazz',1959,'/m/jazz/2.flac',1704067200,'Columbia'), \
               (3,2,2,'Giant Steps','Jazz',1960,'/m/jazz/3.flac',1546300800,'Atlantic'), \
               (4,3,3,'Solo','Rock',2001,'/m/rock/4.flac',1546300800,NULL); \
             INSERT INTO tags (id, name) VALUES (7,'J''adore'),(8,'Album'),(9,'Artiste'); \
             INSERT INTO item_tags (tag_id, item_type, item_id) VALUES \
               (7,'track',1),(8,'album',2),(9,'artist',3); \
             INSERT INTO album_ratings (album_id, profile_id, rating) VALUES \
               (1,1,5),(2,1,2),(2,2,5); \
             INSERT INTO track_credits (track_id, artist_name, role) VALUES (1,'Teo Macero','producer'); \
             INSERT INTO listen_history (track_id, title, listened_at) VALUES \
               (1,'Blue in Green','2024-06-01T10:00:00Z'), \
               (1,'Blue in Green','2024-06-02T10:00:00Z'), \
               (1,'Blue in Green','2024-06-03T10:00:00Z'), \
               (2,'So What','2019-03-01T10:00:00Z');",
        )
        .unwrap();
        db
    }

    /// Les identifiants des pistes que retient une règle de PLAYLIST.
    pub(crate) fn pistes(db: &SqliteDb, regles: &str) -> Vec<i64> {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _o, _l, refusees) =
            build_smart_query_rapport(regles, "all", "title", "asc", None, &ctx);
        assert!(refusees.is_empty(), "règle refusée {refusees:?} : {regles}");
        let sql = format!(
            "SELECT t.id FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             {w} ORDER BY t.id"
        );
        db.query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect()
    }

    /// Les identifiants des albums que retient une règle de COLLECTION.
    fn albums(db: &SqliteDb, regles: &str) -> Vec<i64> {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _o, _l) = build_album_query(regles, "all", "title", "asc", None, &ctx);
        assert!(
            !w.is_empty(),
            "règle abandonnée par les collections : {regles}"
        );
        let sql = format!(
            "SELECT al.id FROM albums al \
             LEFT JOIN artists ar ON al.artist_id = ar.id \
             LEFT JOIN tracks t ON t.album_id = al.id \
             {w} GROUP BY al.id ORDER BY al.id"
        );
        db.query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect()
    }

    fn regle(champ: &str, op: &str, v: Value) -> String {
        json!([{"field": champ, "op": op, "value": v}]).to_string()
    }

    /// 🔴 LA GARDE — chaque critère, avec chaque opérateur de sa famille, est
    /// traduit par les DEUX moteurs, et la requête s'exécute.
    ///
    /// Un critère qu'un moteur ne sait pas traduire échoue ici : côté
    /// playlists il arrive dans le rapport des refusées, côté collections la
    /// clause est vide (la règle a disparu). Avant #5547, trente-deux
    /// combinaisons échouaient — dont l'étiquette dans l'éditeur, et `added_at`,
    /// `last_played_at`, `credit`, `track_count`, `cover_path`, `in`, `between`
    /// dans le moteur des playlists.
    #[test]
    fn chaque_critere_est_traduit_par_les_deux_moteurs() {
        let db = bibliotheque();
        let mut essais = 0;
        for c in CRITERES {
            for op in c.famille.operateurs() {
                let v = valeur(c.famille, op);
                if let Some(nom) = c.piste {
                    pistes(&db, &regle(nom, op, v.clone()));
                    essais += 1;
                }
                if let Some(nom) = c.collection {
                    albums(&db, &regle(nom, op, v.clone()));
                    essais += 1;
                }
            }
        }
        assert!(essais > 200, "la garde ne joue presque rien : {essais}");
    }

    /// Ce test vérifiait que la « Note » était cassée des DEUX côtés (`no such
    /// column: t.rating`) et devait échouer le jour de sa réparation. Adapté,
    /// et non supprimé (#5547) : la même requête, dans les deux moteurs, doit
    /// maintenant s'exécuter ET rendre la note de l'album pour le profil actif.
    ///
    /// Notes de la bibliothèque : Kind of Blue 5 (profil 1) ; Giant Steps 2
    /// (profil 1) et 5 (profil 2) ; Seul, aucune.
    #[test]
    fn la_note_de_l_album_marche_des_deux_cotes() {
        let db = bibliotheque();
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let r = regle("rating", ">=", json!(3));
        let (w, _o, _l, refusees) =
            build_smart_query_rapport(&r, "all", "title", "asc", None, &ctx);
        assert!(refusees.is_empty(), "{refusees:?}");
        let sql = format!(
            "SELECT t.id FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id {w} ORDER BY t.id"
        );
        let lignes = db
            .query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("la note des playlists est cassée : {e}\n{sql}"));
        let ids: Vec<i64> = lignes.iter().map(|l| l[0].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1, 2], "les pistes de l'album noté 5");
        let (w, _o, _l) = build_album_query(&r, "all", "title", "asc", None, &ctx);
        let sql = format!(
            "SELECT al.id FROM albums al LEFT JOIN artists ar ON al.artist_id = ar.id \
             LEFT JOIN tracks t ON t.album_id = al.id {w} GROUP BY al.id ORDER BY al.id"
        );
        let lignes = db
            .query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("la note des collections est cassée : {e}\n{sql}"));
        let ids: Vec<i64> = lignes.iter().map(|l| l[0].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1], "l'album noté 5");
    }

    /// Les pistes d'une règle de playlist, pour un PROFIL donné.
    fn pistes_du_profil(db: &SqliteDb, regles: &str, profil: i64) -> Vec<i64> {
        let ctx = RefCtx::root(&EmptyResolver, Some(profil));
        let (w, _o, _l, _) = build_smart_query_rapport(regles, "all", "title", "asc", None, &ctx);
        let sql = format!(
            "SELECT t.id FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id {w} ORDER BY t.id"
        );
        db.query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect()
    }

    #[test]
    fn la_note_est_celle_du_profil_actif() {
        let db = bibliotheque();
        let r = regle("rating", ">=", json!(4));
        assert_eq!(pistes_du_profil(&db, &r, 1), vec![1, 2]);
        // Le profil 2 n'a noté que Giant Steps, 5.
        assert_eq!(pistes_du_profil(&db, &r, 2), vec![3]);
        // Un profil sans note ne retient rien.
        assert!(pistes_du_profil(&db, &r, 9).is_empty());
    }

    #[test]
    fn un_album_sans_note_ne_passe_aucune_comparaison() {
        // « Seul » (piste 4) n'a pas de note : ni « ≥ », ni « < », ni « ≠ »,
        // ni « entre ». La famille numérique n'a pas de « n'est pas noté ».
        let db = bibliotheque();
        assert_eq!(pistes(&db, &regle("rating", "<", json!(3))), vec![3]);
        assert_eq!(pistes(&db, &regle("rating", "!=", json!(5))), vec![3]);
        assert_eq!(
            pistes(&db, &regle("rating", "between", json!([1, 5]))),
            vec![1, 2, 3]
        );
        assert_eq!(pistes(&db, &regle("rating", "=", json!("5"))), vec![1, 2]);
        assert_eq!(albums(&db, &regle("rating", "<", json!(3))), vec![2]);
        assert_eq!(
            albums(&db, &regle("rating", "between", json!([1, 5]))),
            vec![1, 2]
        );
        assert!(!Famille::Nombre.operateurs().contains(&"is_null"));
    }

    #[test]
    fn sans_profil_la_note_ne_retient_rien() {
        let db = bibliotheque();
        let ctx = RefCtx::root(&EmptyResolver, None);
        let r = regle("rating", ">=", json!(1));
        let (w, _o, _l, _) = build_smart_query_rapport(&r, "all", "title", "asc", None, &ctx);
        assert!(w.contains("1 = 0"), "{w}");
        let (w, _o, _l) = build_album_query(&r, "all", "title", "asc", None, &ctx);
        assert!(w.contains("1 = 0"), "{w}");
        let _ = db;
    }

    #[test]
    fn seuls_les_criteres_nommes_sont_propres_a_un_niveau() {
        let propres: Vec<&str> = CRITERES
            .iter()
            .filter(|c| c.collection.is_none())
            .filter_map(|c| c.piste)
            .collect();
        assert_eq!(propres, PROPRES_AUX_PISTES);
        assert!(
            CRITERES.iter().all(|c| c.piste.is_some()),
            "un critère de collection sans équivalent de piste : c'est le défaut de #5547"
        );
    }

    /// Le compte du client : vingt-neuf définitions, dont vingt-sept aux
    /// collections (#5530 : « Généré par IA »). Si ce nombre bouge,
    /// `smartRegles.ts` doit bouger aussi.
    #[test]
    fn le_tableau_a_la_taille_de_celui_du_client() {
        assert_eq!(CRITERES.len(), 29);
        assert_eq!(
            CRITERES.iter().filter(|c| c.collection.is_some()).count(),
            27
        );
    }

    // ---------------------------------------------------------------------
    // Chaque critère AJOUTÉ aux playlists filtre vraiment (#5547).
    // ---------------------------------------------------------------------

    #[test]
    fn l_etiquette_retient_la_piste_son_album_ou_son_artiste() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("tag", "is", json!("7"))),
            vec![1],
            "piste"
        );
        assert_eq!(
            pistes(&db, &regle("tag", "is", json!("8"))),
            vec![3],
            "album"
        );
        assert_eq!(
            pistes(&db, &regle("tag", "is", json!("9"))),
            vec![4],
            "artiste"
        );
        assert_eq!(
            pistes(&db, &regle("tag", "is_not", json!("7"))),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn le_label_et_le_repertoire() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("label", "=", json!("columbia"))),
            vec![1, 2]
        );
        assert_eq!(
            pistes(&db, &regle("folder", "starts_with", json!("/m/rock"))),
            vec![4]
        );
    }

    #[test]
    fn le_nombre_de_pistes_et_la_pochette_de_l_album() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("track_count", ">=", json!(2))),
            vec![1, 2]
        );
        assert_eq!(
            pistes(&db, &regle("cover_path", "is_null", Value::Null)),
            vec![3, 4]
        );
        assert_eq!(
            pistes(&db, &regle("cover_path", "is_not_null", Value::Null)),
            vec![1, 2]
        );
    }

    #[test]
    fn la_date_d_ajout_est_celle_du_fichier() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("added_at", ">", json!("2023-01-01"))),
            vec![1, 2]
        );
        assert_eq!(
            pistes(&db, &regle("added_at", "<", json!("2023-01-01"))),
            vec![3, 4]
        );
        assert_eq!(
            pistes(
                &db,
                &regle("added_at", "between", json!(["2018-06-01", "2019-06-01"]))
            ),
            vec![3, 4]
        );
    }

    #[test]
    fn la_derniere_ecoute() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("last_played_at", ">", json!("2024-01-01"))),
            vec![1]
        );
        assert_eq!(
            pistes(&db, &regle("last_played_at", "<", json!("2024-01-01"))),
            vec![2]
        );
        assert_eq!(
            pistes(
                &db,
                &regle(
                    "last_played_at",
                    "between",
                    json!(["2019-01-01", "2019-12-31"])
                )
            ),
            vec![2]
        );
        assert_eq!(
            pistes(&db, &regle("last_played_at", "is_null", Value::Null)),
            vec![3, 4]
        );
    }

    #[test]
    fn le_credit_de_la_piste() {
        let db = bibliotheque();
        assert_eq!(
            pistes(
                &db,
                &regle(
                    "credit",
                    "has",
                    json!({"role": "producer", "artist_name": "macero"})
                )
            ),
            vec![1]
        );
        assert!(pistes(&db, &regle("credit", "has", json!({"role": "engineer"}))).is_empty());
    }

    #[test]
    fn entre_et_parmi() {
        let db = bibliotheque();
        assert_eq!(
            pistes(&db, &regle("year", "between", json!([1959, 1960]))),
            vec![1, 2, 3]
        );
        assert_eq!(
            pistes(&db, &regle("year", "between", json!("1960,2001"))),
            vec![3, 4]
        );
        // Insensible à la casse, des deux côtés.
        assert_eq!(
            pistes(&db, &regle("genre", "in", json!("rock, Blues"))),
            vec![4]
        );
        assert_eq!(
            pistes(&db, &regle("genre", "in", json!(["JAZZ"]))),
            vec![1, 2, 3]
        );
        assert_eq!(
            pistes(&db, &regle("play_count", "between", json!([1, 2]))),
            vec![2]
        );
        // « entre 0 et N » garde les pistes jamais écoutées.
        assert_eq!(
            pistes(&db, &regle("play_count", "between", json!([0, 1]))),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn un_nombre_json_se_lit_comme_ses_chiffres() {
        // L'éditeur envoie `{"value": 1960}` ; le moteur des playlists n'en
        // lisait que les chaînes, et `1960` y valait `""`, donc `year = 0`.
        let db = bibliotheque();
        assert_eq!(pistes(&db, &regle("year", "=", json!(1960))), vec![3]);
        assert_eq!(pistes(&db, &regle("year", "=", json!("1960"))), vec![3]);
    }

    // ---------------------------------------------------------------------
    // Côté COLLECTIONS : les deux « entre » que le moteur abandonnait.
    // ---------------------------------------------------------------------

    #[test]
    fn les_collections_savent_enfin_dire_entre_pour_les_ecoutes() {
        let db = bibliotheque();
        // Kind of Blue : 4 écoutes. Les deux autres : aucune.
        assert_eq!(
            albums(&db, &regle("play_count", "between", json!([3, 5]))),
            vec![1]
        );
        assert_eq!(
            albums(&db, &regle("play_count", "between", json!([0, 1]))),
            vec![2, 3]
        );
        assert_eq!(
            albums(
                &db,
                &regle(
                    "last_played_at",
                    "between",
                    json!(["2024-01-01", "2024-12-31"])
                )
            ),
            vec![1]
        );
    }
}

/// 🔴 Les règles DÉJÀ ENREGISTRÉES rendent exactement la même requête qu'avant
/// #5547.
///
/// `testdata/regles_anciennes_5547.tsv` a été produit par
/// `build_smart_query_rapport` sur la base du lot (`9f46bcc84`, origin/main
/// du 30/09/2026), AVANT toute modification : seize jeux de règles — les six
/// playlists du .18 et les formes que l'ancien éditeur écrivait (`equals`,
/// `gte`, `operator`, `is_empty`, `branch_of`…). Chaque colonne — clause,
/// tri, borne, refusées — doit ressortir au caractère près.
#[cfg(test)]
mod anciennes_regles {
    use crate::smart_playlists::build_smart_query_rapport;
    use crate::smart_refs::{EmptyResolver, RefCtx};

    #[test]
    fn les_regles_anciennes_rendent_la_meme_requete_qu_avant() {
        let gele = include_str!("../testdata/regles_anciennes_5547.tsv");
        let mut vus = 0;
        for ligne in gele.lines().filter(|l| !l.trim().is_empty()) {
            let c: Vec<&str> = ligne.split('\t').collect();
            assert_eq!(c.len(), 6, "ligne gelée mal formée : {ligne}");
            let ctx = RefCtx::root(&EmptyResolver, Some(1));
            let (w, o, l, refusees) =
                build_smart_query_rapport(c[0], c[1], "title", "asc", Some(50), &ctx);
            assert_eq!(w, c[2], "clause changée pour {}", c[0]);
            assert_eq!(o, c[3], "tri changé pour {}", c[0]);
            assert_eq!(l, c[4], "borne changée pour {}", c[0]);
            assert_eq!(format!("{refusees:?}"), c[5], "refus changés pour {}", c[0]);
            vus += 1;
        }
        assert_eq!(vus, 16, "le gel ne porte plus ses seize jeux de règles");
    }
}
