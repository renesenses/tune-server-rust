//! #7064 — un nom de label qui est en fait une ligne de copyright.
//!
//! Certaines balises LABEL portent la ligne ℗/© de l'album au lieu du nom du
//! label : « 2003 EMI Records Ltd. », « 2012 London Symphony Orchestra »,
//! « ℗ 1963 The Verve Music Group… ». L'onglet Labels les montrait telles
//! quelles, et aucun logo ne pouvait leur correspondre.
//!
//! La normalisation est PRUDENTE : elle ne retire que ce qui est en TÊTE.
//!
//! - les marques ℗, ©, Ⓟ, (P), (C) ;
//! - puis une année (1900–2099), ou une plage d'années, suivie d'une espace.
//!
//! Une année seule n'est retirée que si le reste compte au moins deux mots
//! et ne commence pas par un mot générique (« Records », « Music »…) :
//! « 1972 Records » ou « 2000 Music » peuvent être de vrais noms. Une marque
//! ℗/© lève ce doute : ce qui la suit est une ligne de copyright.
//!
//! #7089 — une ligne de copyright porte souvent la société mère après le
//! label : « The Verve Music Group a division of UMG Recordings, Inc. ». La
//! queue « a division of … » est coupée : ce qui la précède est le label. Un
//! nom qui COMMENCE par « a division of » ne bouge pas.
//!
//! Elle ne touche à rien d'autre : ni la casse, ni « Ltd. »/« Inc. », ni la
//! ponctuation. Deux labels distincts ne sont donc jamais fusionnés. Seule une
//! ligne de copyright peut rejoindre le label qu'elle désigne, et c'est le but.
//! « 4AD » et « 32 Jazz » restent tels quels.

use std::borrow::Cow;
use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

const MARQUES: &[&str] = &["℗", "©", "Ⓟ", "(P)", "(p)", "(C)", "(c)"];

/// Un premier mot qui, après une année, laisse penser que l'année fait partie
/// du nom (« 1972 Records ») : on ne la retire pas.
const MOTS_GENERIQUES: &[&str] = &[
    "records",
    "recordings",
    "music",
    "musique",
    "label",
    "labels",
    "productions",
    "production",
    "entertainment",
    "media",
    "disques",
    "ltd",
    "ltd.",
    "inc",
    "inc.",
    "sound",
    "sounds",
    "studio",
    "studios",
];

/// Une année 1900–2099 en tête de `s` : rend ce qui la suit.
fn annee_en_tete(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let annee = |i: usize| {
        b.len() >= i + 4
            && b[i..i + 4].iter().all(u8::is_ascii_digit)
            && matches!(&b[i..i + 2], b"19" | b"20")
    };
    if !annee(0) {
        return None;
    }
    let mut reste = &s[4..];
    // Une plage : « 1999-2003 », « 1999 – 2003 », « 1999/2003 ».
    let sans_espace = reste.trim_start();
    if let Some(apres) = ['-', '–', '/']
        .iter()
        .find_map(|t| sans_espace.strip_prefix(*t))
    {
        let apres = apres.trim_start();
        if annee_en_tete_seule(apres) {
            reste = &apres[4..];
        }
    }
    let reste = reste.strip_prefix(',').unwrap_or(reste);
    // L'année doit être un mot entier : « 2003EMI » n'est pas une année.
    reste
        .starts_with(char::is_whitespace)
        .then(|| reste.trim_start())
}

fn annee_en_tete_seule(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 4 && b[..4].iter().all(u8::is_ascii_digit) && matches!(&b[..2], b"19" | b"20")
}

/// La marque de la société mère dans une ligne de copyright (#7089).
const MARQUE_DE_DIVISION: &str = " a division of ";

/// Coupe « a division of … » et ce qui suit, si ce qui précède contient au
/// moins une lettre. Comparaison sans casse ; une virgule ou un tiret qui
/// précède la marque part avec elle.
fn sans_societe_mere(s: &str) -> &str {
    let minuscules = s.to_lowercase();
    // `to_lowercase` peut changer la longueur d'un caractère non ASCII : on
    // ne coupe que si l'index retrouvé tombe sur la même marque dans `s`.
    let Some(i) = minuscules.find(MARQUE_DE_DIVISION) else {
        return s;
    };
    if !s.is_char_boundary(i)
        || !s
            .get(i..i + MARQUE_DE_DIVISION.len())
            .is_some_and(|m| m.eq_ignore_ascii_case(MARQUE_DE_DIVISION))
    {
        return s;
    }
    let tete =
        s[..i].trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '-' | '–' | '—'));
    if tete.chars().any(char::is_alphabetic) {
        tete
    } else {
        s
    }
}

/// Le nom de label débarrassé d'une ligne de copyright en tête. Rend le nom
/// d'origine (emprunté) quand il n'y a rien à retirer, ou quand ce qui
/// resterait ne contient aucune lettre.
pub fn nom_de_label_normalise(brut: &str) -> Cow<'_, str> {
    let mut s = brut.trim();
    let mut marque = false;
    while let Some(apres) = MARQUES.iter().find_map(|m| s.strip_prefix(m)) {
        s = apres.trim_start();
        marque = true;
    }
    if let Some(reste) = annee_en_tete(s) {
        let mut mots = reste.split_whitespace();
        let premier = mots.next().map(str::to_lowercase);
        let deux_mots = mots.next().is_some();
        let generique = premier
            .as_deref()
            .is_some_and(|p| MOTS_GENERIQUES.contains(&p));
        if !reste.is_empty() && (marque || (deux_mots && !generique)) {
            s = reste;
        }
    }
    let s = sans_societe_mere(s);
    if s == brut || !s.chars().any(char::is_alphabetic) {
        Cow::Borrowed(brut)
    } else {
        Cow::Owned(s.to_string())
    }
}

/// Le nom normalisé, seulement s'il diffère du nom d'origine.
pub fn normaliser_si_besoin(brut: &str) -> Option<String> {
    match nom_de_label_normalise(brut) {
        Cow::Owned(s) => Some(s),
        Cow::Borrowed(_) => None,
    }
}

fn placeholder(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Postgres => PostgresDialect.placeholder(i),
        Engine::Sqlite => SqliteDialect.placeholder(i),
    }
}

/// Normalise les labels DÉJÀ en base (`albums.label`, `tracks.label`). La
/// lecture des balises normalise les fichiers relus ; cette passe rattrape
/// ceux qu'aucun scan ne relira. Idempotente : un second passage ne touche
/// aucune ligne. Elle ne lit que les labels distincts (quelques centaines) et
/// n'écrit que ceux qui changent. Rend le nombre de lignes modifiées.
pub fn normaliser_les_labels_en_base(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    let engine = db.engine();
    let mut lignes = 0;
    for table in ["albums", "tracks"] {
        let noms = db.query_many(
            &format!("SELECT DISTINCT label FROM {table} WHERE label IS NOT NULL AND label != ''"),
            &[],
        )?;
        let sql = format!(
            "UPDATE {table} SET label = {} WHERE label = {}",
            placeholder(engine, 1),
            placeholder(engine, 2)
        );
        for brut in noms.iter().filter_map(|r| r.first()?.as_string()) {
            if let Some(propre) = normaliser_si_besoin(&brut) {
                lignes += db.execute(&sql, &[&propre as &dyn ToSqlValue, &brut])?;
            }
        }
    }
    Ok(lignes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        nom_de_label_normalise(s).into_owned()
    }

    /// Les trois lignes de copyright de la vraie liste du .18 (26/10/2026).
    #[test]
    fn les_lignes_de_copyright_du_18_perdent_leur_annee_7064() {
        assert_eq!(
            n("1963 The Verve Music Group a division of UMG Recordings, Inc."),
            "The Verve Music Group",
            "#7089 — la société mère après « a division of » n'est pas le label"
        );
        assert_eq!(n("2003 EMI Records Ltd."), "EMI Records Ltd.");
        assert_eq!(
            n("2012 London Symphony Orchestra"),
            "London Symphony Orchestra"
        );
    }

    #[test]
    fn les_marques_de_copyright_sont_retirees() {
        assert_eq!(n("℗ 2003 EMI Records Ltd."), "EMI Records Ltd.");
        assert_eq!(n("© 2012 Warp"), "Warp");
        assert_eq!(n("(P) 1999 Mute"), "Mute");
        assert_eq!(n("℗© 2001 Domino"), "Domino");
        assert_eq!(n("℗ ECM Records"), "ECM Records");
        assert_eq!(
            n("1999-2003 Sony Music Entertainment"),
            "Sony Music Entertainment"
        );
        assert_eq!(n("2003, EMI Records Ltd."), "EMI Records Ltd.");
    }

    /// #7089 — « a division of … » est coupé, avec ou sans année ni
    /// marque, sans casse ; un nom qui ne garderait aucune lettre ne bouge pas.
    #[test]
    fn la_societe_mere_apres_a_division_of_est_coupee_7089() {
        assert_eq!(
            n("The Verve Music Group a division of UMG Recordings, Inc."),
            "The Verve Music Group"
        );
        assert_eq!(
            n("℗ 2011 Island Records, a division of Universal Music Operations Limited"),
            "Island Records"
        );
        assert_eq!(n("Decca A Division Of Universal Music"), "Decca");
        assert_eq!(n("Rhino - a division of Warner Music"), "Rhino");
        for nom in [
            "a division of UMG",
            "Universal Division Jazz",
            "Division of Labour Records",
        ] {
            assert!(
                matches!(nom_de_label_normalise(nom), Cow::Borrowed(_)),
                "« {nom} » ne devait pas changer, devenu « {} »",
                n(nom)
            );
        }
    }

    /// Aucun vrai nom de label n'est touché, et deux labels distincts ne se
    /// rejoignent jamais.
    #[test]
    fn les_vrais_noms_ne_bougent_pas() {
        for nom in [
            "4AD",
            "32 Jazz",
            "1972 Records",
            "2000 Music Ltd",
            "1965",
            "Blue Note",
            "Warp Records",
            "Warp",
            "BMG Direct Marketing, Inc.",
            "Sony Music Japan International Inc.",
            "2003EMI",
            "1800 Strings Orchestra",
            "© 2003",
        ] {
            assert!(
                matches!(nom_de_label_normalise(nom), Cow::Borrowed(_)),
                "« {nom} » ne devait pas changer, devenu « {} »",
                n(nom)
            );
        }
        assert_ne!(n("Warp Records"), n("Warp"));
    }

    /// Le chemin du scan : la balise LABEL lue dans un fichier est normalisée
    /// avec les autres champs texte (`sanitize_text_fields`).
    #[test]
    fn la_lecture_des_balises_normalise_le_label_7064() {
        let mut meta = crate::metadata::TrackMetadata {
            label: Some("2003 EMI Records Ltd.".into()),
            ..Default::default()
        };
        meta.sanitize_text_fields();
        assert_eq!(
            meta.label.as_deref(),
            Some("EMI Records Ltd."),
            "#7064 — le scan doit ranger le nom du label, pas la ligne de copyright"
        );
        let mut vrai = crate::metadata::TrackMetadata {
            label: Some("4AD".into()),
            ..Default::default()
        };
        vrai.sanitize_text_fields();
        assert_eq!(vrai.label.as_deref(), Some("4AD"));
    }

    /// Une base FICHIER : en mémoire, le pool de lecture ne voit pas
    /// toujours les écritures, et l'épreuve deviendrait aveugle. Le dossier
    /// est rendu avec la base : il est supprimé à sa sortie de portée.
    fn base() -> (crate::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
        let dossier = crate::test_scratch::scratch_dir("labels_7064");
        let chemin = dossier.join("tune.db");
        let db = crate::db::sqlite::SqliteDb::open(chemin.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();
        (dossier, Arc::new(db))
    }

    /// La passe de démarrage réécrit les labels déjà en base, albums ET
    /// pistes, ne touche pas les vrais noms, et ne fait rien au second tour.
    #[test]
    fn la_passe_normalise_la_base_et_reste_idempotente_7064() {
        let (_dossier, db) = base();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'A');
             INSERT INTO albums (id, title, artist_id, label) VALUES
               (1, 'Un', 1, '2003 EMI Records Ltd.'),
               (2, 'Deux', 1, 'EMI Records Ltd.'),
               (3, 'Trois', 1, '4AD'),
               (4, 'Quatre', 1, '1972 Records');
             INSERT INTO tracks (id, title, album_id, artist_id, label) VALUES
               (1, 't1', 1, 1, '2003 EMI Records Ltd.'),
               (2, 't3', 3, 1, '4AD');",
        )
        .unwrap();

        let touchees = normaliser_les_labels_en_base(&db).unwrap();
        assert_eq!(
            touchees, 2,
            "un album et une piste portaient la ligne de copyright"
        );

        let labels: Vec<String> = db
            .query_many("SELECT label FROM albums ORDER BY id", &[])
            .unwrap()
            .into_iter()
            .filter_map(|r| r.first()?.as_string())
            .collect();
        assert_eq!(
            labels,
            [
                "EMI Records Ltd.",
                "EMI Records Ltd.",
                "4AD",
                "1972 Records"
            ],
            "#7064 — la ligne de copyright doit rejoindre le label qu'elle désigne"
        );
        let piste = db
            .query_many("SELECT label FROM tracks WHERE id = 1", &[])
            .unwrap()[0][0]
            .as_string();
        assert_eq!(piste.as_deref(), Some("EMI Records Ltd."));

        assert_eq!(
            normaliser_les_labels_en_base(&db).unwrap(),
            0,
            "second passage"
        );
    }
}
