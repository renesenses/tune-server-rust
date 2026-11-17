//! #8958 — « Albums douteux » : marquer une alerte comme NORMALE.
//!
//! Demande de Didier (fil 2685) : une piste de 3 s volontaire, un disque
//! d'effets sonores sans artiste… reviennent à chaque ouverture de l'écran.
//! Une alerte marquée normale n'apparaît plus ; la liste des alertes ignorées
//! permet de revenir sur le choix.
//!
//! # Portée
//!
//! Elle dépend de la NATURE de l'alerte ([`portee_pour`]) :
//!
//! * `missing_artist` : l'ALBUM quand la piste en a un (titré) — l'artiste
//!   manque en général sur tout le disque, et un album d'ambiances n'en aura
//!   jamais ; sinon la piste ;
//! * `very_short`, `missing_album` et toute autre raison : la PISTE.
//!
//! # Clé STABLE, pas l'identifiant
//!
//! Les identifiants de pistes et d'albums meurent : rescan complet
//! (`delete_all` puis réinsertion), fusion d'albums, racine déplacée. Le
//! choix se rattache donc à ce qui survit à tout cela :
//!
//! * une PISTE : son chemin de fichier ; pour une piste de feuille CUE (qui
//!   n'a pas de `file_path`), son identité CUE, `cue_media_path#cue_start_ms`
//!   — le couple de l'index unique `idx_tracks_cue_identity` ;
//! * un ALBUM : son DOSSIER (le dossier commun de ses pistes, qui couvre
//!   `CD1/`, `CD2/`) ET son titre exact. Le titre empêche un dossier trop
//!   large de couvrir un autre disque ; le dossier départage deux albums du
//!   même titre.
//!
//! `cible_id` et `libelle` ne sont que le dernier identifiant connu et le nom
//! affiché au moment du choix : jamais lus par le filtre.
//!
//! 🔴 Mémorisé pour TOUTE la bibliothèque, pas par profil : l'écran
//! Métadonnées agit sur la bibliothèque commune et n'a pas de vue par profil.
//!
//! Le filtre est en SQL ([`clause_non_ignoree`]) : le total de
//! `/metadata/doubtful`, sa pagination et la pastille du tableau de bord
//! (`doubtful_count`) comptent la même chose.
//!
//! Un choix dont la clé ne correspond plus à rien n'est retiré qu'en fin de
//! scan COMPLET sain ([`purger_introuvables`]), jamais à la lecture : un
//! disque débranché le temps d'un scan partiel ne fait rien perdre.
//!
//! Table sans numéro de migration, sur le modèle de `politiques_parentales`
//! (les numéros SQLite 155 et PG 121/122 sont disputés par plusieurs PR du
//! lot rc13) : passe finale de `run_migrations` (SQLite), `ENSURE_TABLES` et
//! `PG_FULL_SCHEMA` (PostgreSQL). Copiée à la bascule : c'est un choix de
//! l'utilisateur.

use super::backend::{DbBackend, ToSqlValue};
use super::models::Track;
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/alertes_douteuses_ignorees.sql");

/// Les raisons que `/metadata/doubtful` peut rendre.
pub const RAISONS: &[&str] = &["missing_artist", "very_short", "missing_album"];

/// La clé stable d'une piste d'alias `t`, en SQL (mêmes règles que [`cle_piste`]).
pub const SQL_CLE_PISTE: &str = "COALESCE(NULLIF(t.file_path, ''), \
     t.cue_media_path || '#' || CAST(t.cue_start_ms AS TEXT))";

/// Le chemin sur disque d'une piste d'alias `t` (fichier, ou image CUE).
const SQL_CHEMIN: &str = "COALESCE(NULLIF(t.file_path, ''), t.cue_media_path)";

/// Où le choix est mémorisé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Portee {
    Piste,
    Album,
}

impl Portee {
    pub fn as_str(self) -> &'static str {
        match self {
            Portee::Piste => "track",
            Portee::Album => "album",
        }
    }

    pub fn depuis(s: &str) -> Option<Portee> {
        match s {
            "track" => Some(Portee::Piste),
            "album" => Some(Portee::Album),
            _ => None,
        }
    }
}

/// Les raisons pour lesquelles la piste est douteuse — la règle de
/// `where_doubtful`, lue sur la piste hydratée.
pub fn raisons_de(t: &Track) -> Vec<&'static str> {
    let mut r = Vec::new();
    if t.artist_name
        .as_ref()
        .map(|a| a.is_empty() || a == "Unknown Artist")
        .unwrap_or(true)
    {
        r.push("missing_artist");
    }
    if t.duration_ms > 0 && t.duration_ms < 5000 {
        r.push("very_short");
    }
    if t.album_title.as_ref().map(|a| a.is_empty()).unwrap_or(true) {
        r.push("missing_album");
    }
    r
}

/// La clé stable d'une piste : son chemin, ou son identité CUE.
pub fn cle_piste(t: &Track) -> Option<String> {
    if let Some(p) = t.file_path.as_deref().filter(|p| !p.is_empty()) {
        return Some(p.to_string());
    }
    Some(format!("{}#{}", t.cue_media_path.as_deref()?, t.cue_start_ms?))
}

/// Le chemin sur disque d'une piste (fichier, ou image CUE).
fn chemin(t: &Track) -> Option<&str> {
    t.file_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .or(t.cue_media_path.as_deref())
}

fn est_separateur(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Le dossier commun de chemins de fichiers (séparateurs `/` ou `\`), sans
/// séparateur final. `None` quand il n'y en a pas, ou que c'est une racine
/// (`/`, `C:`) : trop large pour désigner un album.
pub fn dossier_commun<'a>(chemins: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut commun: Option<Vec<&str>> = None;
    let mut sep = "/";
    for c in chemins {
        if commun.is_none() && c.contains('\\') && !c.contains('/') {
            sep = "\\";
        }
        let parties: Vec<&str> = c.split(est_separateur).collect();
        let dossier = &parties[..parties.len().saturating_sub(1)];
        commun = Some(match commun {
            None => dossier.to_vec(),
            Some(prec) => prec
                .iter()
                .zip(dossier)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    let parties = commun?;
    // Au moins deux composants non vides : jamais `/` ni `C:` seuls.
    if parties.iter().filter(|p| !p.is_empty()).count() < 2 {
        return None;
    }
    Some(parties.join(sep))
}

/// `chemin` est-il dans le dossier `dossier` (frontière de composant) ?
pub fn sous_le_dossier(chemin: &str, dossier: &str) -> bool {
    chemin.len() > dossier.len()
        && chemin.starts_with(dossier)
        && chemin[dossier.len()..].starts_with(est_separateur)
}

/// La portée d'une alerte `raison` sur une piste d'album `album_id` titré
/// `album_titre` (voir l'en-tête du module).
pub fn portee_pour(raison: &str, album_id: Option<i64>, album_titre: Option<&str>) -> Portee {
    let album_reel =
        album_id.is_some_and(|id| id > 0) && album_titre.is_some_and(|t| !t.trim().is_empty());
    if raison == "missing_artist" && album_reel {
        Portee::Album
    } else {
        Portee::Piste
    }
}

/// Le filtre d'une raison, sur une piste d'alias `t` (et son album `al`) :
/// vrai tant que l'alerte `raison` n'est ignorée ni pour la piste ni pour son
/// album. `raison` est une constante du code (jamais une saisie) : inlinée.
pub fn clause_non_ignoree(raison: &'static str) -> String {
    debug_assert!(raison.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    format!(
        "NOT EXISTS (SELECT 1 FROM alertes_douteuses_ignorees ig \
         WHERE ig.raison = '{raison}' AND ({correspond}))",
        correspond = sql_correspond("ig"),
    )
}

/// La correspondance d'une ligne d'alias `ig` avec la piste d'alias `t` et
/// le titre de son album `al.title` : par CLÉ, jamais par identifiant.
fn sql_correspond(ig: &str) -> String {
    format!(
        "({ig}.portee = 'track' AND {ig}.cle = {SQL_CLE_PISTE}) \
         OR ({ig}.portee = 'album' AND {ig}.titre = al.title \
             AND substr({SQL_CHEMIN}, 1, length({ig}.cle)) = {ig}.cle \
             AND substr({SQL_CHEMIN}, length({ig}.cle) + 1, 1) IN ('/', '\\'))"
    )
}

/// Une alerte à ignorer, ou ignorée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlerteIgnoree {
    pub portee: Portee,
    /// Chemin (piste) ou dossier (album).
    pub cle: String,
    /// Titre exact de l'album ; vide pour une piste.
    pub titre: String,
    pub raison: String,
    /// Dernier identifiant connu (piste ou album) — affichage seulement.
    pub cible_id: Option<i64>,
    /// Nom affiché au moment du choix.
    pub libelle: Option<String>,
    pub ignoree_le: i64,
}

impl AlerteIgnoree {
    /// Cette alerte ignorée couvre-t-elle l'alerte `raison` de `t` ?
    pub fn couvre(&self, t: &Track, raison: &str) -> bool {
        if self.raison != raison {
            return false;
        }
        match self.portee {
            Portee::Piste => cle_piste(t).as_deref() == Some(self.cle.as_str()),
            Portee::Album => {
                t.album_title.as_deref() == Some(self.titre.as_str())
                    && chemin(t).is_some_and(|c| sous_le_dossier(c, &self.cle))
            }
        }
    }
}

/// L'alerte `raison` de la piste `t`, telle qu'elle serait mémorisée.
/// `chemins_album` : les chemins des pistes de son album (pour la portée
/// album). `None` quand la piste n'a pas de clé stable.
pub fn alerte_pour(
    t: &Track,
    raison: &str,
    chemins_album: &[String],
    maintenant: i64,
) -> Option<AlerteIgnoree> {
    let mut portee = portee_pour(raison, t.album_id, t.album_title.as_deref());
    let mut dossier = None;
    if portee == Portee::Album {
        dossier = dossier_commun(chemins_album.iter().map(String::as_str));
        if dossier.is_none() {
            // Pas de dossier exploitable : le choix vaut pour la piste.
            portee = Portee::Piste;
        }
    }
    Some(match portee {
        Portee::Piste => AlerteIgnoree {
            portee,
            cle: cle_piste(t)?,
            titre: String::new(),
            raison: raison.to_string(),
            cible_id: t.id,
            libelle: Some(t.title.clone()),
            ignoree_le: maintenant,
        },
        Portee::Album => AlerteIgnoree {
            portee,
            cle: dossier?,
            titre: t.album_title.clone().unwrap_or_default(),
            raison: raison.to_string(),
            cible_id: t.album_id,
            libelle: t.album_title.clone(),
            ignoree_le: maintenant,
        },
    })
}

/// Marque l'alerte comme normale. Rejouée, elle ne change rien. Rend vrai si
/// une ligne a été ajoutée.
pub fn ignorer(db: &Arc<dyn DbBackend>, a: &AlerteIgnoree) -> Result<bool, String> {
    let portee = a.portee.as_str();
    db.execute(
        "INSERT INTO alertes_douteuses_ignorees \
         (portee, cle, titre, raison, cible_id, libelle, ignoree_le) \
         VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (portee, cle, titre, raison) DO NOTHING",
        &[
            &portee as &dyn ToSqlValue,
            &a.cle,
            &a.titre,
            &a.raison,
            &a.cible_id,
            &a.libelle,
            &a.ignoree_le,
        ],
    )
    .map(|n| n > 0)
}

/// Annule le choix : l'alerte redevient visible si elle tient toujours. Rend
/// vrai si une ligne a été retirée.
pub fn retablir(
    db: &Arc<dyn DbBackend>,
    portee: Portee,
    cle: &str,
    titre: &str,
    raison: &str,
) -> Result<bool, String> {
    let portee = portee.as_str();
    db.execute(
        "DELETE FROM alertes_douteuses_ignorees \
         WHERE portee = ? AND cle = ? AND titre = ? AND raison = ?",
        &[&portee as &dyn ToSqlValue, &cle, &titre, &raison],
    )
    .map(|n| n > 0)
}

/// Toutes les alertes ignorées, les plus récentes d'abord.
pub fn lister(db: &Arc<dyn DbBackend>) -> Result<Vec<AlerteIgnoree>, String> {
    let lignes = db.query_many_strong(
        "SELECT portee, cle, titre, raison, cible_id, libelle, ignoree_le \
         FROM alertes_douteuses_ignorees \
         ORDER BY ignoree_le DESC, portee, cle, raison",
        &[],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| {
            Some(AlerteIgnoree {
                portee: Portee::depuis(l.first()?.as_str()?)?,
                cle: l.get(1)?.as_string()?,
                titre: l.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                raison: l.get(3)?.as_string()?,
                cible_id: l.get(4).and_then(|v| v.as_i64()),
                libelle: l.get(5).and_then(|v| v.as_string()),
                ignoree_le: l.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Retire les choix dont la clé ne correspond plus à AUCUNE piste. À
/// n'appeler qu'en fin de scan complet sain : un choix dont les fichiers sont
/// momentanément absents doit survivre. Rend le nombre de lignes retirées.
pub fn purger_introuvables(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    db.execute(
        &format!(
            "DELETE FROM alertes_douteuses_ignorees WHERE NOT EXISTS ( \
             SELECT 1 FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
             WHERE {correspond})",
            correspond = sql_correspond("alertes_douteuses_ignorees"),
        ),
        &[],
    )
}

/// [`purger_introuvables`] en fin de scan, journalisée ; rien si le scan
/// n'est pas complet et sain.
pub fn purger_en_fin_de_scan(db: &Arc<dyn DbBackend>, scan_complet_sain: bool, origine: &str) {
    if !scan_complet_sain {
        return;
    }
    match purger_introuvables(db) {
        Ok(0) => {}
        Ok(n) => tracing::info!(retirees = n, origine, "alertes_douteuses_ignorees_purgees"),
        Err(e) => tracing::warn!(error = %e, origine, "alertes_douteuses_ignorees_purge_echec"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dossier_commun_d_un_album_multi_disques() {
        let c = ["/m/Fx/Bruitages/CD1/01.flac", "/m/Fx/Bruitages/CD2/01.flac"];
        assert_eq!(dossier_commun(c).as_deref(), Some("/m/Fx/Bruitages"));
        assert_eq!(dossier_commun(["/m/Fx/Bruitages/01.flac"]).as_deref(), Some("/m/Fx/Bruitages"));
        // Trop large : la racine seule ne désigne pas un album.
        assert_eq!(dossier_commun(["/a.flac", "/b.flac"]), None);
        assert_eq!(dossier_commun(["/m/a.flac", "/n/b.flac"]), None);
        assert_eq!(
            dossier_commun([r"D:\Musique\Fx\01.flac", r"D:\Musique\Fx\02.flac"]).as_deref(),
            Some(r"D:\Musique\Fx")
        );
    }

    #[test]
    fn frontiere_de_composant() {
        assert!(sous_le_dossier("/m/Fx/01.flac", "/m/Fx"));
        assert!(sous_le_dossier(r"D:\M\Fx\01.flac", r"D:\M\Fx"));
        assert!(!sous_le_dossier("/m/Fx2/01.flac", "/m/Fx"));
        assert!(!sous_le_dossier("/m/Fx", "/m/Fx"));
    }
}
