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
    /// #6285 — un verdict hi-res choisi dans une liste fermée : `verifie`,
    /// `conforme`, `douteux`, `non_analyse`.
    VerdictHiRes,
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
                // #6250 — « ne contient pas » : aucun moteur ne le traduisait.
                "not_contains",
                "in",
                "is_null",
                "is_not_null",
            ],
            Famille::Source => &["=", "!="],
            Famille::Repertoire => &["starts_with", "contains"],
            Famille::Vide => &["is_null", "is_not_null"],
            // #6122 — « dans les N derniers jours » (`in_last`), la durée
            // glissante ; « après » garde sa date fixe.
            Famille::Horodatage => &[">", "<", "between", "in_last", "is_null"],
            Famille::Credit => &["has"],
            Famille::RefCollection | Famille::RefPlaylist => &["in", "not_in"],
            Famille::Favori | Famille::Etiquette => &["is", "is_not"],
            Famille::Compte => &[">=", ">", "<", "=", "between"],
            // « non » d'abord : c'est la règle demandée (« pas d'IA »), et
            // l'éditeur part sur le premier opérateur.
            Famille::Marquage => &["is_false", "is_true"],
            Famille::VerdictHiRes => &["=", "!="],
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
    // #6121 (forum 2218) — « Artiste de l'album » : la balise ALBUMARTIST,
    // et à défaut l'artiste de l'album. Même nom aux deux niveaux. Dans une
    // collection, « Artiste » lit déjà l'artiste de l'album ; dans une
    // playlist, il lit l'interprète de la PISTE — c'est là que les deux
    // critères diffèrent.
    deux("album_artist", Famille::Texte),
    // #6232 (forum 2218) — « Interprète » : la balise ARTIST de la PISTE, à
    // défaut l'artiste de la piste en base. Dans une collection, un album
    // passe dès qu'une de ses pistes passe : c'est ce que « Artiste » (artiste
    // de l'album) ne savait pas faire.
    deux("track_artist", Famille::Texte),
    deux("genre", Famille::Texte),
    deux("composer", Famille::Texte),
    deux("label", Famille::Texte),
    deux("format", Famille::Texte),
    deux("source", Famille::Source),
    deux("folder", Famille::Repertoire),
    deux("year", Famille::Nombre),
    // #6124 — l'année de sortie ORIGINALE (`albums.original_year` : balises
    // ORIGINALDATE / ORIGINALRELEASEDATE / ORIGINALYEAR / TDOR, ou
    // MusicBrainz) et la VERSION de la piste (balise VERSION : original,
    // réédition, remaster…, `track_metadata['version']`).
    deux("original_year", Famille::Nombre),
    deux("version", Famille::Texte),
    deux("sample_rate", Famille::Nombre),
    deux("bit_depth", Famille::Nombre),
    deux("track_count", Famille::Nombre),
    deux("duration", Famille::Nombre),
    deux("track_number", Famille::Nombre),
    deux("disc_number", Famille::Nombre),
    deux("bpm", Famille::Nombre),
    // « Note de l'album » : la note de l'album pour le profil actif (#5547).
    // Le nom `rating` est celui des règles déjà enregistrées : il garde ce
    // sens-là (décision de Bertrand du 11/10, #6123).
    deux("rating", Famille::Nombre),
    // « Note de la piste » (#6123) : la note lue dans les balises du fichier
    // (`tracks.rating`, 0 à 5, sans profil). Dans une collection : un album
    // dont UNE piste au moins passe la comparaison.
    deux("track_rating", Famille::Nombre),
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
    // #6285 — « Verdict hi-res » : celui du badge de l'album dans une
    // collection, celui de la piste dans une playlist (`hires_verifications`).
    deux("hires_verdict", Famille::VerdictHiRes),
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

impl Famille {
    /// Le nom de la famille, dans les mots du client (`TypeChamp` de
    /// `smartRegles.ts`), pour un lecteur hors de cette caisse.
    pub(crate) const fn nom(self) -> &'static str {
        match self {
            Famille::Texte => "text",
            Famille::Nombre => "int",
            Famille::Source => "source",
            Famille::Repertoire => "folder",
            Famille::Vide => "nullable",
            Famille::Horodatage => "timestamp",
            Famille::Compte => "count",
            Famille::Credit => "credit",
            Famille::RefCollection => "collection_ref",
            Famille::RefPlaylist => "playlist_ref",
            Famille::Favori => "favorite",
            Famille::Etiquette => "tag_ref",
            Famille::Marquage => "marquage",
            Famille::VerdictHiRes => "hires_verdict",
        }
    }
}

/// Un critère tel qu'un lecteur EXTÉRIEUR le voit — le greffon « Assistant
/// IA » (chantier assistant-ia, étape 1) en tire le schéma qu'il impose au
/// modèle et la liste contre laquelle il valide sa réponse. Lire LE tableau
/// au lieu d'en recopier un : une règle que le moteur ne connaît pas ne peut
/// pas être proposée, et un critère ajouté ici l'est aussi là-bas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CritereExpose {
    /// Le nom dans une règle de COLLECTION, `None` s'il n'y existe pas.
    pub collection: Option<&'static str>,
    /// Le nom dans une règle de PLAYLIST, `None` s'il n'y existe pas.
    pub piste: Option<&'static str>,
    /// La famille, dans les mots du client (`text`, `int`, `timestamp`…).
    pub famille: &'static str,
    /// Les opérateurs que le moteur traduit pour cette famille.
    pub operateurs: &'static [&'static str],
}

/// Tous les critères, dans l'ordre du client.
pub fn criteres_exposes() -> Vec<CritereExpose> {
    CRITERES
        .iter()
        .map(|c| CritereExpose {
            collection: c.collection,
            piste: c.piste,
            famille: c.famille.nom(),
            operateurs: c.famille.operateurs(),
        })
        .collect()
}

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
            (Famille::Horodatage, "in_last") => json!(30),
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
            (Famille::VerdictHiRes, _) => json!("verifie"),
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

    /// #6271 — collections : « finit par » était ignoré (`_ => continue`), et
    /// un motif sur un NOMBRE passait par `LOWER(entier)`, que PostgreSQL
    /// refuse. Le nombre se lit désormais par sa forme textuelle.
    #[test]
    fn collections_finit_par_et_motifs_sur_un_nombre_6271() {
        let db = bibliotheque();
        // Années des pistes : 1959, 1959 (album 1), 1960 (2), 2001 (3).
        assert_eq!(
            albums(&db, &regle("year", "contains", json!("195"))),
            vec![1]
        );
        assert_eq!(
            albums(&db, &regle("year", "not_contains", json!("195"))),
            vec![2, 3]
        );
        assert_eq!(
            albums(&db, &regle("year", "starts_with", json!("19"))),
            vec![1, 2]
        );
        assert_eq!(
            albums(&db, &regle("year", "ends_with", json!("01"))),
            vec![3]
        );
        assert_eq!(
            albums(&db, &regle("title", "ends_with", json!("STEPS"))),
            vec![2]
        );
        assert_eq!(
            albums(&db, &regle("genre", "ends_with", json!("ock"))),
            vec![3]
        );
        // Les playlists, elles, le faisaient déjà : même réponse à la piste.
        assert_eq!(
            pistes(&db, &regle("title", "ends_with", json!("steps"))),
            vec![3]
        );
        // La forme SQL : aucun `LOWER()` autour d'une colonne numérique.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        for c in CRITERES.iter().filter(|c| c.famille == Famille::Nombre) {
            let Some(nom) = c.collection else { continue };
            if nom == "rating" {
                continue; // note d'album : sa propre famille d'opérateurs
            }
            for op in ["contains", "not_contains", "starts_with", "ends_with"] {
                let (w, _o, _l) = build_album_query(
                    &regle(nom, op, json!("1")),
                    "all",
                    "title",
                    "asc",
                    None,
                    &ctx,
                );
                assert!(w.contains("CAST("), "{nom} {op} : {w}");
                assert!(!w.contains("LOWER(CAST"), "{nom} {op} : {w}");
                albums(&db, &regle(nom, op, json!("1")));
            }
        }
    }

    /// #6250 — « ne contient pas » est le complément EXACT de « contient »,
    /// dans les deux moteurs et pour CHAQUE critère texte : les deux règles se
    /// partagent la bibliothèque, sans trou ni recouvrement. Une valeur absente
    /// tombe du côté « ne contient pas ».
    ///
    /// Avant : FAUX côté playlists (rien), règle ignorée côté collections
    /// (tout).
    #[test]
    fn ne_contient_pas_est_le_complement_de_contient_6250() {
        let db = bibliotheque();
        // Album 1 devient MIXTE (Jazz + Rock), la piste 3 perd son genre ; un
        // seul compositeur dans la bibliothèque.
        db.execute_batch(
            "UPDATE tracks SET genre = 'Rock' WHERE id = 2; \
             UPDATE tracks SET genre = NULL WHERE id = 3; \
             UPDATE tracks SET composer = 'Bill Evans' WHERE id = 1;",
        )
        .unwrap();
        let toutes_pistes = vec![1, 2, 3, 4];
        let tous_albums = vec![1, 2, 3];
        let mut essais = 0;
        for c in CRITERES.iter().filter(|c| c.famille == Famille::Texte) {
            for v in ["o", "rock", "Blue", "zz-absent"] {
                if let Some(nom) = c.piste {
                    let oui = pistes(&db, &regle(nom, "contains", json!(v)));
                    let non = pistes(&db, &regle(nom, "not_contains", json!(v)));
                    let mut union: Vec<i64> = oui.iter().chain(&non).copied().collect();
                    union.sort();
                    assert_eq!(union, toutes_pistes, "pistes {nom} {v} : {oui:?} / {non:?}");
                    essais += 1;
                }
                if let Some(nom) = c.collection {
                    let oui = albums(&db, &regle(nom, "contains", json!(v)));
                    let non = albums(&db, &regle(nom, "not_contains", json!(v)));
                    let mut union: Vec<i64> = oui.iter().chain(&non).copied().collect();
                    union.sort();
                    assert_eq!(union, tous_albums, "albums {nom} {v} : {oui:?} / {non:?}");
                    essais += 1;
                }
            }
        }
        assert!(essais > 50, "la garde ne joue presque rien : {essais}");

        let r = |champ: &str, v: &str| regle(champ, "not_contains", json!(v));
        // Playlists : la piste elle-même. La piste 3, sans genre, passe.
        assert_eq!(pistes(&db, &r("genre", "rock")), vec![1, 3]);
        // Collections, critère de PISTE : aucune piste de l'album ne contient.
        // Kind of Blue a une piste Rock : écarté, alors qu'une négation posée
        // ligne à ligne l'aurait gardé grâce à sa piste Jazz.
        assert_eq!(albums(&db, &r("genre", "rock")), vec![2]);
        // Un album dont l'unique piste n'a pas de genre passe aussi.
        assert_eq!(albums(&db, &r("genre", "jazz")), vec![2, 3]);
        // Critère d'ALBUM, valeur absente : « Seul » n'a pas de label.
        assert_eq!(albums(&db, &r("label", "col")), vec![2, 3]);
        assert_eq!(pistes(&db, &r("label", "col")), vec![3, 4]);
        // Insensible à la casse ET aux accents, comme « contient ».
        assert_eq!(pistes(&db, &r("artist", "MÎLES")), vec![3, 4]);
        assert_eq!(albums(&db, &r("artist_name", "MILES")), vec![2, 3]);
        // Les autres graphies se lisent pareil, et la clé `operator` aussi.
        for op in ["does_not_contain", "not_like"] {
            assert_eq!(
                pistes(&db, &regle("genre", op, json!("rock"))),
                vec![1, 3],
                "{op}"
            );
            assert_eq!(
                albums(&db, &regle("genre", op, json!("rock"))),
                vec![2],
                "{op}"
            );
        }
        let operator =
            json!([{"field": "genre", "operator": "not_contains", "value": "rock"}]).to_string();
        assert_eq!(pistes(&db, &operator), vec![1, 3]);
        assert_eq!(albums(&db, &operator), vec![2]);
        // Les jokers restent des caractères : « 100% » ne vaut pas « 100 ».
        db.execute_batch("UPDATE albums SET label = '100 Records' WHERE id = 2;")
            .unwrap();
        assert_eq!(albums(&db, &r("label", "100%")), vec![1, 2, 3]);
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

    /// #6123 — « note de la piste » lit `tracks.rating`, des deux côtés ; et
    /// « note de l'album » (`rating`) garde son sens, même quand les pistes
    /// portent une note.
    ///
    /// Notes de piste posées ici : 1 → 4, 2 → 1, 3 → 5, 4 aucune.
    #[test]
    fn la_note_de_la_piste_marche_des_deux_cotes_6123() {
        let db = bibliotheque();
        db.execute_batch(
            "UPDATE tracks SET rating = 4 WHERE id = 1; \
             UPDATE tracks SET rating = 1 WHERE id = 2; \
             UPDATE tracks SET rating = 5 WHERE id = 3;",
        )
        .unwrap();
        let r = |op: &str, v: Value| regle("track_rating", op, v);
        assert_eq!(pistes(&db, &r(">=", json!(4))), vec![1, 3]);
        assert_eq!(pistes(&db, &r("<", json!(2))), vec![2]);
        assert_eq!(pistes(&db, &r("=", json!("5"))), vec![3]);
        // Une piste sans note ne passe aucune comparaison.
        assert_eq!(pistes(&db, &r("between", json!([0, 5]))), vec![1, 2, 3]);
        // Collections : l'album dont une piste passe.
        assert_eq!(albums(&db, &r(">=", json!(4))), vec![1, 2]);
        assert_eq!(albums(&db, &r("<", json!(2))), vec![1]);
        // La note de l'ALBUM n'a pas bougé : Kind of Blue 5, Giant Steps 2.
        assert_eq!(pistes(&db, &regle("rating", ">=", json!(4))), vec![1, 2]);
        assert_eq!(albums(&db, &regle("rating", ">=", json!(4))), vec![1]);
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

    /// Le compte du client : trente-cinq définitions, dont trente-trois aux
    /// collections (#5530 : « Généré par IA » ; #6123 : « Note de la piste » ;
    /// #6121 : « Artiste de l'album » ; #6124 : « Année de sortie originale »
    /// et « Version » ; #6232 : « Interprète » ; #6285 : « Verdict hi-res »,
    /// que `smartRegles.ts` n'a pas encore). Si ce nombre bouge,
    /// `smartRegles.ts` doit bouger aussi.
    #[test]
    fn le_tableau_a_la_taille_de_celui_du_client() {
        assert_eq!(CRITERES.len(), 35);
        assert_eq!(
            CRITERES.iter().filter(|c| c.collection.is_some()).count(),
            33
        );
    }
    // ---------------------------------------------------------------------
    // #6232 — « Interprète » (forum 2218).
    // ---------------------------------------------------------------------

    /// Le cas de Didier : sur l'album ordinaire 1 (Miles Davis), la piste 2
    /// porte la balise ARTIST « Miles Davis & Genesis ». En base, son artiste
    /// reste celui de l'album (le scan l'y met), et la balise vit dans
    /// `track_metadata['track_artist']`. Les autres pistes n'ont pas la clé :
    /// leur artiste en base répond.
    fn avec_un_invite() -> SqliteDb {
        let db = bibliotheque();
        db.execute_batch(
            "INSERT INTO track_metadata (track_id, key, value) VALUES \
               (2,'track_artist','Miles Davis & Genesis');",
        )
        .unwrap();
        db
    }

    #[test]
    fn l_interprete_d_une_piste_amene_son_album_6232() {
        let db = avec_un_invite();
        assert_eq!(
            albums(&db, &regle("track_artist", "contains", json!("genesis"))),
            vec![1]
        );
        // « Artiste » (l'artiste de l'album) ne la voit toujours pas : les
        // deux critères sont distincts.
        assert!(albums(&db, &regle("artist_name", "contains", json!("genesis"))).is_empty());
        assert!(albums(&db, &regle("album_artist", "contains", json!("genesis"))).is_empty());
        // Sans balise en base : l'artiste de la piste répond.
        assert_eq!(
            albums(&db, &regle("track_artist", "=", json!("john coltrane"))),
            vec![2]
        );
        assert_eq!(
            albums(&db, &regle("track_artist", "starts_with", json!("Miles"))),
            vec![1]
        );
        // « ne contient pas » : AUCUNE piste de l'album ne contient.
        assert_eq!(
            albums(
                &db,
                &regle("track_artist", "not_contains", json!("genesis"))
            ),
            vec![2, 3]
        );
        assert!(albums(&db, &regle("track_artist", "is_null", Value::Null)).is_empty());
    }

    #[test]
    fn l_interprete_dans_une_playlist_6232() {
        let db = avec_un_invite();
        assert_eq!(
            pistes(&db, &regle("track_artist", "contains", json!("Genesis"))),
            vec![2]
        );
        // « Artiste » d'une playlist lit l'artiste rangé en base : la piste
        // n'y répond pas, c'est le défaut du fil 2218.
        assert!(pistes(&db, &regle("artist", "contains", json!("Genesis"))).is_empty());
        assert_eq!(
            pistes(&db, &regle("track_artist", "=", json!("Miles Davis"))),
            vec![1]
        );
        assert_eq!(
            pistes(&db, &regle("track_artist", "!=", json!("Miles Davis"))),
            vec![2, 3, 4]
        );
    }

    /// Un artiste CORRIGÉ dans Tune (champ tenu) l'emporte sur la balise du
    /// fichier, que l'écriture dans les fichiers, désactivée par défaut, ne
    /// change pas.
    #[test]
    fn un_artiste_tenu_l_emporte_sur_la_balise_6232() {
        let db = avec_un_invite();
        db.execute_batch(
            "INSERT INTO track_metadata (track_id, key, value) VALUES \
               (2,'champs_tenus','{\"chemin\":\"/m/jazz/2.flac\",\"artist_id\":1,\"artist_name\":\"Miles Davis\"}');",
        )
        .unwrap();
        assert!(albums(&db, &regle("track_artist", "contains", json!("genesis"))).is_empty());
        assert!(pistes(&db, &regle("track_artist", "contains", json!("genesis"))).is_empty());
        assert_eq!(
            pistes(&db, &regle("track_artist", "=", json!("Miles Davis"))),
            vec![1, 2]
        );
        // Une tenue qui ne porte PAS l'artiste (le titre seul) laisse la
        // balise répondre.
        db.execute_batch(
            "UPDATE track_metadata SET value = '{\"chemin\":\"/m/jazz/2.flac\",\"title\":\"So What\"}' \
             WHERE track_id = 2 AND key = 'champs_tenus';",
        )
        .unwrap();
        assert_eq!(
            pistes(&db, &regle("track_artist", "contains", json!("genesis"))),
            vec![2]
        );
    }

    // ---------------------------------------------------------------------
    // #6124 — « Année de sortie originale » et « Version ».
    // ---------------------------------------------------------------------

    /// La bibliothèque commune, plus : Kind of Blue sorti à l'origine en
    /// 1959, Giant Steps en 1960 ; « Blue in Green » balisée
    /// `VERSION=Remaster 2009`, « Giant Steps » `VERSION=Mono`.
    fn avec_dates_et_versions() -> SqliteDb {
        let db = bibliotheque();
        db.execute_batch(
            "UPDATE albums SET original_year = 1959 WHERE id = 1; \
             UPDATE albums SET original_year = 1960 WHERE id = 2; \
             INSERT INTO track_metadata (track_id, key, value) VALUES \
               (1,'version','Remaster 2009'), (3,'version','Mono'), \
               (2,'original_date','1959-08-17');",
        )
        .unwrap();
        db
    }

    #[test]
    fn l_annee_originale_filtre_les_deux_niveaux_6124() {
        let db = avec_dates_et_versions();
        assert_eq!(
            pistes(&db, &regle("original_year", "=", json!(1959))),
            vec![1, 2]
        );
        assert_eq!(
            pistes(&db, &regle("original_year", ">", json!(1959))),
            vec![3]
        );
        assert_eq!(
            pistes(&db, &regle("original_year", "between", json!([1950, 1965]))),
            vec![1, 2, 3]
        );
        assert_eq!(
            albums(&db, &regle("original_year", "<", json!("1960"))),
            vec![1]
        );
        assert_eq!(
            albums(&db, &regle("original_year", "between", json!("1955,1960"))),
            vec![1, 2]
        );
        // Un album sans année originale ne passe aucune comparaison.
        assert!(!pistes(&db, &regle("original_year", "!=", json!(1959))).contains(&4));
    }

    #[test]
    fn la_version_filtre_les_deux_niveaux_6124() {
        let db = avec_dates_et_versions();
        assert_eq!(
            pistes(&db, &regle("version", "=", json!("remaster 2009"))),
            vec![1]
        );
        assert_eq!(
            pistes(&db, &regle("version", "contains", json!("MONO"))),
            vec![3]
        );
        assert_eq!(
            pistes(&db, &regle("version", "starts_with", json!("Rem"))),
            vec![1]
        );
        assert_eq!(
            pistes(&db, &regle("version", "in", json!("mono, autre"))),
            vec![3]
        );
        assert_eq!(
            pistes(&db, &regle("version", "is_not_null", Value::Null)),
            vec![1, 3]
        );
        assert_eq!(
            pistes(&db, &regle("version", "is_null", Value::Null)),
            vec![2, 4]
        );
        // Une autre clé de `track_metadata` n'est pas la version.
        assert!(!pistes(&db, &regle("version", "contains", json!("1959"))).contains(&2));
        assert_eq!(
            albums(&db, &regle("version", "contains", json!("mono"))),
            vec![2]
        );
        assert_eq!(
            albums(&db, &regle("version", "=", json!("Remaster 2009"))),
            vec![1]
        );
    }

    // ---------------------------------------------------------------------
    // #6121 — « Artiste de l'album » (forum 2218).
    // ---------------------------------------------------------------------

    /// La bibliothèque commune, plus une COMPILATION : album 4 « Ballads »,
    /// artiste d'album « Various Artists », pistes 5 (Bill Evans) et 6 (Miles
    /// Davis) balisées ALBUMARTIST=Various Artists. Les pistes 1 à 4 n'ont
    /// pas la balise : l'artiste de l'album sert de repli.
    fn avec_compilation() -> SqliteDb {
        let db = bibliotheque();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (4,'Various Artists'),(5,'Bill Evans'); \
             INSERT INTO albums (id, title, artist_id, track_count) VALUES (4,'Ballads',4,2); \
             INSERT INTO tracks (id, album_id, artist_id, album_artist, title, file_path) VALUES \
               (5,4,5,'Various Artists','Peace Piece','/m/c/5.flac'), \
               (6,4,1,'Various Artists','Flamenco Sketches','/m/c/6.flac');",
        )
        .unwrap();
        db
    }

    #[test]
    fn l_artiste_de_l_album_dans_une_collection() {
        let db = avec_compilation();
        assert_eq!(
            albums(&db, &regle("album_artist", "=", json!("various artists"))),
            vec![4]
        );
        // Sans balise : l'artiste de l'album répond.
        assert_eq!(
            albums(&db, &regle("album_artist", "=", json!("Miles Davis"))),
            vec![1]
        );
        assert_eq!(
            albums(&db, &regle("album_artist", "contains", json!("VARIOUS"))),
            vec![4]
        );
        assert_eq!(
            albums(&db, &regle("album_artist", "starts_with", json!("John"))),
            vec![2]
        );
        assert_eq!(
            albums(&db, &regle("album_artist", "!=", json!("Various Artists"))),
            vec![1, 2, 3]
        );
        assert_eq!(
            // En minuscules : le « parmi » des collections compare
            // `LOWER(col)` aux valeurs telles quelles (défaut commun à tous
            // les champs texte, hors de ce correctif).
            albums(
                &db,
                &regle("album_artist", "in", json!("autre, various artists"))
            ),
            vec![3, 4]
        );
        assert!(albums(&db, &regle("album_artist", "is_null", Value::Null)).is_empty());
        assert_eq!(
            albums(&db, &regle("album_artist", "is_not_null", Value::Null)),
            vec![1, 2, 3, 4]
        );
        // « Artiste » ne change pas : il lit toujours l'artiste de l'album.
        assert_eq!(
            albums(&db, &regle("artist_name", "=", json!("Miles Davis"))),
            vec![1]
        );
        assert_eq!(
            albums(&db, &regle("artist", "=", json!("Various Artists"))),
            vec![4]
        );
    }

    #[test]
    fn l_artiste_de_l_album_dans_une_playlist() {
        let db = avec_compilation();
        assert_eq!(
            pistes(&db, &regle("album_artist", "=", json!("Various Artists"))),
            vec![5, 6]
        );
        // Une piste sans balise ALBUMARTIST prend l'artiste de son album.
        assert_eq!(
            pistes(&db, &regle("album_artist", "=", json!("miles davis"))),
            vec![1, 2]
        );
        assert_eq!(
            pistes(&db, &regle("album_artist", "is_not_null", Value::Null)),
            vec![1, 2, 3, 4, 5, 6]
        );
        // « Artiste » reste l'interprète de la PISTE : la piste 6 de la
        // compilation en est.
        assert_eq!(
            pistes(&db, &regle("artist", "=", json!("Miles Davis"))),
            vec![1, 2, 6]
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

    // ---------------------------------------------------------------------
    // #6122 — « dans les N derniers jours » (forum 2217).
    // ---------------------------------------------------------------------

    /// La bibliothèque commune, plus des dates RÉCENTES, posées par rapport à
    /// l'horloge du test (les dates de `bibliotheque()` datent de 2024 et
    /// 2019) : album 4 « Récent », piste 5 dont le fichier a 5 jours ; une
    /// écoute de la piste 3 (Giant Steps) il y a 2 jours, une de la piste 4
    /// (Seul) il y a 40 jours.
    fn avec_dates_recentes() -> SqliteDb {
        let db = bibliotheque();
        let maintenant = chrono::Utc::now();
        let il_y_a = |j: i64| maintenant - chrono::Duration::days(j);
        let iso = |j: i64| il_y_a(j).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        db.execute_batch(&format!(
            "INSERT INTO albums (id, title, artist_id, track_count) VALUES (4,'Récent',3,1); \
             INSERT INTO tracks (id, album_id, artist_id, title, file_path, file_mtime) VALUES \
               (5,4,3,'Neuf','/m/neuf/5.flac',{mtime}); \
             INSERT INTO listen_history (track_id, title, listened_at) VALUES \
               (3,'Giant Steps','{deux}'), (4,'Solo','{quarante}');",
            mtime = il_y_a(5).timestamp(),
            deux = iso(2),
            quarante = iso(40),
        ))
        .unwrap();
        db
    }

    #[test]
    fn ajoutes_dans_les_n_derniers_jours_dans_une_collection() {
        let db = avec_dates_recentes();
        let r = |v: Value| albums(&db, &regle("added_at", "in_last", v));
        assert_eq!(r(json!(30)), vec![4]);
        // La valeur en chiffres, comme l'enverrait un champ texte.
        assert_eq!(r(json!("30")), vec![4]);
        // Le fichier a 5 jours : hors des 3 derniers.
        assert!(r(json!(3)).is_empty());
        // Même ensemble que l'ancienne écriture relative « après now-30d » —
        // qui, elle, ne change pas.
        assert_eq!(
            albums(&db, &regle("added_at", ">", json!("now-30d"))),
            vec![4]
        );
    }

    #[test]
    fn ecoutes_dans_les_n_derniers_jours_dans_une_collection() {
        let db = avec_dates_recentes();
        let r = |v: Value| albums(&db, &regle("last_played_at", "in_last", v));
        // Giant Steps, écouté il y a 2 jours ; Kind of Blue (2024) non.
        assert_eq!(r(json!(7)), vec![2]);
        assert_eq!(r(json!(60)), vec![2, 3]);
        assert!(r(json!(1)).is_empty());
    }

    #[test]
    fn dans_les_n_derniers_jours_dans_une_playlist() {
        let db = avec_dates_recentes();
        assert_eq!(
            pistes(&db, &regle("added_at", "in_last", json!(30))),
            vec![5]
        );
        assert!(pistes(&db, &regle("added_at", "in_last", json!(3))).is_empty());
        assert_eq!(
            pistes(&db, &regle("last_played_at", "in_last", json!(7))),
            vec![3]
        );
        assert_eq!(
            pistes(&db, &regle("last_played_at", "in_last", json!("60"))),
            vec![3, 4]
        );
    }

    /// Un N illisible, nul ou négatif ne retient RIEN — jamais toute la
    /// bibliothèque (#1231) — et la playlist le dit dans son rapport.
    #[test]
    fn un_nombre_de_jours_illisible_ne_retient_rien() {
        let db = avec_dates_recentes();
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        for champ in ["added_at", "last_played_at"] {
            for v in [json!(0), json!(-3), json!("abc"), Value::Null] {
                let r = regle(champ, "in_last", v.clone());
                assert!(albums(&db, &r).is_empty(), "{champ} {v}");
                let (w, _o, _l) = build_album_query(&r, "all", "title", "asc", None, &ctx);
                assert!(w.contains(crate::regles_sql::FAUX), "{champ} {v} : {w}");
                let (w, _o, _l, refusees) =
                    build_smart_query_rapport(&r, "all", "title", "asc", None, &ctx);
                assert!(w.contains(crate::regles_sql::FAUX), "{champ} {v} : {w}");
                assert!(!refusees.is_empty(), "{champ} {v} : refus non signalé");
            }
        }
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
///
/// Seule retouche voulue depuis : la clause « Favori · est · Piste » (#5997,
/// forum #2127) départage désormais les éditions par l'album et l'ISRC — la
/// ligne gelée correspondante a été mise à jour avec elle.
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

#[cfg(test)]
#[path = "criteres_hires_6285_tests.rs"]
mod hires_6285;
