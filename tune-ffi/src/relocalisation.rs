//! Relocalisation du conteneur de l'application hôte.
//!
//! iOS range chaque application dans un conteneur dont le chemin porte un
//! UUID (`…/Containers/Data/Application/<UUID>`), et ce chemin CHANGE à la
//! réinstallation, à la restauration d'une sauvegarde, et parfois à la mise à
//! jour. Le serveur, lui, range des chemins ABSOLUS : `tracks.file_path`,
//! `albums.folder_path`, les pochettes, `settings['music_dirs']`… Après un
//! déplacement, plus rien ne mène nulle part, et le scan réimporte la
//! bibliothèque à côté de l'ancienne (2 albums et 20 pistes pour 5 fichiers,
//! vu sur le simulateur le 11/10).
//!
//! L'hôte donne donc la racine de son conteneur (`tune_server_set_container_root`,
//! iOS : `NSHomeDirectory()`). Elle est retenue en base (`settings`, clé
//! [`CLE_RACINE`]). Au démarrage suivant, si elle a changé et que l'ancienne
//! n'existe plus, chaque valeur texte de la base qui commence par l'ancienne
//! racine — ou qui la contient, pour les réglages en JSON — est réécrite sous
//! la nouvelle, AVANT que le scan, le surveillant ou une lecture ne la lise.
//!
//! Le balayage est générique (toutes les colonnes texte de toutes les tables
//! ordinaires) plutôt qu'une liste de colonnes : une colonne de chemin ajoutée
//! demain est couverte sans y penser. Il est sûr parce que la racine contient
//! l'UUID du conteneur : aucune autre valeur ne peut la contenir par hasard.
//! Les tables virtuelles (plein texte) et leurs tables fantômes sont laissées
//! à leurs déclencheurs.

use rusqlite::{Connection, OptionalExtension};

/// Clé de `settings` qui retient la racine du conteneur au dernier démarrage.
pub(crate) const CLE_RACINE: &str = "ffi_container_root";

/// Ce que la relocalisation a fait, pour le journal et les essais.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Bilan {
    /// L'ancienne racine, quand un déplacement a été constaté.
    pub ancienne: Option<String>,
    /// Valeurs réécrites, toutes tables confondues.
    pub reecrites: usize,
    /// Valeurs restées sous l'ancienne racine : une contrainte d'unicité a
    /// refusé la réécriture (la même piste existe déjà sous le nouveau chemin,
    /// réimportée avant cette correction). Le scan suivant ne les voit plus.
    pub restees: usize,
}

fn sans_barre_finale(chemin: &str) -> &str {
    let c = chemin.trim_end_matches('/');
    if c.is_empty() { "/" } else { c }
}

/// L'ancienne racine, quand la base n'en a pas retenu (base créée avant cette
/// correction) : on la déduit d'un dossier de musique disparu. Si l'hôte passe
/// `<racine>/Documents/Music` et que la base retient `X/Documents/Music`, qui
/// n'existe plus, l'ancienne racine était `X`.
fn racine_deduite(
    conn: &Connection,
    racine: &str,
    dossiers_de_l_hote: &[String],
    existe: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let brut: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'music_dirs'",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()?;
    let retenus: Vec<String> = serde_json::from_str(&brut).ok()?;
    for dossier in dossiers_de_l_hote {
        let Some(relatif) = dossier.strip_prefix(racine) else {
            continue;
        };
        if !relatif.starts_with('/') {
            continue;
        }
        for ancien in &retenus {
            if ancien == dossier || existe(ancien) {
                continue;
            }
            if let Some(avant) = ancien.strip_suffix(relatif)
                && !avant.is_empty()
                && avant != "/"
            {
                return Some(avant.to_string());
            }
        }
    }
    None
}

/// Les colonnes texte des tables ordinaires, hors tables virtuelles et leurs
/// tables fantômes (`<virtuelle>_content`, `_data`, …).
fn colonnes_texte(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut virtuelles = Vec::new();
    let mut tables = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT name, COALESCE(sql, '') FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let lignes = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for l in lignes {
            let (nom, sql) = l?;
            if sql
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE VIRTUAL")
            {
                virtuelles.push(nom);
            } else {
                tables.push(nom);
            }
        }
    }
    tables.retain(|t| !virtuelles.iter().any(|v| t.starts_with(&format!("{v}_"))));

    let mut colonnes = Vec::new();
    for table in tables {
        let mut st = conn.prepare(&format!(
            "SELECT name, type FROM pragma_table_info('{}')",
            table.replace('\'', "''")
        ))?;
        let lignes = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for l in lignes {
            let (colonne, genre) = l?;
            let genre = genre.to_ascii_uppercase();
            // Affinité texte (TEXT, VARCHAR, CLOB) ou colonne sans type.
            if genre.is_empty()
                || genre.contains("TEXT")
                || genre.contains("CHAR")
                || genre.contains("CLOB")
            {
                colonnes.push((table.clone(), colonne));
            }
        }
    }
    Ok(colonnes)
}

fn ident(nom: &str) -> String {
    format!("\"{}\"", nom.replace('"', "\"\""))
}

/// Réécrit `ancienne` en `nouvelle` dans toutes les valeurs texte de la base.
fn reecrire(conn: &Connection, ancienne: &str, nouvelle: &str) -> rusqlite::Result<(usize, usize)> {
    let ancienne_dossier = format!("{ancienne}/");
    let nouvelle_dossier = format!("{nouvelle}/");
    let mut reecrites = 0;
    let mut restees = 0;
    for (table, colonne) in colonnes_texte(conn)? {
        let (t, c) = (ident(&table), ident(&colonne));
        // La racine elle-même (un dossier de musique égal à la racine).
        reecrites += conn.execute(
            &format!("UPDATE OR IGNORE {t} SET {c} = ?2 WHERE {c} = ?1"),
            [ancienne, nouvelle],
        )?;
        // Tout ce qui est dessous, en tête de valeur ou dans un JSON.
        reecrites += conn.execute(
            &format!(
                "UPDATE OR IGNORE {t} SET {c} = replace({c}, ?1, ?2) WHERE instr({c}, ?1) > 0"
            ),
            [ancienne_dossier.as_str(), nouvelle_dossier.as_str()],
        )?;
        let reste: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {t} WHERE {c} = ?1 OR instr({c}, ?2) > 0"),
            [ancienne, ancienne_dossier.as_str()],
            |r| r.get(0),
        )?;
        restees += reste as usize;
    }
    Ok((reecrites, restees))
}

/// Relocalise la base si le conteneur a changé depuis le dernier démarrage,
/// puis retient la racine du jour. `existe` dit si un chemin existe encore
/// (`Path::exists` en vrai, un double dans les essais).
pub(crate) fn relocaliser(
    conn: &Connection,
    racine: &str,
    dossiers_de_l_hote: &[String],
    existe: &dyn Fn(&str) -> bool,
) -> Result<Bilan, String> {
    let racine = sans_barre_finale(racine);
    if racine == "/" || !racine.starts_with('/') {
        return Err(format!("racine de conteneur refusée : {racine:?}"));
    }
    let retenue: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [CLE_RACINE],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("lecture de {CLE_RACINE} : {e}"))?;
    let ancienne = match retenue {
        Some(r) => Some(sans_barre_finale(&r).to_string()),
        None => racine_deduite(conn, racine, dossiers_de_l_hote, existe),
    };

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("transaction : {e}"))?;
    let mut bilan = Bilan::default();
    if let Some(ancienne) = ancienne.filter(|a| a != racine) {
        if existe(&ancienne) {
            // Deux conteneurs vivants : ce n'est pas un déplacement. On ne
            // touche à rien, et on ne retient pas la nouvelle racine.
            return Err(format!(
                "l'ancienne racine {ancienne} existe encore : base laissée telle quelle"
            ));
        }
        let (reecrites, restees) =
            reecrire(&tx, &ancienne, racine).map_err(|e| format!("réécriture : {e}"))?;
        bilan = Bilan {
            ancienne: Some(ancienne),
            reecrites,
            restees,
        };
    }
    tx.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [CLE_RACINE, racine],
    )
    .map_err(|e| format!("écriture de {CLE_RACINE} : {e}"))?;
    tx.commit().map_err(|e| format!("validation : {e}"))?;
    Ok(bilan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::sqlite::SqliteDb;

    const VIEUX: &str = "/var/mobile/Containers/Data/Application/AAAA-1111";
    const NEUF: &str = "/var/mobile/Containers/Data/Application/BBBB-2222";

    fn base() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        db
    }

    fn jamais(_: &str) -> bool {
        false
    }

    fn piste(conn: &Connection, chemin: &str) {
        conn.execute(
            "INSERT INTO tracks (title, file_path) VALUES ('t', ?1)",
            [chemin],
        )
        .unwrap();
    }

    fn chemins(conn: &Connection) -> Vec<String> {
        let mut st = conn
            .prepare("SELECT file_path FROM tracks ORDER BY file_path")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn reglage(conn: &Connection, cle: &str) -> Option<String> {
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [cle], |r| {
            r.get(0)
        })
        .optional()
        .unwrap()
    }

    #[test]
    fn premier_demarrage_retient_la_racine_sans_rien_reecrire() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        let bilan = relocaliser(&conn, &format!("{NEUF}/"), &[], &jamais).unwrap();
        assert_eq!(bilan, Bilan::default());
        assert_eq!(reglage(&conn, CLE_RACINE).as_deref(), Some(NEUF));
    }

    #[test]
    fn le_conteneur_deplace_reecrit_pistes_albums_et_reglages() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        relocaliser(&conn, VIEUX, &[], &jamais).unwrap();
        let musique = format!("{VIEUX}/Documents/Music");
        piste(&conn, &format!("{musique}/Album/01.flac"));
        piste(&conn, &format!("{musique}/Album/02.flac"));
        piste(&conn, "/ailleurs/03.flac");
        conn.execute(
            "INSERT INTO albums (title, folder_path, cover_path) VALUES ('a', ?1, ?2)",
            [
                format!("{musique}/Album"),
                format!("{VIEUX}/Library/Application Support/Tune/artwork_cache/x.jpg"),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('music_dirs', ?1)",
            [serde_json::to_string(&vec![musique.clone()]).unwrap()],
        )
        .unwrap();

        let bilan = relocaliser(&conn, NEUF, &[], &jamais).unwrap();
        assert_eq!(bilan.ancienne.as_deref(), Some(VIEUX));
        assert_eq!(bilan.restees, 0);
        assert!(bilan.reecrites >= 5, "{bilan:?}");

        assert_eq!(
            chemins(&conn),
            vec![
                "/ailleurs/03.flac".to_string(),
                format!("{NEUF}/Documents/Music/Album/01.flac"),
                format!("{NEUF}/Documents/Music/Album/02.flac"),
            ]
        );
        let (dossier, pochette): (String, String) = conn
            .query_row("SELECT folder_path, cover_path FROM albums", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(dossier, format!("{NEUF}/Documents/Music/Album"));
        assert!(pochette.starts_with(NEUF), "{pochette}");
        assert_eq!(
            reglage(&conn, "music_dirs").unwrap(),
            serde_json::to_string(&vec![format!("{NEUF}/Documents/Music")]).unwrap()
        );
        assert_eq!(reglage(&conn, CLE_RACINE).as_deref(), Some(NEUF));

        // Démarrage suivant, même conteneur : rien ne bouge.
        let bilan = relocaliser(&conn, NEUF, &[], &jamais).unwrap();
        assert_eq!(bilan, Bilan::default());
    }

    #[test]
    fn base_d_avant_la_correction_la_racine_se_deduit_du_dossier_disparu() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        let ancien = format!("{VIEUX}/Documents/Music");
        piste(&conn, &format!("{ancien}/a.flac"));
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('music_dirs', ?1)",
            [serde_json::to_string(&vec![ancien]).unwrap()],
        )
        .unwrap();
        let hote = vec![format!("{NEUF}/Documents/Music")];
        let bilan = relocaliser(&conn, NEUF, &hote, &jamais).unwrap();
        assert_eq!(bilan.ancienne.as_deref(), Some(VIEUX));
        assert_eq!(
            chemins(&conn),
            vec![format!("{NEUF}/Documents/Music/a.flac")]
        );
    }

    #[test]
    fn un_doublon_deja_reimporte_reste_sous_l_ancien_chemin_et_se_compte() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        relocaliser(&conn, VIEUX, &[], &jamais).unwrap();
        piste(&conn, &format!("{VIEUX}/Documents/Music/a.flac"));
        piste(&conn, &format!("{NEUF}/Documents/Music/a.flac"));
        let bilan = relocaliser(&conn, NEUF, &[], &jamais).unwrap();
        assert_eq!(bilan.restees, 1, "{bilan:?}");
    }

    #[test]
    fn une_ancienne_racine_encore_vivante_n_est_pas_un_deplacement() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        relocaliser(&conn, VIEUX, &[], &jamais).unwrap();
        piste(&conn, &format!("{VIEUX}/Documents/Music/a.flac"));
        let vivant = |p: &str| p == VIEUX;
        assert!(relocaliser(&conn, NEUF, &[], &vivant).is_err());
        assert_eq!(
            chemins(&conn),
            vec![format!("{VIEUX}/Documents/Music/a.flac")]
        );
        assert_eq!(reglage(&conn, CLE_RACINE).as_deref(), Some(VIEUX));
    }

    #[test]
    fn une_racine_voisine_n_est_pas_touchee() {
        // `…/AAAA-1111` ne doit pas réécrire `…/AAAA-11112/…`.
        let db = base();
        let conn = db.connection().lock().unwrap();
        relocaliser(&conn, VIEUX, &[], &jamais).unwrap();
        let voisin = format!("{VIEUX}2/Documents/Music/a.flac");
        piste(&conn, &voisin);
        relocaliser(&conn, NEUF, &[], &jamais).unwrap();
        assert_eq!(chemins(&conn), vec![voisin]);
    }

    #[test]
    fn racine_relative_ou_nue_refusee() {
        let db = base();
        let conn = db.connection().lock().unwrap();
        assert!(relocaliser(&conn, "/", &[], &jamais).is_err());
        assert!(relocaliser(&conn, "Documents", &[], &jamais).is_err());
        assert!(reglage(&conn, CLE_RACINE).is_none());
    }
}
