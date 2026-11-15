//! #8676 — « Remplacer le chemin » : déplacer la racine de la bibliothèque.
//!
//! Fil forum 2799 : une base restaurée depuis une autre machine porte les
//! chemins ABSOLUS de cette machine (`/mnt/Music/…`), alors que la musique vit
//! ici sous un autre chemin (`/media/nas/Musique/…`). Rescanner recréerait
//! chaque piste sous un nouvel identifiant : écoutes, notes, favoris, listes de
//! lecture et crédits resteraient attachés aux anciennes lignes. Ce module
//! réécrit le PRÉFIXE des chemins stockés, en une transaction, sans toucher à
//! un seul identifiant.
//!
//! # Le recensement des chemins absolus
//!
//! [`COLONNES`] et [`REGLAGES_LISTES`] / [`REGLAGES_SIMPLES`] sont la liste
//! complète, relevée le 13/11/2026 sur `batch/bugs-rc12-20261108` (schémas
//! SQLite de `sqlite.rs` + `migrations.rs`, PostgreSQL de `pg_migrate.rs` +
//! `migrations/postgres/`). Ce qui n'y figure pas, délibérément :
//!
//! - `tracks.path_terms` : dérivée du chemin (colonne générée sous
//!   PostgreSQL, déclencheurs `tracks_fts` sous SQLite) — elle se recalcule
//!   d'elle-même quand `file_path` change ;
//! - `spectrogrammes`, `tracks.waveform_json`, `lyrics_cache`,
//!   `hires_verifications`, `enrich_tentatives`, `track_audio_embedding` :
//!   indexés par `track_id`, que l'opération conserve ; `spectrogrammes.fichier`
//!   est dans le dossier de données de Tune, pas dans la bibliothèque ;
//! - `offline_cache.file_path`, `profiles.avatar_path` : dossier de données ;
//! - `network_mounts.mount_path` : le point de montage d'un partage est une
//!   CONFIGURATION de la machine, pas un chemin de la bibliothèque ;
//! - `hidden_items`, `item_tags`, `notes_personnelles`, `queue_items`,
//!   `listen_history` : désignent les éléments par identifiant, jamais par
//!   chemin.
//!
//! # La comparaison
//!
//! Un préfixe se compare sur une FRONTIÈRE de composant : `/mnt/Music` couvre
//! `/mnt/Music` et `/mnt/Music/…`, jamais `/mnt/Music2/…` (#2016). Les
//! séparateurs finaux sont retirés. Un préfixe Windows (`D:\Musique`,
//! `\\nas\musique`) est comparé séparateurs NORMALISÉS (`\` → `/`), comme
//! [`crate::taches_de_fond::perimetre`] : une base Windows porte l'un ou
//! l'autre. Quand l'ancien et le nouveau préfixe ne sont pas du même système,
//! la fin du chemin prend les séparateurs du nouveau — sinon le prochain scan
//! sous Windows, qui écrit `\`, ne retrouverait pas ses pistes.
//!
//! Les valeurs passent en LITTÉRAUX SQL, guillemets simples doublés : forme
//! comprise à l'identique par SQLite et PostgreSQL (même raison que
//! `perimetre`), et les longueurs sont des entiers écrits dans le texte —
//! `SUBSTR(text, bigint)` n'existe pas sous PostgreSQL.
//!
//! # Réversible
//!
//! Remplacer `B` par `A` défait « remplacer `A` par `B` » : le journal
//! ([`CLE_JOURNAL`]) garde les deux préfixes de chaque opération.
use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::db::backend::{DbBackend, DbTxHandle, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Ce que fait l'opération d'une ligne dont le chemin réécrit existe déjà.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collision {
    /// Colonne sans contrainte d'unicité.
    Aucune,
    /// Colonne UNIQUE qui porte une piste : une collision veut dire que la
    /// piste existe déjà sous le nouveau chemin (un scan est passé). L'aperçu
    /// la compte et l'application est REFUSÉE — fusionner deux pistes n'est
    /// pas le rôle de ce geste.
    Refuser,
    /// Colonne UNIQUE d'un cache (date de première vue, tempo estimé) : la
    /// ligne déjà présente sous le nouveau chemin est gardée, l'ancienne reste
    /// sous l'ancien, sans effet.
    Garder,
}

/// Une colonne qui stocke un chemin absolu.
#[derive(Clone, Copy, Debug)]
pub struct Colonne {
    pub table: &'static str,
    pub colonne: &'static str,
    /// Texte fixe qui précède le chemin dans la valeur (`playlists.description`
    /// des listes de dossier : `Dossier : <chemin>`). Vide sinon.
    pub prefixe_de_valeur: &'static str,
    pub collision: Collision,
}

const fn col(table: &'static str, colonne: &'static str, collision: Collision) -> Colonne {
    Colonne {
        table,
        colonne,
        prefixe_de_valeur: "",
        collision,
    }
}

/// Le recensement des colonnes réécrites (voir la doc du module).
pub const COLONNES: &[Colonne] = &[
    // Le fichier de chaque piste (UNIQUE).
    col("tracks", "file_path", Collision::Refuser),
    // Le support d'une piste découpée par une feuille CUE (`file_path` nul).
    col("tracks", "cue_media_path", Collision::Aucune),
    // L'image de la piste (cache ou image du dossier).
    col("tracks", "cover_path", Collision::Aucune),
    // Les exemplaires identiques d'une piste (#4907, UNIQUE).
    col("track_copies", "file_path", Collision::Refuser),
    // Le dossier de l'album.
    col("albums", "folder_path", Collision::Aucune),
    // La pochette retenue (cache ou fichier du dossier).
    col("albums", "cover_path", Collision::Aucune),
    // Le fichier d'où vient la pochette (`folder.jpg`, piste à image intégrée).
    col("albums", "cover_source_path", Collision::Aucune),
    // L'image locale d'un artiste (`artist.jpg` du dossier, ou cache).
    col("artists", "image_path", Collision::Aucune),
    // Le dossier de musique préféré pour lire un album (#7876).
    col("album_preferred_roots", "root", Collision::Aucune),
    // Le chemin figé d'un favori, pour le retrouver après une purge.
    col("favorites", "item_path", Collision::Aucune),
    // Le chemin figé d'une entrée de liste devenue orpheline (#6179).
    col("playlist_tracks", "file_path", Collision::Aucune),
    // Les listes « dossier » : la description est leur clé (`folder_playlists`).
    Colonne {
        table: "playlists",
        colonne: "description",
        prefixe_de_valeur: crate::library::folder_playlists::DESC_PREFIX,
        collision: Collision::Aucune,
    },
    // La date de première vue d'un fichier (nouveautés, UNIQUE).
    col("file_first_seen", "file_path", Collision::Garder),
    // Le tempo estimé d'un fichier (UNIQUE).
    col("bpm_estimes", "file_path", Collision::Garder),
];

/// Réglages qui portent une LISTE JSON de chemins.
pub const REGLAGES_LISTES: &[&str] = &[
    "music_dirs",
    crate::library::exemplaires::CLE_ORDRE_DES_REPERTOIRES,
    crate::taches_de_fond::perimetre::CLE_RACINES_EXCLUES,
    crate::taches_de_fond::perimetre::CLE_DOSSIERS_INCLUS,
    crate::taches_de_fond::perimetre::CLE_DOSSIERS_EXCLUS_ENRICHISSEMENT,
    // Motifs d'exclusion du scan : seules les entrées qui sont un chemin sous
    // l'ancien préfixe changent ; un motif (`/incoming/`) reste tel quel.
    "scan_exclude_paths",
];

/// Réglages qui portent UN chemin.
pub const REGLAGES_SIMPLES: &[&str] = &[
    "cd_rip_output_dir",
    "sacd_rip_output_dir",
    "converter_output_root",
];

/// Le journal des opérations, dans `settings` : les [`JOURNAL_MAX`] dernières.
pub const CLE_JOURNAL: &str = "library_root_moves_journal";
const JOURNAL_MAX: usize = 20;

/// Nombre de pistes au plus dont l'aperçu vérifie la présence sur disque.
/// Au-delà, un échantillon régulier : 700 000 `stat` sur un partage réseau
/// prendraient des minutes.
pub const ECHANTILLON_MAX: usize = 2000;

/// Exemples rendus par l'aperçu.
const EXEMPLES_MAX: usize = 10;

/// Pourquoi l'opération est refusée. Le code est stable : le client le traduit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refus {
    /// Préfixe vide, racine seule (`/`), relatif ou contenant `..`.
    InvalidPrefix,
    /// Ancien et nouveau désignent le même chemin.
    SamePrefix,
    /// Aucune piste de la base sous l'ancien préfixe.
    NoTracks,
    /// Aucune des pistes vérifiées n'existe au nouveau chemin.
    NoneFound,
    /// Des pistes existent déjà en base sous le nouveau chemin.
    Conflicts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Unix,
    Windows,
}

impl Style {
    fn de(chemin: &str) -> Self {
        let b = chemin.as_bytes();
        let lecteur = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        if lecteur || chemin.contains('\\') {
            Style::Windows
        } else {
            Style::Unix
        }
    }

    fn separateur(self) -> char {
        match self {
            Style::Unix => '/',
            Style::Windows => '\\',
        }
    }
}

/// Les deux préfixes, validés et normalisés.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefixes {
    /// L'ancien préfixe, sans séparateur final.
    pub ancien: String,
    /// Le nouveau préfixe, sans séparateur final.
    pub nouveau: String,
    style_ancien: Style,
    style_nouveau: Style,
}

fn sans_separateur_final(p: &str) -> String {
    p.trim().trim_end_matches(['/', '\\']).to_string()
}

fn est_absolu(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with("\\\\")
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

impl Prefixes {
    /// Valide et normalise. `Err` porte le code du refus.
    pub fn nouveaux(ancien: &str, nouveau: &str) -> Result<Self, Refus> {
        let a = sans_separateur_final(ancien);
        let n = sans_separateur_final(nouveau);
        for p in [&a, &n] {
            if p.is_empty() || !est_absolu(p) || p.split(['/', '\\']).any(|c| c == "..") {
                return Err(Refus::InvalidPrefix);
            }
        }
        let prefixes = Prefixes {
            style_ancien: Style::de(&a),
            style_nouveau: Style::de(&n),
            ancien: a,
            nouveau: n,
        };
        if prefixes.ancien == prefixes.nouveau {
            return Err(Refus::SamePrefix);
        }
        Ok(prefixes)
    }

    /// L'ancien préfixe tel qu'il se compare (séparateurs normalisés pour un
    /// préfixe Windows).
    fn cle_ancien(&self) -> String {
        match self.style_ancien {
            Style::Unix => self.ancien.clone(),
            Style::Windows => self.ancien.replace('\\', "/"),
        }
    }

    /// Le préfixe inverse : appliqué après celui-ci, il défait l'opération.
    pub fn inverse(&self) -> Self {
        Prefixes {
            ancien: self.nouveau.clone(),
            nouveau: self.ancien.clone(),
            style_ancien: self.style_nouveau,
            style_nouveau: self.style_ancien,
        }
    }
}

/// Le chemin réécrit, ou `None` s'il n'est pas sous l'ancien préfixe (sur une
/// frontière de composant). Jumeau Rust exact de [`sql_reecriture`].
pub fn remplacer(chemin: &str, p: &Prefixes) -> Option<String> {
    remplacer_avec(chemin, "", p)
}

fn remplacer_avec(valeur: &str, prefixe_de_valeur: &str, p: &Prefixes) -> Option<String> {
    let chemin = valeur.strip_prefix(prefixe_de_valeur)?;
    let cle = p.cle_ancien();
    let compare: String = match p.style_ancien {
        Style::Unix => chemin.to_string(),
        Style::Windows => chemin.replace('\\', "/"),
    };
    let n = cle.chars().count();
    let tete: String = compare.chars().take(n).collect();
    if tete != cle {
        return None;
    }
    let suite_comparee: String = compare.chars().skip(n).collect();
    if !(suite_comparee.is_empty() || suite_comparee.starts_with('/')) {
        return None;
    }
    let mut reste: String = chemin.chars().skip(n).collect();
    if p.style_ancien != p.style_nouveau {
        let vers = p.style_nouveau.separateur();
        let depuis = p.style_ancien.separateur();
        reste = reste.replace(depuis, &vers.to_string());
    }
    let resultat = format!("{prefixe_de_valeur}{}{reste}", p.nouveau);
    (resultat != valeur).then_some(resultat)
}

fn litteral(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Les deux fragments SQL d'une colonne : (condition « sous l'ancien préfixe
/// et réellement changée », expression de la valeur réécrite). `expr` est la
/// colonne, qualifiée si besoin.
fn sql_reecriture(expr: &str, prefixe_de_valeur: &str, p: &Prefixes) -> (String, String) {
    let cle = format!("{prefixe_de_valeur}{}", p.cle_ancien());
    let n = cle.chars().count();
    let compare = match p.style_ancien {
        Style::Unix => expr.to_string(),
        Style::Windows => format!("REPLACE({expr}, '\\', '/')"),
    };
    let reste = format!("SUBSTR({expr}, {})", n + 1);
    let reste = if p.style_ancien != p.style_nouveau {
        format!(
            "REPLACE({reste}, {}, {})",
            litteral(&p.style_ancien.separateur().to_string()),
            litteral(&p.style_nouveau.separateur().to_string())
        )
    } else {
        reste
    };
    let nouvelle = format!(
        "({} || {reste})",
        litteral(&format!("{prefixe_de_valeur}{}", p.nouveau))
    );
    let condition = format!(
        "({expr} IS NOT NULL AND ({compare} = {} OR SUBSTR({compare}, 1, {}) = {}) AND {nouvelle} <> {expr})",
        litteral(&cle),
        n + 1,
        litteral(&format!("{cle}/")),
    );
    (condition, nouvelle)
}

/// Condition « la piste est sous l'ancien préfixe » (fichier ou support CUE).
fn condition_piste(p: &Prefixes) -> String {
    let (f, _) = sql_reecriture("tracks.file_path", "", p);
    let (c, _) = sql_reecriture("tracks.cue_media_path", "", p);
    format!("({f} OR {c})")
}

fn marque(engine: Engine, n: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

fn compter(db: &dyn DbBackend, sql: &str) -> Result<u64, String> {
    Ok(db
        .query_one(sql, &[])?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        .max(0) as u64)
}

/// La colonne existe-t-elle dans cette base ? Une table d'une fonction pas
/// encore migrée ne doit pas faire échouer la transaction entière.
fn colonne_presente(db: &dyn DbBackend, c: &Colonne) -> bool {
    db.query_one(
        &format!("SELECT {} FROM {} WHERE 1 = 0", c.colonne, c.table),
        &[],
    )
    .is_ok()
}

fn conflits_sql(c: &Colonne, p: &Prefixes, select: &str) -> String {
    let expr = format!("a.{}", c.colonne);
    let (cond, nouvelle) = sql_reecriture(&expr, c.prefixe_de_valeur, p);
    format!(
        "SELECT {select} FROM {t} a WHERE {cond} AND EXISTS \
         (SELECT 1 FROM {t} b WHERE b.{col} = {nouvelle})",
        t = c.table,
        col = c.colonne,
    )
}

/// Une colonne et le nombre de ses lignes concernées.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LignesDeColonne {
    pub table: &'static str,
    pub column: &'static str,
    pub rows: u64,
}

/// Un réglage qui change.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ReglageChange {
    pub key: &'static str,
    pub before: Value,
    pub after: Value,
}

/// Un exemple de piste : chemin actuel, chemin réécrit.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Exemple {
    pub old: String,
    pub new: String,
}

/// Ce que l'aperçu rend (clés en anglais : c'est le corps de l'API).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Apercu {
    pub old_prefix: String,
    pub new_prefix: String,
    /// Pistes de la base sous l'ancien préfixe (fichier ou support CUE).
    pub tracks: u64,
    /// Fichiers distincts dont la présence a été vérifiée au nouveau chemin.
    pub checked: u64,
    pub found: u64,
    pub missing: u64,
    /// Vrai si `checked` est un échantillon régulier, pas toutes les pistes.
    pub sampled: bool,
    pub missing_examples: Vec<Exemple>,
    pub found_examples: Vec<Exemple>,
    /// Pistes qui existent DÉJÀ en base sous le nouveau chemin.
    pub conflicts: u64,
    pub conflict_examples: Vec<String>,
    pub columns: Vec<LignesDeColonne>,
    pub settings: Vec<ReglageChange>,
    pub refusal: Option<Refus>,
    pub can_apply: bool,
}

fn liste_de_chemins(brut: &str) -> Option<Vec<String>> {
    serde_json::from_str::<Vec<String>>(brut).ok()
}

/// Les réglages qui changeraient, lus par `lire` (la base ou la transaction).
fn reglages_changes(
    lire: &dyn Fn(&str) -> Result<Option<String>, String>,
    p: &Prefixes,
) -> Result<Vec<(&'static str, String, ReglageChange)>, String> {
    let mut changes = Vec::new();
    for &cle in REGLAGES_LISTES {
        let Some(brut) = lire(cle)? else { continue };
        // Une valeur illisible n'est jamais réécrite : on ne remplace pas ce
        // qu'on ne comprend pas (même prudence que #6175 pour `music_dirs`).
        let Some(liste) = liste_de_chemins(&brut) else {
            continue;
        };
        let mut vus = HashSet::new();
        let mut apres = Vec::with_capacity(liste.len());
        let mut change = false;
        for e in &liste {
            let r = match remplacer(e, p) {
                Some(n) => {
                    change = true;
                    n
                }
                None => e.clone(),
            };
            // Le nouveau préfixe déjà déclaré : une seule entrée.
            if vus.insert(r.clone()) {
                apres.push(r);
            }
        }
        if change {
            let json = serde_json::to_string(&apres).map_err(|e| e.to_string())?;
            changes.push((
                cle,
                json,
                ReglageChange {
                    key: cle,
                    before: Value::from(liste),
                    after: Value::from(apres),
                },
            ));
        }
    }
    for &cle in REGLAGES_SIMPLES {
        let Some(brut) = lire(cle)? else { continue };
        if let Some(n) = remplacer(&brut, p) {
            changes.push((
                cle,
                n.clone(),
                ReglageChange {
                    key: cle,
                    before: Value::from(brut),
                    after: Value::from(n),
                },
            ));
        }
    }
    Ok(changes)
}

fn lire_reglage(db: &dyn DbBackend, cle: &str) -> Result<Option<String>, String> {
    Ok(db
        .query_one(
            &format!(
                "SELECT value FROM settings WHERE key = {}",
                marque(db.engine(), 1)
            ),
            &[&cle as &dyn ToSqlValue],
        )?
        .and_then(|r| r.first().and_then(|v| v.as_string())))
}

/// L'aperçu : rien n'est écrit. `existe` répond « ce fichier est-il sur le
/// disque ? » — `Path::exists` en production, une table en épreuve.
pub fn apercu(
    db: &dyn DbBackend,
    p: &Prefixes,
    echantillon_max: usize,
    existe: &dyn Fn(&str) -> bool,
) -> Result<Apercu, String> {
    let cond = condition_piste(p);
    let ids: Vec<i64> = db
        .query_many(
            &format!("SELECT id FROM tracks WHERE {cond} ORDER BY id"),
            &[],
        )?
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let tracks = ids.len() as u64;

    // Échantillon régulier sur les identifiants triés.
    let echantillon: Vec<i64> = if ids.len() <= echantillon_max {
        ids.clone()
    } else {
        let pas = ids.len() as f64 / echantillon_max as f64;
        (0..echantillon_max)
            .map(|i| ids[((i as f64) * pas) as usize])
            .collect()
    };
    let sampled = echantillon.len() < ids.len();

    let mut a_verifier: Vec<(String, String)> = Vec::new();
    let mut vus = HashSet::new();
    for lot in echantillon.chunks(500) {
        let liste = lot
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        for r in db.query_many(
            &format!(
                "SELECT file_path, cue_media_path FROM tracks WHERE id IN ({liste}) ORDER BY id"
            ),
            &[],
        )? {
            let f = r
                .first()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let c = r
                .get(1)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let paire = [f, c]
                .into_iter()
                .flatten()
                .find_map(|ancien| remplacer(&ancien, p).map(|n| (ancien, n)));
            if let Some((ancien, nouveau)) = paire
                && vus.insert(nouveau.clone())
            {
                a_verifier.push((ancien, nouveau));
            }
        }
    }
    let mut found = 0u64;
    let mut missing_examples = Vec::new();
    let mut found_examples = Vec::new();
    for (ancien, nouveau) in &a_verifier {
        if existe(nouveau) {
            found += 1;
            if found_examples.len() < 3 {
                found_examples.push(Exemple {
                    old: ancien.clone(),
                    new: nouveau.clone(),
                });
            }
        } else if missing_examples.len() < EXEMPLES_MAX {
            missing_examples.push(Exemple {
                old: ancien.clone(),
                new: nouveau.clone(),
            });
        }
    }
    let checked = a_verifier.len() as u64;

    let mut columns = Vec::new();
    let mut conflicts = 0u64;
    let mut conflict_examples = Vec::new();
    for c in COLONNES {
        if !colonne_presente(db, c) {
            continue;
        }
        let (cond, _) = sql_reecriture(
            &format!("{}.{}", c.table, c.colonne),
            c.prefixe_de_valeur,
            p,
        );
        let rows = compter(
            db,
            &format!("SELECT COUNT(*) FROM {} WHERE {cond}", c.table),
        )?;
        if rows > 0 {
            columns.push(LignesDeColonne {
                table: c.table,
                column: c.colonne,
                rows,
            });
        }
        if c.collision == Collision::Refuser && rows > 0 {
            let n = compter(db, &conflits_sql(c, p, "COUNT(*)"))?;
            if n > 0 {
                conflicts += n;
                for r in db.query_many(
                    &format!(
                        "{} ORDER BY a.{} LIMIT {EXEMPLES_MAX}",
                        conflits_sql(c, p, &format!("a.{}", c.colonne)),
                        c.colonne
                    ),
                    &[],
                )? {
                    if conflict_examples.len() < EXEMPLES_MAX
                        && let Some(s) = r.first().and_then(|v| v.as_string())
                    {
                        conflict_examples.push(s);
                    }
                }
            }
        }
    }

    let settings = reglages_changes(&|k| lire_reglage(db, k), p)?
        .into_iter()
        .map(|(_, _, r)| r)
        .collect();

    let refusal = if tracks == 0 {
        Some(Refus::NoTracks)
    } else if found == 0 {
        Some(Refus::NoneFound)
    } else if conflicts > 0 {
        Some(Refus::Conflicts)
    } else {
        None
    };
    Ok(Apercu {
        old_prefix: p.ancien.clone(),
        new_prefix: p.nouveau.clone(),
        tracks,
        checked,
        found,
        missing: checked - found,
        sampled,
        missing_examples,
        found_examples,
        conflicts,
        conflict_examples,
        columns,
        settings,
        can_apply: refusal.is_none(),
        refusal,
    })
}

/// Ce que l'application a réécrit.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Bilan {
    pub old_prefix: String,
    pub new_prefix: String,
    pub columns: Vec<LignesDeColonne>,
    pub settings: Vec<&'static str>,
    /// Lignes `tracks.file_path` réécrites.
    pub tracks_updated: u64,
}

/// Applique la réécriture, en UNE transaction : tout ou rien. N'importe quel
/// échec (contrainte d'unicité comprise) annule tout.
///
/// Ne revérifie ni le disque ni les collisions : c'est le rôle de l'appelant,
/// qui passe par [`apercu`] juste avant.
pub fn appliquer(db: &Arc<dyn DbBackend>, p: &Prefixes) -> Result<Bilan, String> {
    let engine = db.engine();
    let presentes: Vec<Colonne> = COLONNES
        .iter()
        .copied()
        .filter(|c| colonne_presente(db.as_ref(), c))
        .collect();
    let mut bilan = Bilan {
        old_prefix: p.ancien.clone(),
        new_prefix: p.nouveau.clone(),
        columns: Vec::new(),
        settings: Vec::new(),
        tracks_updated: 0,
    };
    db.write_tx(&mut |tx: &dyn DbTxHandle| {
        bilan.columns.clear();
        bilan.settings.clear();
        bilan.tracks_updated = 0;
        for c in &presentes {
            let expr = format!("{}.{}", c.table, c.colonne);
            let (cond, nouvelle) = sql_reecriture(&expr, c.prefixe_de_valeur, p);
            let garde = if c.collision == Collision::Garder {
                format!(
                    " AND NOT EXISTS (SELECT 1 FROM {t} b WHERE b.{col} = {nouvelle})",
                    t = c.table,
                    col = c.colonne
                )
            } else {
                String::new()
            };
            let n = tx.execute(
                &format!(
                    "UPDATE {t} SET {col} = {nouvelle} WHERE {cond}{garde}",
                    t = c.table,
                    col = c.colonne
                ),
                &[],
            )? as u64;
            if c.table == "tracks" && c.colonne == "file_path" {
                bilan.tracks_updated = n;
            }
            if n > 0 {
                bilan.columns.push(LignesDeColonne {
                    table: c.table,
                    column: c.colonne,
                    rows: n,
                });
            }
        }
        let lire = |cle: &str| -> Result<Option<String>, String> {
            Ok(tx
                .query_one(
                    &format!(
                        "SELECT value FROM settings WHERE key = {}",
                        marque(engine, 1)
                    ),
                    &[&cle as &dyn ToSqlValue],
                )?
                .and_then(|r| r.first().and_then(|v| v.as_string())))
        };
        for (cle, valeur, _) in reglages_changes(&lire, p)? {
            tx.execute(
                &format!(
                    "UPDATE settings SET value = {} WHERE key = {}",
                    marque(engine, 1),
                    marque(engine, 2)
                ),
                &[&valeur as &dyn ToSqlValue, &cle as &dyn ToSqlValue],
            )?;
            bilan.settings.push(cle);
        }
        journaliser(tx, engine, &bilan)
    })?;
    Ok(bilan)
}

/// Le journal : les [`JOURNAL_MAX`] dernières opérations, dans `settings`.
fn journaliser(tx: &dyn DbTxHandle, engine: Engine, bilan: &Bilan) -> Result<(), String> {
    let p1 = marque(engine, 1);
    let p2 = marque(engine, 2);
    let mut journal: Vec<Value> = tx
        .query_one(
            &format!("SELECT value FROM settings WHERE key = {p1}"),
            &[&CLE_JOURNAL as &dyn ToSqlValue],
        )?
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    journal.push(serde_json::json!({
        "at": maintenant,
        "old_prefix": bilan.old_prefix,
        "new_prefix": bilan.new_prefix,
        "tracks": bilan.tracks_updated,
        "columns": bilan.columns,
        "settings": bilan.settings,
    }));
    let debut = journal.len().saturating_sub(JOURNAL_MAX);
    let journal: Vec<_> = journal.into_iter().skip(debut).collect();
    let json = serde_json::to_string(&journal).map_err(|e| e.to_string())?;
    tx.execute(
        &format!(
            "INSERT INTO settings (key, value) VALUES ({p1}, {p2}) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value"
        ),
        &[&CLE_JOURNAL as &dyn ToSqlValue, &json as &dyn ToSqlValue],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "deplacer_racine_tests_8676.rs"]
mod tests;
