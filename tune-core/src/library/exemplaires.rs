//! Les EXEMPLAIRES d'une piste : la même musique dans plusieurs répertoires
//! (#4907).
//!
//! ## Le besoin
//!
//! Un NAS, un disque local, une copie de sauvegarde : la même musique vit
//! souvent à plusieurs endroits. L'utilisateur veut choisir DEPUIS OÙ elle est
//! lue, sans que la bibliothèque affiche de doublons, et que Tune se replie
//! tout seul sur une autre copie quand la préférée ne répond pas (NAS éteint).
//!
//! ## Le modèle
//!
//! Une piste reste UNE ligne `tracks`, avec son identifiant stable : c'est lui
//! que visent playlists, favoris, historique, notes et files d'attente. Ses
//! exemplaires sont :
//!
//! 1. **son propre fichier** (`tracks.file_path`) ;
//! 2. **ses copies à l'identique** (table `track_copies`, migration SQLite 110
//!    / PostgreSQL 073) : un fichier octet pour octet identique, rangé dans
//!    le même album. Le scan l'écartait (`skip_duplicate_audio_hash`) ; il le
//!    rattache désormais à la piste, sans ligne `tracks` de plus — aucune vue,
//!    aucun compteur ne bouge ;
//! 3. **ses sœurs de même clé** : les autres lignes `tracks` du même album,
//!    même disque, même numéro, même titre — la clé de
//!    [`crate::db::track_repo::dedup_display_tracks`]. Ce sont les copies
//!    d'une AUTRE qualité (un FLAC et son MP3). Elles existaient déjà comme
//!    lignes distinctes, masquées à l'affichage (#1362) ; leurs identifiants
//!    sont peut-être déjà dans des playlists, ils ne changent donc pas. Elles
//!    comptent simplement comme exemplaires les unes des autres, ainsi que
//!    leurs propres copies.
//!
//! ## La règle de choix, au moment de jouer
//!
//! Décisions de Bertrand (24/09/2026), dans cet ordre :
//!
//! 1. le **répertoire préféré de l'album**, s'il est posé
//!    (`album_preferred_roots`) ;
//! 2. la **meilleure qualité** ([`crate::library::quality::score_qualite`],
//!    le barème de « Disponible en meilleure qualité » et de
//!    `dedup_display_tracks`) ;
//! 3. à qualité égale, l'**ordre des répertoires** (réglage
//!    [`CLE_ORDRE_DES_REPERTOIRES`], par défaut l'ordre de `music_dirs`) ;
//! 4. **repli** : le premier exemplaire joignable dans cet ordre.
//!
//! Le reste (ReplayGain, DR, balises, historique) se rapporte à la PISTE
//! demandée : les exemplaires d'une même clé sont la même musique, et la
//! ligne demandée est celle que l'utilisateur a choisie.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tracing::{info, warn};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::db::models::Track;
use crate::db::track_repo::InfoFichier;
use crate::metadata::enrich_scope::sous_le_dossier;

/// Réglage : l'ordre des répertoires de musique, liste JSON de chemins.
///
/// Absent ⇒ l'ordre de `music_dirs`. Une entrée qui n'est plus un dossier de
/// musique configuré est ignorée ; un dossier configuré absent de la liste
/// vient après ceux qu'elle classe, dans l'ordre de `music_dirs`.
pub const CLE_ORDRE_DES_REPERTOIRES: &str = "ordre_des_repertoires";

/// `source` des [`InfoFichier`] rendues par [`carte_des_exemplaires`] : ce
/// ne sont PAS des lignes `tracks`, et rien ne doit les purger comme telles.
pub const SOURCE_EXEMPLAIRE: &str = "exemplaire";

fn ph(db: &dyn DbBackend, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

// ─── Répertoires ─────────────────────────────────────────────────────────

/// Les dossiers de musique configurés (`settings['music_dirs']`), normalisés
/// comme le scan les normalise, dans leur ordre.
pub fn dossiers_de_musique(db: &dyn DbBackend) -> Vec<String> {
    let brut: Vec<String> = lire_liste(db, "music_dirs");
    let mut out: Vec<String> = Vec::new();
    for d in brut {
        let d = crate::scanner::walker::normalize_path(&d);
        if !d.is_empty() && !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// L'ordre des répertoires tel que l'utilisateur l'a réglé (brut, normalisé).
pub fn ordre_regle(db: &dyn DbBackend) -> Vec<String> {
    lire_liste(db, CLE_ORDRE_DES_REPERTOIRES)
        .into_iter()
        .map(|d| crate::scanner::walker::normalize_path(&d))
        .filter(|d| !d.is_empty())
        .collect()
}

fn lire_liste(db: &dyn DbBackend, cle: &str) -> Vec<String> {
    // La requête de `SettingsRepo::get`, lue sur un `&dyn DbBackend` : le
    // dépôt veut un `Arc`, que le scan et l'orchestrateur n'ont pas toujours
    // sous la main à cet endroit.
    let sql = format!("SELECT value FROM settings WHERE key = {}", ph(db, 1));
    let params: [&dyn ToSqlValue; 1] = [&cle];
    db.query_one_strong(&sql, &params)
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// L'ordre EFFECTIF : les entrées réglées qui sont encore des dossiers de
/// musique, puis les dossiers que le réglage ne classe pas, dans l'ordre de
/// `music_dirs`. Sans réglage, c'est `music_dirs` tel quel.
pub fn ordre_effectif(dossiers: &[String], regle: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(dossiers.len());
    for r in regle {
        if dossiers.contains(r) && !out.contains(r) {
            out.push(r.clone());
        }
    }
    for d in dossiers {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// [`ordre_effectif`] lu en base.
pub fn racines_ordonnees(db: &dyn DbBackend) -> Vec<String> {
    ordre_effectif(&dossiers_de_musique(db), &ordre_regle(db))
}

/// La racine qui contient `chemin` : la plus longue, pour des dossiers de
/// musique imbriqués. `None` hors de toute racine configurée.
pub fn racine_de(chemin: &str, racines: &[String]) -> Option<String> {
    racines
        .iter()
        .filter(|r| sous_le_dossier(chemin, r))
        .max_by_key(|r| r.len())
        .cloned()
}

/// Les dossiers « miroirs » de `dossier` : le même chemin RELATIF sous chacune
/// des AUTRES racines. `/nas/Musique/Miles Davis/Kind of Blue` sous la racine
/// `/nas/Musique` a pour miroir `/mnt/sauvegarde/Miles Davis/Kind of Blue`
/// quand `/mnt/sauvegarde` est aussi un dossier de musique.
///
/// C'est ce qui fait d'une copie de sauvegarde le MÊME album et non une
/// parution de plus : le dossier est l'identité d'un album
/// ([`crate::scanner::album_folder`]), mais un dossier recopié tel quel sous
/// une autre racine est la même parution. Sans cette règle, chaque copie
/// d'une racine à l'autre fabriquait un album en double dans toutes les vues.
pub fn dossiers_miroirs(dossier: &str, racines: &[String]) -> Vec<String> {
    let Some(racine) = racine_de(dossier, racines) else {
        return Vec::new();
    };
    let relatif = &dossier[racine.trim_end_matches(['/', '\\']).len()..];
    if relatif.is_empty() {
        // La racine elle-même n'est le miroir de rien.
        return Vec::new();
    }
    racines
        .iter()
        .filter(|r| **r != racine)
        .map(|r| format!("{}{}", r.trim_end_matches(['/', '\\']), relatif))
        .collect()
}

// ─── Exemplaires d'une piste ─────────────────────────────────────────────

/// Un fichier qui peut jouer une piste.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Exemplaire {
    /// La ligne `tracks` qui porte ce fichier ou dont il est la copie.
    pub track_id: i64,
    /// Le chemin tel que la base l'enregistre.
    pub chemin: String,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub file_size: Option<i64>,
    /// Vrai pour une copie à l'identique (`track_copies`), faux pour le
    /// fichier propre d'une ligne `tracks`.
    pub copie: bool,
}

impl Exemplaire {
    fn score(&self) -> (bool, i64) {
        crate::library::quality::score_qualite(
            self.format.as_deref(),
            self.sample_rate.map(i64::from),
            self.bit_depth.map(i64::from),
        )
    }
}

/// Range les exemplaires dans l'ordre de la règle : répertoire préféré de
/// l'album, puis meilleure qualité, puis ordre des répertoires. À égalité
/// complète, la ligne demandée passe devant ses sœurs et un fichier propre
/// devant une copie — rien ne bouge pour une bibliothèque sans exemplaire.
pub fn ordonner(
    mut exemplaires: Vec<Exemplaire>,
    racines: &[String],
    prefere: Option<&str>,
    piste_demandee: i64,
) -> Vec<Exemplaire> {
    let rang = |e: &Exemplaire| {
        racine_de(&e.chemin, racines)
            .and_then(|r| racines.iter().position(|x| *x == r))
            .unwrap_or(usize::MAX)
    };
    let est_prefere = |e: &Exemplaire| {
        prefere.is_some_and(|p| racine_de(&e.chemin, racines).as_deref() == Some(p))
    };
    exemplaires.sort_by(|a, b| {
        est_prefere(b)
            .cmp(&est_prefere(a))
            .then_with(|| b.score().cmp(&a.score()))
            .then_with(|| rang(a).cmp(&rang(b)))
            .then_with(|| (a.track_id != piste_demandee).cmp(&(b.track_id != piste_demandee)))
            .then_with(|| a.copie.cmp(&b.copie))
            .then_with(|| a.chemin.cmp(&b.chemin))
    });
    exemplaires
}

/// L'exemplaire retenu pour la lecture.
#[derive(Debug, Clone, PartialEq)]
pub struct Choix {
    pub exemplaire: Exemplaire,
    /// Le chemin tel qu'il existe sur le disque (graphie NFC/NFD résolue).
    pub chemin_resolu: String,
    /// Les exemplaires mieux placés, sautés parce qu'injoignables.
    pub ecartes: Vec<String>,
}

/// Le premier exemplaire JOIGNABLE dans l'ordre donné — le repli est là :
/// un exemplaire préféré injoignable cède sa place au suivant.
pub fn choisir(
    ordonnes: &[Exemplaire],
    resoudre: impl Fn(&str) -> Option<String>,
) -> Option<Choix> {
    let mut ecartes = Vec::new();
    for e in ordonnes {
        match resoudre(&e.chemin) {
            Some(chemin_resolu) => {
                return Some(Choix {
                    exemplaire: e.clone(),
                    chemin_resolu,
                    ecartes,
                });
            }
            None => ecartes.push(e.chemin.clone()),
        }
    }
    None
}

fn cle_de_presentation(disque: i64, numero: i64, titre: &str) -> (i64, i64, String) {
    (disque, numero, titre.trim().to_lowercase())
}

fn exemplaire_de_ligne(r: &[SqlValue], copie: bool) -> Option<Exemplaire> {
    Some(Exemplaire {
        track_id: r.first()?.as_i64()?,
        chemin: r.get(1)?.as_string()?,
        format: r.get(2).and_then(|v| v.as_string()),
        sample_rate: r.get(3).and_then(|v| v.as_i64()).map(|v| v as i32),
        bit_depth: r.get(4).and_then(|v| v.as_i64()).map(|v| v as i32),
        file_size: r.get(5).and_then(|v| v.as_i64()),
        copie,
    })
}

/// Tous les exemplaires d'une piste : son fichier, ses sœurs de même clé,
/// et les copies à l'identique des unes et des autres.
pub fn exemplaires_de_la_piste(
    db: &dyn DbBackend,
    piste: &Track,
) -> Result<Vec<Exemplaire>, String> {
    let Some(id) = piste.id else {
        return Ok(Vec::new());
    };
    let mut lignes: Vec<Exemplaire> = Vec::new();
    if let Some(chemin) = piste.file_path.clone() {
        lignes.push(Exemplaire {
            track_id: id,
            chemin,
            format: piste.format.clone(),
            sample_rate: piste.sample_rate,
            bit_depth: piste.bit_depth,
            file_size: piste.file_size,
            copie: false,
        });
    }
    if let Some(album_id) = piste.album_id {
        let cle = cle_de_presentation(
            i64::from(piste.disc_number),
            i64::from(piste.track_number),
            &piste.title,
        );
        let sql = format!(
            "SELECT id, file_path, format, sample_rate, bit_depth, file_size, \
             title, disc_number, track_number FROM tracks \
             WHERE album_id = {} AND id <> {} AND file_path IS NOT NULL \
             AND cue_media_path IS NULL",
            ph(db, 1),
            ph(db, 2)
        );
        let params: [&dyn ToSqlValue; 2] = [&album_id, &id];
        for r in db.query_many(&sql, &params)? {
            let titre = r.get(6).and_then(|v| v.as_string()).unwrap_or_default();
            let disque = r.get(7).and_then(|v| v.as_i64()).unwrap_or(1);
            let numero = r.get(8).and_then(|v| v.as_i64()).unwrap_or(0);
            if cle_de_presentation(disque, numero, &titre) != cle {
                continue;
            }
            if let Some(e) = exemplaire_de_ligne(&r, false) {
                lignes.push(e);
            }
        }
    }
    let ids: Vec<String> = lignes.iter().map(|e| e.track_id.to_string()).collect();
    if !ids.is_empty() {
        let sql = format!(
            "SELECT track_id, file_path, format, sample_rate, bit_depth, file_size \
             FROM track_copies WHERE track_id IN ({}) ORDER BY id",
            ids.join(",")
        );
        for r in db.query_many(&sql, &[])? {
            if let Some(e) = exemplaire_de_ligne(&r, true) {
                lignes.push(e);
            }
        }
    }
    Ok(lignes)
}

// ─── Préférence par album ────────────────────────────────────────────────

/// Le répertoire préféré d'un album, s'il est posé.
pub fn racine_preferee(db: &dyn DbBackend, album_id: i64) -> Option<String> {
    let sql = format!(
        "SELECT root FROM album_preferred_roots WHERE album_id = {}",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&album_id];
    db.query_one(&sql, &params)
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
}

/// Pose (ou remplace) le répertoire préféré d'un album.
pub fn poser_racine_preferee(
    db: &dyn DbBackend,
    album_id: i64,
    racine: &str,
) -> Result<(), String> {
    let sql = format!(
        "INSERT INTO album_preferred_roots (album_id, root) VALUES ({}, {}) \
         ON CONFLICT (album_id) DO UPDATE SET root = excluded.root",
        ph(db, 1),
        ph(db, 2)
    );
    let racine = racine.to_string();
    let params: [&dyn ToSqlValue; 2] = [&album_id, &racine];
    db.execute(&sql, &params).map(|_| ())
}

/// Retire la préférence : l'album revient à la règle par défaut. Rend vrai si
/// une préférence existait.
pub fn retirer_racine_preferee(db: &dyn DbBackend, album_id: i64) -> Result<bool, String> {
    let sql = format!(
        "DELETE FROM album_preferred_roots WHERE album_id = {}",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&album_id];
    db.execute(&sql, &params).map(|n| n > 0)
}

// ─── Lecture ─────────────────────────────────────────────────────────────

/// Ce que la dernière lecture d'une piste a réellement ouvert — lu par le
/// chemin du signal (`routes/zones/signal_path.rs`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExemplaireLu {
    pub chemin: String,
    pub racine: Option<String>,
    /// Vrai quand un exemplaire mieux placé était injoignable.
    pub repli: bool,
    /// Nombre d'exemplaires connus de la piste.
    pub exemplaires: usize,
}

fn registre() -> &'static Mutex<HashMap<i64, ExemplaireLu>> {
    static R: OnceLock<Mutex<HashMap<i64, ExemplaireLu>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// L'exemplaire que la dernière lecture de `track_id` a ouvert, si la piste
/// en a plusieurs.
pub fn exemplaire_lu(track_id: i64) -> Option<ExemplaireLu> {
    registre().lock().ok()?.get(&track_id).cloned()
}

/// Choisit le fichier à lire pour `piste` et l'y écrit (`file_path`, format,
/// fréquence, profondeur, taille). Ne touche à RIEN quand la piste n'a qu'un
/// exemplaire, ou quand aucun n'est joignable — l'appelant rend alors son
/// erreur habituelle (`file_not_found:`).
pub fn appliquer_a_la_lecture(db: &dyn DbBackend, piste: &mut Track) -> Option<Choix> {
    let track_id = piste.id?;
    if piste.bornes_cue().is_some() {
        // Une tranche de feuille CUE vit dans SON fichier image.
        return None;
    }
    let exemplaires = match exemplaires_de_la_piste(db, piste) {
        Ok(e) => e,
        Err(e) => {
            warn!(track_id, error = %e, "exemplaires_lecture_impossible");
            return None;
        }
    };
    if exemplaires.len() <= 1 {
        return None;
    }
    let nombre = exemplaires.len();
    let racines = racines_ordonnees(db);
    let prefere = piste.album_id.and_then(|a| racine_preferee(db, a));
    let ordonnes = ordonner(exemplaires, &racines, prefere.as_deref(), track_id);
    let Some(choix) = choisir(
        &ordonnes,
        crate::library::local_path::resolve_existing_local_path,
    ) else {
        warn!(
            track_id,
            exemplaires = nombre,
            "exemplaires_tous_injoignables — aucun exemplaire de cette piste ne répond"
        );
        return None;
    };
    let racine = racine_de(&choix.exemplaire.chemin, &racines);
    if choix.ecartes.is_empty() {
        info!(
            track_id,
            exemplaire = %choix.exemplaire.chemin,
            racine = ?racine,
            racine_preferee = ?prefere,
            exemplaires = nombre,
            "exemplaire_choisi"
        );
    } else {
        warn!(
            track_id,
            exemplaire = %choix.exemplaire.chemin,
            racine = ?racine,
            injoignables = ?choix.ecartes,
            exemplaires = nombre,
            "exemplaire_repli — l'exemplaire préféré ne répond pas, lecture depuis le suivant"
        );
    }
    let e = &choix.exemplaire;
    piste.file_path = Some(e.chemin.clone());
    piste.format = e.format.clone().or_else(|| piste.format.clone());
    piste.sample_rate = e.sample_rate.or(piste.sample_rate);
    piste.bit_depth = e.bit_depth.or(piste.bit_depth);
    piste.file_size = e.file_size.or(piste.file_size);
    if let Ok(mut r) = registre().lock() {
        r.insert(
            track_id,
            ExemplaireLu {
                chemin: e.chemin.clone(),
                racine,
                repli: !choix.ecartes.is_empty(),
                exemplaires: nombre,
            },
        );
    }
    Some(choix)
}

// ─── Album ───────────────────────────────────────────────────────────────

/// Un exemplaire d'ALBUM : ce qu'un répertoire racine en possède.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExemplaireDAlbum {
    /// Le dossier de musique qui porte ces fichiers ; `null` hors de toute
    /// racine configurée.
    pub racine: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    /// Nombre de fichiers de l'album sous cette racine.
    pub pistes: usize,
    /// La racine répond-elle (dossier présent) ?
    pub joignable: bool,
    /// Est-ce le répertoire préféré de l'album ?
    pub prefere: bool,
}

/// Les exemplaires d'un album, un par répertoire racine, dans l'ordre des
/// répertoires.
pub fn exemplaires_de_l_album(
    db: &dyn DbBackend,
    album_id: i64,
) -> Result<Vec<ExemplaireDAlbum>, String> {
    let sql = format!(
        "SELECT id, file_path, format, sample_rate, bit_depth, file_size FROM tracks \
         WHERE album_id = {} AND file_path IS NOT NULL AND cue_media_path IS NULL",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&album_id];
    let mut fichiers: Vec<Exemplaire> = db
        .query_many(&sql, &params)?
        .iter()
        .filter_map(|r| exemplaire_de_ligne(r, false))
        .collect();
    let sql = format!(
        "SELECT c.track_id, c.file_path, c.format, c.sample_rate, c.bit_depth, c.file_size \
         FROM track_copies c JOIN tracks t ON t.id = c.track_id WHERE t.album_id = {}",
        ph(db, 1)
    );
    fichiers.extend(
        db.query_many(&sql, &params)?
            .iter()
            .filter_map(|r| exemplaire_de_ligne(r, true)),
    );
    let racines = racines_ordonnees(db);
    let prefere = racine_preferee(db, album_id);
    let mut groupes: Vec<(Option<String>, Vec<Exemplaire>)> = Vec::new();
    for f in fichiers {
        let r = racine_de(&f.chemin, &racines);
        match groupes.iter_mut().find(|(g, _)| *g == r) {
            Some((_, v)) => v.push(f),
            None => groupes.push((r, vec![f])),
        }
    }
    let rang = |r: &Option<String>| {
        r.as_ref()
            .and_then(|r| racines.iter().position(|x| x == r))
            .unwrap_or(usize::MAX)
    };
    groupes.sort_by_key(|(r, _)| rang(r));
    Ok(groupes
        .into_iter()
        .map(|(racine, v)| {
            let meilleur = v.iter().max_by_key(|e| e.score()).cloned();
            let joignable = match &racine {
                Some(r) => std::path::Path::new(r).is_dir(),
                None => v.iter().any(|e| {
                    crate::library::local_path::resolve_existing_local_path(&e.chemin).is_some()
                }),
            };
            ExemplaireDAlbum {
                prefere: racine.is_some() && racine == prefere,
                format: meilleur.as_ref().and_then(|e| e.format.clone()),
                sample_rate: meilleur.as_ref().and_then(|e| e.sample_rate),
                bit_depth: meilleur.as_ref().and_then(|e| e.bit_depth),
                pistes: v.len(),
                joignable,
                racine,
            }
        })
        .collect())
}

// ─── Scan ────────────────────────────────────────────────────────────────

/// Une copie à l'identique rencontrée par le scan, à rattacher à la piste
/// qui possède `chemin_proprietaire`.
#[derive(Debug, Clone, PartialEq)]
pub struct NouvelExemplaire {
    pub chemin_proprietaire: String,
    pub chemin: String,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub file_size: Option<i64>,
    pub file_mtime: Option<f64>,
    pub audio_hash: Option<String>,
}

impl NouvelExemplaire {
    /// La copie décrite par la piste que l'importateur vient d'en tirer.
    pub fn depuis_la_piste(chemin_proprietaire: &str, piste: &Track) -> Option<Self> {
        Some(Self {
            chemin_proprietaire: chemin_proprietaire.to_string(),
            chemin: piste.file_path.clone()?,
            format: piste.format.clone(),
            sample_rate: piste.sample_rate,
            bit_depth: piste.bit_depth,
            file_size: piste.file_size,
            file_mtime: piste.file_mtime,
            audio_hash: piste.audio_hash.clone(),
        })
    }
}

/// Rattache chaque copie à la piste qui possède son `chemin_proprietaire`.
/// Lectures « fortes » : le scan écrit dans une transaction, et le
/// propriétaire vient souvent d'être inséré par ce même lot. Rend le nombre
/// d'exemplaires écrits ; un échec se journalise et n'arrête rien.
pub fn rattacher(db: &dyn DbBackend, nouveaux: &[NouvelExemplaire]) -> usize {
    if nouveaux.is_empty() {
        return 0;
    }
    let cherche = format!("SELECT id FROM tracks WHERE file_path = {}", ph(db, 1));
    let ecrit = format!(
        "INSERT INTO track_copies \
         (track_id, file_path, format, sample_rate, bit_depth, file_size, file_mtime, audio_hash) \
         VALUES ({}, {}, {}, {}, {}, {}, {}, {}) \
         ON CONFLICT (file_path) DO UPDATE SET track_id = excluded.track_id, \
         format = excluded.format, sample_rate = excluded.sample_rate, \
         bit_depth = excluded.bit_depth, file_size = excluded.file_size, \
         file_mtime = excluded.file_mtime, audio_hash = excluded.audio_hash",
        ph(db, 1),
        ph(db, 2),
        ph(db, 3),
        ph(db, 4),
        ph(db, 5),
        ph(db, 6),
        ph(db, 7),
        ph(db, 8)
    );
    let mut ecrits = 0;
    for n in nouveaux {
        let params: [&dyn ToSqlValue; 1] = [&n.chemin_proprietaire];
        let proprietaire = db
            .query_one_strong(&cherche, &params)
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()));
        let Some(track_id) = proprietaire else {
            warn!(
                chemin = %n.chemin,
                proprietaire = %n.chemin_proprietaire,
                "exemplaire_sans_proprietaire — la piste identique n'est pas en base"
            );
            continue;
        };
        let params: [&dyn ToSqlValue; 8] = [
            &track_id,
            &n.chemin,
            &n.format,
            &n.sample_rate,
            &n.bit_depth,
            &n.file_size,
            &n.file_mtime,
            &n.audio_hash,
        ];
        match db.execute(&ecrit, &params) {
            Ok(_) => {
                ecrits += 1;
                info!(
                    track_id,
                    chemin = %n.chemin,
                    proprietaire = %n.chemin_proprietaire,
                    "exemplaire_rattache"
                );
            }
            Err(e) => warn!(chemin = %n.chemin, error = %e, "exemplaire_rattachement_echec"),
        }
    }
    ecrits
}

/// `file_path` → ce que la base sait de chaque copie à l'identique, dans la
/// forme de la carte des pistes (`id` = la piste propriétaire,
/// `source` = [`SOURCE_EXEMPLAIRE`]). Lecture forte.
pub fn carte_des_exemplaires(db: &dyn DbBackend) -> Result<HashMap<String, InfoFichier>, String> {
    let lignes = db.query_many_strong(
        "SELECT file_path, track_id, file_mtime, file_size FROM track_copies",
        &[],
    )?;
    Ok(lignes
        .into_iter()
        .filter_map(|r| {
            Some((
                r.first()?.as_string()?,
                InfoFichier {
                    id: r.get(1)?.as_i64()?,
                    mtime: r.get(2).and_then(|v| v.as_f64()),
                    taille: r.get(3).and_then(|v| v.as_i64()),
                    source: SOURCE_EXEMPLAIRE.to_string(),
                },
            ))
        })
        .collect())
}

/// Retire des exemplaires. Rend le nombre de lignes retirées.
pub fn retirer_des_exemplaires(db: &dyn DbBackend, chemins: &[String]) -> usize {
    let sql = format!("DELETE FROM track_copies WHERE file_path = {}", ph(db, 1));
    chemins
        .iter()
        .map(|c| {
            let params: [&dyn ToSqlValue; 1] = [c];
            db.execute(&sql, &params).unwrap_or_else(|e| {
                warn!(chemin = %c, error = %e, "exemplaire_retrait_echec");
                0
            })
        })
        .sum()
}

/// Filet : une copie dont la piste n'existe plus. La clé étrangère
/// `ON DELETE CASCADE` les retire d'ordinaire ; celles qui survivraient
/// cacheraient leur fichier au scan (il se croirait déjà connu).
pub fn nettoyer_les_orphelins(db: &dyn DbBackend) -> usize {
    db.execute(
        "DELETE FROM track_copies WHERE track_id NOT IN (SELECT id FROM tracks)",
        &[],
    )
    .unwrap_or(0)
}

/// Le fichier propre de la piste `track_id` a disparu : un de ses exemplaires
/// joignables prend sa place, et la piste GARDE son identifiant. Rend le
/// nouveau chemin, ou `None` quand aucune copie ne répond (la piste part
/// alors comme avant).
pub fn promouvoir(db: &dyn DbBackend, track_id: i64) -> Result<Option<String>, String> {
    let sql = format!(
        "SELECT track_id, file_path, format, sample_rate, bit_depth, file_size, file_mtime \
         FROM track_copies WHERE track_id = {} ORDER BY id",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&track_id];
    let lignes = db.query_many_strong(&sql, &params)?;
    let mtimes: HashMap<String, Option<f64>> = lignes
        .iter()
        .filter_map(|r| Some((r.get(1)?.as_string()?, r.get(6).and_then(|v| v.as_f64()))))
        .collect();
    let copies: Vec<Exemplaire> = lignes
        .iter()
        .filter_map(|r| exemplaire_de_ligne(r, true))
        .collect();
    let racines = racines_ordonnees(db);
    let ordonnes = ordonner(copies, &racines, None, track_id);
    let Some(choix) = choisir(
        &ordonnes,
        crate::library::local_path::resolve_existing_local_path,
    ) else {
        return Ok(None);
    };
    let ancien = db
        .query_one_strong(
            &format!("SELECT file_path FROM tracks WHERE id = {}", ph(db, 1)),
            &params,
        )?
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    let nouveau = choix.exemplaire.chemin.clone();
    let mtime = mtimes.get(&nouveau).copied().flatten();
    let taille = choix.exemplaire.file_size;
    let maj = format!(
        "UPDATE tracks SET file_path = {}, file_mtime = {}, file_size = {} WHERE id = {}",
        ph(db, 1),
        ph(db, 2),
        ph(db, 3),
        ph(db, 4)
    );
    let retrait = format!("DELETE FROM track_copies WHERE file_path = {}", ph(db, 1));
    // La date d'ajout suit la piste, pas son chemin (#4546) : sans cette
    // recopie, l'album remonterait dans « Ajoutés récemment ».
    let date = match db.engine() {
        Engine::Sqlite => "INSERT OR IGNORE INTO file_first_seen (file_path, first_seen_at) \
             SELECT ?, first_seen_at FROM file_first_seen WHERE file_path = ?"
            .to_string(),
        Engine::Postgres => "INSERT INTO file_first_seen (file_path, first_seen_at) \
             SELECT CAST($1 AS TEXT), first_seen_at FROM file_first_seen WHERE file_path = $2 \
             ON CONFLICT (file_path) DO NOTHING"
            .to_string(),
    };
    // Trois écritures SANS `write_tx` : le scan appelle ceci entre ses propres
    // transactions, et un `BEGIN` imbriqué échouerait. L'ordre borne le pire
    // cas : si la mise à jour échoue, rien n'a bougé ; si le retrait de la
    // copie échoue ensuite, la piste possède déjà le fichier et la ligne de
    // copie en double est sans effet (même chemin, même piste).
    let p: [&dyn ToSqlValue; 4] = [&nouveau, &mtime, &taille, &track_id];
    db.execute(&maj, &p)?;
    let p: [&dyn ToSqlValue; 1] = [&nouveau];
    if let Err(e) = db.execute(&retrait, &p) {
        warn!(track_id, chemin = %nouveau, error = %e, "exemplaire_promu_copie_non_retiree");
    }
    if let Some(ancien) = &ancien {
        let p: [&dyn ToSqlValue; 2] = [&nouveau, ancien];
        if let Err(e) = db.execute(&date, &p) {
            warn!(track_id, error = %e, "exemplaire_promu_date_d_ajout_non_recopiee");
        }
    }
    info!(
        track_id,
        ancien = ?ancien,
        nouveau = %nouveau,
        "exemplaire_promu — le fichier de la piste a disparu, une copie prend sa place"
    );
    Ok(Some(nouveau))
}

/// Ce que le surveillant de fichiers doit faire d'un fichier disparu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetraitDuFichier {
    /// C'était une copie : elle est retirée, la piste reste.
    ExemplaireRetire,
    /// C'était le fichier d'une piste qui a une copie joignable : la copie
    /// prend sa place, la piste garde son identifiant.
    Promu(String),
    /// Rien à voir avec les exemplaires : le retrait ordinaire s'applique.
    Aucun,
}

/// Retrait d'UN fichier disparu, pour le surveillant de fichiers.
pub fn retirer_le_fichier(db: &dyn DbBackend, chemin: &str) -> RetraitDuFichier {
    if retirer_des_exemplaires(db, &[chemin.to_string()]) > 0 {
        return RetraitDuFichier::ExemplaireRetire;
    }
    let sql = format!("SELECT id FROM tracks WHERE file_path = {}", ph(db, 1));
    let params: [&dyn ToSqlValue; 1] = [&chemin];
    let Some(track_id) = db
        .query_one_strong(&sql, &params)
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
    else {
        return RetraitDuFichier::Aucun;
    };
    match promouvoir(db, track_id) {
        Ok(Some(nouveau)) => RetraitDuFichier::Promu(nouveau),
        Ok(None) => RetraitDuFichier::Aucun,
        Err(e) => {
            warn!(track_id, error = %e, "exemplaire_promotion_echec");
            RetraitDuFichier::Aucun
        }
    }
}

#[cfg(test)]
#[path = "exemplaires_tests.rs"]
mod tests;
