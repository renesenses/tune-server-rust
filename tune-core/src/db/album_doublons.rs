//! La fusion des albums en double — UNE logique pour les trois chemins qui
//! fusionnent (reste de #5005, décision de Bertrand du 26/09/2026).
//!
//! | déclencheur | route ou moment | plafond |
//! |---|---|---|
//! | [`Declencheur::Manuel`] | `POST /library/albums/merge-duplicates` | aucun : l'utilisateur l'a demandé, la réponse compte ce qui est fait |
//! | [`Declencheur::FinDeScan`] | fin de scan (`routes/system/scan.rs`, #593) | [`PLAFOND_AUTOMATIQUE`] |
//! | [`Declencheur::Nettoyage`] | `POST /system/cleanup` | [`PLAFOND_AUTOMATIQUE`] |
//!
//! # Ce que les trois copies d'avant faisaient mal
//!
//! 1. La fin de scan et le nettoyage agrégeaient par `GROUP_CONCAT(id)` écrit
//!    en dur : PostgreSQL répond « function group_concat(bigint) does not
//!    exist », l'erreur était avalée (`ou_defaut_journalise`) — ces deux
//!    fusions ne tournaient JAMAIS sur PostgreSQL.
//! 2. Elles ignoraient les paires déclarées distinctes (#1276), que seule la
//!    route manuelle respectait. Les réveiller telles quelles aurait fusionné
//!    des albums que l'utilisateur avait séparés.
//! 3. Les trois ne déplaçaient que `tracks` : favoris, étiquettes, notes,
//!    historique d'écoute, dossiers et métadonnées du perdant mouraient avec
//!    sa ligne. On passe désormais par [`AlbumRepo::absorber`] (BIB-A2), qui
//!    repointe tout.
//! 4. La route manuelle regroupait par `LOWER(title)` SEUL : deux « Greatest
//!    Hits » d'artistes différents étaient fusionnés. Le critère commun est
//!    celui des deux chemins automatiques, `(LOWER(title), artist_id)`.
//!
//! # Les gardes, dans l'ordre
//!
//! * une paire déclarée distincte n'est JAMAIS fusionnée — ni avec l'album
//!   conservé, ni avec un album déjà absorbé par lui (sinon A, B, C avec B≠C
//!   déclarés réuniraient B et C sous A) ;
//! * deux albums identifiés sous deux releases MusicBrainz différentes ne sont
//!   pas fusionnés : l'identification les a déjà dits distincts ;
//! * avec « Split albums by quality » (`quality_split`, actif par défaut),
//!   deux éditions que l'IMPORT a séparées ne sont pas réunies (#7839) :
//!   qualités connues sans rien en commun (DSD et CD, #7694), années connues
//!   différentes, ou dossiers connus différents qui ne sont pas frères (deux
//!   extractions distinctes). Les dossiers frères restent fusionnables : des
//!   disques aux noms libres (« Nocturnes/Le jour », « Nocturnes/La nuit »)
//!   ou un coffret `CD01/CD02` sont un seul album. Les éditions laissées à
//!   part forment leur propre groupe : leurs vrais doublons fusionnent entre
//!   eux. Réglage coupé, rien ne change ;
//! * un album tenu par une édition manuelle (`album_metadata.edition_manuelle`,
//!   C3) n'est jamais le PERDANT : il devient l'album conservé. Deux albums
//!   édités à la main dans le même groupe : le groupe est laissé tel quel ;
//! * en automatique, au-delà de [`PLAFOND_AUTOMATIQUE`] fusions prévues, rien
//!   n'est fusionné et un avertissement le dit : une base PostgreSQL où ces
//!   fusions dormaient depuis toujours ne se réorganise pas d'un coup, en
//!   silence, au premier scan. La route manuelle reste là pour le faire.
//!
//! Chaque absorption est journalisée (`album_doublon_fusionne`, avec le
//! déclencheur et le titre), en plus du bilan `album_absorbe` d'`absorber`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use super::absorption::table_absente;
use super::album_distinct_repo::{AlbumDistinctRepo, DistinctPairSet};
use super::album_metadata_repo::AlbumMetadataRepo;
use super::album_repo::AlbumRepo;
use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::settings_repo::SettingsRepo;
use crate::TuneError;

/// Au-delà de ce nombre de fusions prévues, un chemin AUTOMATIQUE s'abstient.
pub const PLAFOND_AUTOMATIQUE: usize = 25;

/// Qui demande la fusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declencheur {
    /// `POST /library/albums/merge-duplicates` : le geste de l'utilisateur.
    Manuel,
    /// La fin de chaque scan (#593).
    FinDeScan,
    /// `POST /system/cleanup`.
    Nettoyage,
}

impl Declencheur {
    pub fn nom(self) -> &'static str {
        match self {
            Declencheur::Manuel => "manuel",
            Declencheur::FinDeScan => "fin_de_scan",
            Declencheur::Nettoyage => "nettoyage",
        }
    }

    fn plafond(self) -> Option<usize> {
        match self {
            Declencheur::Manuel => None,
            Declencheur::FinDeScan | Declencheur::Nettoyage => Some(PLAFOND_AUTOMATIQUE),
        }
    }
}

/// Le bilan d'une passe de fusion.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BilanDesDoublons {
    /// Groupes `(LOWER(title), artist_id)` de plus d'un album.
    pub groupes: usize,
    /// Albums absorbés (lignes supprimées, tout repointé vers le conservé).
    pub fusionnes: usize,
    /// Albums laissés à part : paire déclarée distincte (#1276).
    pub proteges: usize,
    /// Albums laissés à part : identifiés sous une autre release MusicBrainz.
    pub identifies_ailleurs: usize,
    /// Albums laissés à part : groupe à plusieurs éditions manuelles.
    pub edites_a_la_main: usize,
    /// Éditions gardées en plus de la première : séparées à l'import
    /// (qualité, année ou dossier, avec `quality_split`) — #7839.
    pub editions_separees: usize,
    /// Fusions prévues mais non faites : plafond automatique dépassé.
    pub suspendus: usize,
    /// Absorptions tentées qui ont échoué (journalisées).
    pub echecs: usize,
}

/// Un membre de groupe, avec ce qu'il faut pour choisir et garder.
#[derive(Debug, Clone)]
struct Membre {
    id: i64,
    pistes: i64,
    release: Option<String>,
    edite: bool,
    /// `albums.folder_path` : le dossier qui identifie l'édition à l'import.
    dossier: Option<String>,
    annee: Option<i64>,
    /// Les scores [`score_qualite`](crate::library::quality::score_qualite)
    /// de ses pistes dont la qualité est connue. Vide sans `quality_split`.
    qualites: BTreeSet<(bool, i64)>,
}

/// Une fusion décidée : `cible` absorbe `doublon`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fusion {
    cible: i64,
    doublon: i64,
    titre: String,
}

pub struct FusionDesDoublons {
    db: Arc<dyn DbBackend>,
}

fn marque(db: &dyn DbBackend, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// La requête des groupes, portable : l'agrégat passe par le dialecte
/// (`GROUP_CONCAT` / `STRING_AGG`, qui exige du texte — d'où le `CAST`).
pub fn sql_groupes(engine: Engine) -> String {
    let agregat = match engine {
        Engine::Sqlite => SqliteDialect.group_concat("CAST(id AS TEXT)", ","),
        Engine::Postgres => PostgresDialect.group_concat("CAST(id AS TEXT)", ","),
    };
    format!(
        "SELECT MIN(title), {agregat} FROM albums WHERE source = 'local' \
         GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1"
    )
}

impl FusionDesDoublons {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    /// Cherche les groupes, décide, puis fusionne — ou s'abstient.
    ///
    /// Une erreur de LECTURE (groupes, paires distinctes) rend `Err` : rien
    /// n'est fusionné sans savoir ce que l'utilisateur a protégé. Une
    /// absorption qui échoue est journalisée et comptée, la passe continue.
    pub fn fusionner(&self, declencheur: Declencheur) -> Result<BilanDesDoublons, TuneError> {
        let mut bilan = BilanDesDoublons::default();
        let groupes = self.groupes()?;
        bilan.groupes = groupes.len();
        if groupes.is_empty() {
            return Ok(bilan);
        }
        let distinctes = self.paires_distinctes()?;
        let quality_split = self.quality_split();

        let mut plan: Vec<Fusion> = Vec::new();
        for (titre, ids) in &groupes {
            let membres = self.membres(ids, quality_split)?;
            self.decider(
                titre,
                &membres,
                &distinctes,
                quality_split,
                &mut plan,
                &mut bilan,
            );
        }

        if let Some(plafond) = declencheur.plafond()
            && plan.len() > plafond
        {
            bilan.suspendus = plan.len();
            tracing::warn!(
                declencheur = declencheur.nom(),
                prevues = plan.len(),
                plafond,
                "album_fusion_auto_suspendue — trop d'albums en double pour une fusion \
                 automatique : rien n'est fusionné. « Fusionner les doublons » \
                 (POST /library/albums/merge-duplicates) le fait à la demande."
            );
            return Ok(bilan);
        }

        let repo = AlbumRepo::with_backend(self.db.clone());
        for f in &plan {
            match repo.absorber(f.cible, f.doublon) {
                Ok(rapport) => {
                    bilan.fusionnes += 1;
                    tracing::info!(
                        declencheur = declencheur.nom(),
                        titre = %f.titre,
                        conserve = f.cible,
                        absorbe = f.doublon,
                        pistes = rapport.pistes,
                        marqueurs = rapport.marqueurs,
                        "album_doublon_fusionne"
                    );
                }
                Err(e) => {
                    bilan.echecs += 1;
                    tracing::warn!(
                        declencheur = declencheur.nom(),
                        conserve = f.cible,
                        absorbe = f.doublon,
                        error = %e,
                        "album_doublon_fusion_echouee"
                    );
                }
            }
        }
        // Un album né de la fusion de dossiers frères sans DISCNUMBER montrait
        // toutes ses pistes au disque 1, numéros en double : un disque par
        // dossier, en base seulement ([`super::disques_par_dossier`]).
        if bilan.fusionnes > 0 {
            super::disques_par_dossier::passe_journalisee(&self.db, declencheur.nom());
        }
        if bilan.fusionnes > 0 || bilan.proteges > 0 || bilan.suspendus > 0 {
            tracing::info!(
                declencheur = declencheur.nom(),
                groupes = bilan.groupes,
                fusionnes = bilan.fusionnes,
                proteges = bilan.proteges,
                identifies_ailleurs = bilan.identifies_ailleurs,
                edites_a_la_main = bilan.edites_a_la_main,
                editions_separees = bilan.editions_separees,
                echecs = bilan.echecs,
                "albums_doublons_bilan"
            );
        }
        Ok(bilan)
    }

    /// Les groupes `(titre affiché, ids triés)`.
    fn groupes(&self) -> Result<Vec<(String, Vec<i64>)>, TuneError> {
        let lignes = self
            .db
            .query_many(&sql_groupes(self.db.engine()), &[])
            .map_err(|e| TuneError::from(format!("albums en double illisibles : {e}")))?;
        Ok(lignes
            .into_iter()
            .filter_map(|r| {
                let titre = r.first().and_then(|v| v.as_string()).unwrap_or_default();
                let mut ids: Vec<i64> = r
                    .get(1)?
                    .as_string()?
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                (ids.len() > 1).then_some((titre, ids))
            })
            .collect())
    }

    /// Les paires déclarées distinctes. Une table absente (base ancienne) vaut
    /// « aucune paire » ; toute autre erreur arrête la fusion.
    fn paires_distinctes(&self) -> Result<DistinctPairSet, TuneError> {
        match AlbumDistinctRepo::with_backend(self.db.clone()).charger_ensemble() {
            Ok(p) => Ok(p),
            Err(e) if table_absente(&e) => Ok(DistinctPairSet::default()),
            Err(e) => Err(TuneError::from(format!(
                "paires d'albums distinctes illisibles, fusion abandonnée : {e}"
            ))),
        }
    }

    /// « Split albums by quality », lu comme l'import le lit
    /// (`routes/system/scan.rs`) : actif sauf `false` ou `0`, actif par défaut.
    fn quality_split(&self) -> bool {
        SettingsRepo::with_backend(self.db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true)
    }

    fn membres(&self, ids: &[i64], quality_split: bool) -> Result<Vec<Membre>, TuneError> {
        let p1 = marque(&*self.db, 1);
        let sql_pistes = format!("SELECT COUNT(id) FROM tracks WHERE album_id = {p1}");
        let sql_release =
            format!("SELECT musicbrainz_release_id, folder_path, year FROM albums WHERE id = {p1}");
        let sql_qualites = match self.db.engine() {
            Engine::Sqlite => {
                super::album_repo::sql::qualites_des_pistes_de_l_album(&SqliteDialect)
            }
            Engine::Postgres => {
                super::album_repo::sql::qualites_des_pistes_de_l_album(&PostgresDialect)
            }
        };
        let meta = AlbumMetadataRepo::with_backend(self.db.clone());
        let mut membres = Vec::with_capacity(ids.len());
        for &id in ids {
            let params: [&dyn ToSqlValue; 1] = [&id];
            let pistes = self
                .db
                .query_one(&sql_pistes, &params)?
                .and_then(|r| r.first().and_then(|v| v.as_i64()))
                .unwrap_or(0);
            let ligne = self.db.query_one(&sql_release, &params)?;
            let texte = |i: usize| {
                ligne
                    .as_ref()
                    .and_then(|r| r.get(i).and_then(|v| v.as_string()))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            let release = texte(0);
            let dossier = texte(1);
            let annee = ligne
                .as_ref()
                .and_then(|r| r.get(2).and_then(|v| v.as_i64()))
                .filter(|&a| a > 0);
            // Une piste sans format, cadence ni profondeur n'a pas de qualité
            // connue : l'import ne la compte pas non plus (`scan_import`).
            let qualites = if quality_split {
                self.db
                    .query_many(&sql_qualites, &params)?
                    .iter()
                    .filter_map(|r| {
                        let format = r.get(1).and_then(|v| v.as_str());
                        let cadence = r.get(2).and_then(|v| v.as_i64());
                        let profondeur = r.get(3).and_then(|v| v.as_i64());
                        (format.is_some() || cadence.is_some() || profondeur.is_some()).then(|| {
                            crate::library::quality::score_qualite(format, cadence, profondeur)
                        })
                    })
                    .collect()
            } else {
                BTreeSet::new()
            };
            let edite = !meta
                .champs_edites_a_la_main(id)
                .unwrap_or_default()
                .is_empty();
            membres.push(Membre {
                id,
                pistes,
                release,
                edite,
                dossier,
                annee,
                qualites,
            });
        }
        Ok(membres)
    }

    /// Choisit l'album conservé du groupe et les absorptions permises.
    fn decider(
        &self,
        titre: &str,
        membres: &[Membre],
        distinctes: &DistinctPairSet,
        quality_split: bool,
        plan: &mut Vec<Fusion>,
        bilan: &mut BilanDesDoublons,
    ) {
        let edites: Vec<&Membre> = membres.iter().filter(|m| m.edite).collect();
        if edites.len() > 1 {
            bilan.edites_a_la_main += membres.len();
            tracing::info!(
                titre,
                albums = ?membres.iter().map(|m| m.id).collect::<Vec<_>>(),
                "album_doublon_ignore_plusieurs_editions_manuelles"
            );
            return;
        }
        // L'album édité à la main est conservé ; sinon le plus fourni, et à
        // égalité le plus ancien (ids triés) — un choix stable d'une passe à
        // l'autre.
        let cible = match edites.first() {
            Some(m) => (*m).clone(),
            None => membres
                .iter()
                .fold(None::<&Membre>, |best, m| match best {
                    Some(b) if b.pistes >= m.pistes => Some(b),
                    _ => Some(m),
                })
                .expect("groupe non vide")
                .clone(),
        };
        let mut release = cible.release.clone();
        let mut reunis: Vec<i64> = vec![cible.id];
        // Ce que l'ensemble réuni sous la cible porte déjà : un membre est
        // comparé à chacun, pas seulement à la cible (sinon A, B, C avec B
        // DSD et C CD réuniraient B et C sous A).
        let mut reunis_membres: Vec<&Membre> = vec![
            membres
                .iter()
                .find(|m| m.id == cible.id)
                .expect("la cible est un membre"),
        ];
        let mut a_part: Vec<Membre> = Vec::new();
        for m in membres.iter().filter(|m| m.id != cible.id) {
            if let Some(&contre) = reunis.iter().find(|&&r| distinctes.contains(r, m.id)) {
                bilan.proteges += 1;
                tracing::info!(
                    titre,
                    conserve = cible.id,
                    distinct_de = contre,
                    protege = m.id,
                    "album_merge_ignoree_paire_declaree_distincte"
                );
                continue;
            }
            if let (Some(a), Some(b)) = (&release, &m.release)
                && a != b
            {
                bilan.identifies_ailleurs += 1;
                tracing::info!(
                    titre,
                    conserve = cible.id,
                    protege = m.id,
                    "album_merge_ignoree_releases_musicbrainz_differentes"
                );
                continue;
            }
            if quality_split
                && let Some((contre, raison)) = reunis_membres
                    .iter()
                    .find_map(|r| separes_a_l_import(r, m).map(|raison| (r.id, raison)))
            {
                tracing::info!(
                    titre,
                    conserve = cible.id,
                    separe_de = contre,
                    protege = m.id,
                    raison,
                    "album_merge_ignoree_edition_separee_a_l_import"
                );
                a_part.push(m.clone());
                continue;
            }
            // `absorber` reprend les champs vides de la cible : la release du
            // doublon devient la sienne pour les membres suivants.
            if release.is_none() {
                release = m.release.clone();
            }
            reunis.push(m.id);
            reunis_membres.push(m);
            plan.push(Fusion {
                cible: cible.id,
                doublon: m.id,
                titre: titre.to_string(),
            });
        }
        // Les éditions laissées à part forment leur propre groupe : deux
        // copies de l'édition CD fusionnent encore entre elles, à part de la
        // DSD. Chaque passe en retire au moins la cible : la récursion finit.
        if !a_part.is_empty() {
            bilan.editions_separees += 1;
            if a_part.len() > 1 {
                self.decider(titre, &a_part, distinctes, quality_split, plan, bilan);
            }
        }
    }
}

/// Pourquoi l'import a fait de `a` et `b` deux éditions, avec
/// `quality_split` actif — `None` s'il les aurait réunis (#7839).
///
/// Les mêmes clés que `scan_import` et
/// [`AlbumRepo::get_or_create_for_folder_de_qualite`] : le dossier de
/// l'album, l'année, et la qualité (règle des dossiers miroirs, #7694). Une
/// valeur inconnue d'un côté ne sépare jamais : un album indexé avant que les
/// dossiers soient relevés reste fusionnable, comme avant.
fn separes_a_l_import(a: &Membre, b: &Membre) -> Option<&'static str> {
    if !a.qualites.is_empty() && !b.qualites.is_empty() && a.qualites.is_disjoint(&b.qualites) {
        return Some("qualites_sans_rien_en_commun");
    }
    if let (Some(x), Some(y)) = (a.annee, b.annee)
        && x != y
    {
        return Some("annees_differentes");
    }
    if let (Some(x), Some(y)) = (&a.dossier, &b.dossier)
        && !dossiers_d_une_meme_edition(x, y)
    {
        return Some("dossiers_distincts");
    }
    None
}

/// Deux dossiers d'album qui peuvent porter la même édition : le même, l'un
/// sous l'autre (`Album` et `Album/CD2`), ou deux frères sous un vrai parent
/// — les disques d'un coffret (`CD01`, `CD02`, voir
/// [`crate::scanner::compilation::sont_des_disques_du_meme_coffret`]) comme
/// des disques aux noms libres (`Nocturnes/Le jour`, `Nocturnes/La nuit`).
fn dossiers_d_une_meme_edition(a: &str, b: &str) -> bool {
    let (pa, pb) = (
        Path::new(a.trim_end_matches('/')),
        Path::new(b.trim_end_matches('/')),
    );
    if pa == pb || pa.starts_with(pb) || pb.starts_with(pa) {
        return true;
    }
    // Comme le coffret : la racine du système de fichiers n'est pas un parent.
    match (pa.parent(), pb.parent()) {
        (Some(x), Some(y)) => x == y && !x.as_os_str().is_empty() && x.parent().is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base");
        db.init_schema().expect("schéma");
        crate::db::migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    fn artiste(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
        db.execute(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn album(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64, pistes: usize) -> i64 {
        db.execute(
            "INSERT INTO albums (title, artist_id, source, track_count) VALUES (?, ?, 'local', 0)",
            &[&titre as &dyn ToSqlValue, &artiste],
        )
        .unwrap();
        let id = db.last_insert_rowid();
        for i in 0..pistes {
            let chemin = format!("/t/{id}/{i}.flac");
            db.execute(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path, source) \
                 VALUES ('p', ?, ?, 1000, ?, 'local')",
                &[&id as &dyn ToSqlValue, &artiste, &chemin],
            )
            .unwrap();
        }
        id
    }

    fn existe(db: &Arc<dyn DbBackend>, id: i64) -> bool {
        db.query_one(
            "SELECT COUNT(*) FROM albums WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0)
            > 0
    }

    #[test]
    fn le_critere_est_titre_et_artiste() {
        let db = base();
        let queen = artiste(&db, "Queen");
        let abba = artiste(&db, "ABBA");
        let a = album(&db, "Greatest Hits", queen, 1);
        let b = album(&db, "greatest hits", queen, 2);
        let c = album(&db, "Greatest Hits", abba, 1);
        let g = FusionDesDoublons::with_backend(db.clone())
            .groupes()
            .unwrap();
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].1, vec![a, b]);
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::Nettoyage)
            .unwrap();
        assert_eq!(bilan.fusionnes, 1);
        assert!(!existe(&db, a) && existe(&db, b) && existe(&db, c));
    }

    #[test]
    fn une_paire_distincte_avec_un_membre_deja_absorbe_reste_a_part() {
        let db = base();
        let x = artiste(&db, "X");
        let a = album(&db, "Live", x, 3);
        let b = album(&db, "Live", x, 1);
        let c = album(&db, "Live", x, 1);
        AlbumDistinctRepo::with_backend(db.clone())
            .declarer_distincts(b, c)
            .unwrap();
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert_eq!((bilan.fusionnes, bilan.proteges), (1, 1), "{bilan:?}");
        assert!(existe(&db, a) && !existe(&db, b) && existe(&db, c));
    }

    #[test]
    fn deux_releases_musicbrainz_differentes_ne_fusionnent_pas() {
        let db = base();
        let x = artiste(&db, "X");
        let a = album(&db, "Kind of Blue", x, 2);
        let b = album(&db, "Kind of Blue", x, 1);
        db.execute("UPDATE albums SET musicbrainz_release_id = 'r-' || id", &[])
            .unwrap();
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::Nettoyage)
            .unwrap();
        assert_eq!((bilan.fusionnes, bilan.identifies_ailleurs), (0, 1));
        assert!(existe(&db, a) && existe(&db, b));
    }

    #[test]
    fn l_album_edite_a_la_main_est_conserve_meme_moins_fourni() {
        let db = base();
        let x = artiste(&db, "X");
        let fourni = album(&db, "Blue", x, 5);
        let edite = album(&db, "blue", x, 1);
        AlbumMetadataRepo::with_backend(db.clone())
            .set(
                edite,
                crate::db::album_metadata_repo::CLE_EDITION_MANUELLE,
                "[\"title\"]",
            )
            .unwrap();
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert_eq!(bilan.fusionnes, 1);
        assert!(existe(&db, edite) && !existe(&db, fourni));
    }

    #[test]
    fn au_dela_du_plafond_l_automatique_s_abstient_et_le_manuel_fusionne() {
        let db = base();
        let x = artiste(&db, "X");
        for i in 0..=PLAFOND_AUTOMATIQUE {
            let t = format!("Album {i}");
            album(&db, &t, x, 1);
            album(&db, &t, x, 1);
        }
        let f = FusionDesDoublons::with_backend(db.clone());
        let auto = f.fusionner(Declencheur::FinDeScan).unwrap();
        assert_eq!(auto.fusionnes, 0);
        assert_eq!(auto.suspendus, PLAFOND_AUTOMATIQUE + 1);
        let manuel = f.fusionner(Declencheur::Manuel).unwrap();
        assert_eq!(manuel.fusionnes, PLAFOND_AUTOMATIQUE + 1);
    }

    // ── #7839 : la fusion respecte les clés de séparation de l'import ──

    const DSD: (&str, i64, i64) = ("dsf", 11_289_600, 1);
    const CD: (&str, i64, i64) = ("flac", 44_100, 16);

    /// Un album comme l'import le laisse avec `quality_split` : son dossier,
    /// son année, et des pistes d'une qualité connue rangées dans le dossier.
    fn edition(
        db: &Arc<dyn DbBackend>,
        titre: &str,
        artiste: i64,
        dossier: &str,
        annee: Option<i64>,
        (format, cadence, profondeur): (&str, i64, i64),
        pistes: usize,
    ) -> i64 {
        db.execute(
            "INSERT INTO albums (title, artist_id, source, track_count, folder_path, year) \
             VALUES (?, ?, 'local', 0, ?, ?)",
            &[&titre as &dyn ToSqlValue, &artiste, &dossier, &annee],
        )
        .unwrap();
        let id = db.last_insert_rowid();
        for i in 0..pistes {
            let chemin = format!("{dossier}/{id}-{i:02}.{format}");
            db.execute(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path, source, \
                 format, sample_rate, bit_depth) VALUES ('p', ?, ?, 1000, ?, 'local', ?, ?, ?)",
                &[
                    &id as &dyn ToSqlValue,
                    &artiste,
                    &chemin,
                    &format,
                    &cadence,
                    &profondeur,
                ],
            )
            .unwrap();
        }
        id
    }

    fn quality_split(db: &Arc<dyn DbBackend>, actif: bool) {
        SettingsRepo::with_backend(db.clone())
            .set("quality_split", if actif { "true" } else { "false" })
            .unwrap();
    }

    /// Le cas du fil 2564 : l'édition DSD et l'édition CD de « The African
    /// Beat », rangées côte à côte sous le dossier de l'artiste. Les dossiers
    /// sont frères : seule la qualité dit qu'il s'agit de deux éditions.
    #[test]
    fn dsd_et_cd_restent_deux_albums_avec_quality_split_7839() {
        let db = base();
        quality_split(&db, true);
        let x = artiste(&db, "Art Blakey & The Afro-Drum Ensemble");
        let dsd = edition(
            &db,
            "The African Beat",
            x,
            "/m/Art Blakey/The African Beat [DSD]",
            Some(1962),
            DSD,
            7,
        );
        let cd = edition(
            &db,
            "The African Beat",
            x,
            "/m/Art Blakey/The African Beat [CD]",
            Some(1962),
            CD,
            7,
        );
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert!(
            existe(&db, dsd) && existe(&db, cd),
            "#7839 : la fusion de fin de scan a réuni l'édition DSD et l'édition CD \
             que l'import avait séparées (quality_split actif) — {bilan:?}"
        );
        assert_eq!(
            (bilan.fusionnes, bilan.editions_separees),
            (0, 1),
            "{bilan:?}"
        );
    }

    /// Les deux éditions sous deux racines (`/m/DSD/…`, `/m/CD/…`) : le
    /// chemin du diagnostic, avec le bouton « Fusionner les doublons ».
    #[test]
    fn dsd_et_cd_sous_deux_racines_restent_deux_albums_au_manuel_7839() {
        let db = base();
        let x = artiste(&db, "Art Blakey");
        let dsd = edition(
            &db,
            "The African Beat",
            x,
            "/m/DSD/Art Blakey/The African Beat",
            None,
            DSD,
            7,
        );
        let cd = edition(
            &db,
            "The African Beat",
            x,
            "/m/CD/Art Blakey/The African Beat",
            None,
            CD,
            7,
        );
        // Réglage absent : actif par défaut, comme à l'import.
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::Manuel)
            .unwrap();
        assert!(existe(&db, dsd) && existe(&db, cd), "{bilan:?}");
    }

    /// De vrais doublons — même dossier, même qualité, deux lignes — sont
    /// toujours fusionnés.
    #[test]
    fn les_vrais_doublons_meme_dossier_meme_qualite_fusionnent_7839() {
        let db = base();
        quality_split(&db, true);
        let x = artiste(&db, "X");
        let a = edition(&db, "Blue Train", x, "/m/X/Blue Train", Some(1957), CD, 5);
        let b = edition(&db, "blue train", x, "/m/X/Blue Train", Some(1957), CD, 1);
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert_eq!(
            (bilan.fusionnes, bilan.editions_separees),
            (1, 0),
            "{bilan:?}"
        );
        assert!(existe(&db, a) && !existe(&db, b));
    }

    /// Deux extractions de même qualité dans deux dossiers sans parent
    /// commun : l'import en a fait deux albums, la fusion les laisse.
    #[test]
    fn deux_rips_de_meme_qualite_dans_deux_dossiers_restent_deux_albums_7839() {
        let db = base();
        quality_split(&db, true);
        let x = artiste(&db, "X");
        let a = edition(&db, "Blue Train", x, "/m/Rips 2009/Blue Train", None, CD, 5);
        let b = edition(&db, "Blue Train", x, "/m/Rips 2021/Blue Train", None, CD, 5);
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::Nettoyage)
            .unwrap();
        assert!(existe(&db, a) && existe(&db, b), "{bilan:?}");
        assert_eq!(bilan.editions_separees, 1);
    }

    /// Deux années connues et différentes, dossiers frères de même qualité :
    /// seule l'année les sépare, comme à l'import.
    #[test]
    fn deux_annees_differentes_restent_deux_albums_7839() {
        let db = base();
        quality_split(&db, true);
        let x = artiste(&db, "X");
        let a = edition(&db, "Live", x, "/m/X/Live 1970", Some(1970), CD, 3);
        let b = edition(&db, "Live", x, "/m/X/Live 1985", Some(1985), CD, 3);
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert!(existe(&db, a) && existe(&db, b), "{bilan:?}");
    }

    /// Deux copies de l'édition CD et une édition DSD : les copies CD
    /// fusionnent entre elles, la DSD reste à part — la comparaison se fait
    /// contre tout ce qui est réuni, et les éditions écartées forment leur
    /// propre groupe.
    #[test]
    fn les_doublons_d_une_edition_ecartee_fusionnent_entre_eux_7839() {
        let db = base();
        quality_split(&db, true);
        let x = artiste(&db, "X");
        let dsd = edition(&db, "Sketches", x, "/m/X/Sketches [DSD]", None, DSD, 9);
        let cd1 = edition(&db, "Sketches", x, "/m/X/Sketches [CD]", None, CD, 4);
        let cd2 = edition(&db, "Sketches", x, "/m/X/Sketches [CD]", None, CD, 1);
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert!(
            existe(&db, dsd) && existe(&db, cd1) && !existe(&db, cd2),
            "{bilan:?}"
        );
        assert_eq!((bilan.fusionnes, bilan.editions_separees), (1, 1));
    }

    /// Réglage coupé : l'import ne sépare pas, la fusion non plus — le
    /// comportement d'avant, inchangé.
    #[test]
    fn sans_quality_split_dsd_et_cd_fusionnent_comme_avant_7839() {
        let db = base();
        quality_split(&db, false);
        let x = artiste(&db, "X");
        let dsd = edition(
            &db,
            "The African Beat",
            x,
            "/m/DSD/X/The African Beat",
            Some(1962),
            DSD,
            7,
        );
        let cd = edition(
            &db,
            "The African Beat",
            x,
            "/m/CD/X/The African Beat",
            Some(1962),
            CD,
            6,
        );
        let bilan = FusionDesDoublons::with_backend(db.clone())
            .fusionner(Declencheur::FinDeScan)
            .unwrap();
        assert_eq!(
            (bilan.fusionnes, bilan.editions_separees),
            (1, 0),
            "{bilan:?}"
        );
        assert!(existe(&db, dsd) && !existe(&db, cd));
    }

    #[test]
    fn dossiers_d_une_meme_edition_freres_imbriques_ou_non() {
        assert!(dossiers_d_une_meme_edition("/m/A/Album", "/m/A/Album/"));
        assert!(dossiers_d_une_meme_edition("/m/A/Album", "/m/A/Album/CD2"));
        assert!(dossiers_d_une_meme_edition(
            "/m/A/Coffret/CD01",
            "/m/A/Coffret/CD02"
        ));
        assert!(dossiers_d_une_meme_edition(
            "/m/A/Nocturnes/Le jour",
            "/m/A/Nocturnes/La nuit"
        ));
        assert!(!dossiers_d_une_meme_edition(
            "/m/DSD/A/Album",
            "/m/CD/A/Album"
        ));
        // Deux dossiers posés à la racine n'ont rien qui les relie.
        assert!(!dossiers_d_une_meme_edition("/Album 1", "/Album 2"));
    }
}
