//! #7619, volet L1 — la CLÉ DE TRI pré-calculée des pistes et le CURSEUR de
//! la liste `GET /library/tracks`.
//!
//! # Ce qu'on corrige
//!
//! Audit du 31/10/2026 sur la base du .18 (96 496 pistes, 53 454 visibles) :
//! chaque page de l'onglet Titres réévaluait le socle de la vue sur TOUTE la
//! table (trois sous-requêtes corrélées par piste), puis triait toute la liste
//! par `LOWER(ar.name), LOWER(al.title), CAST(…)` dans un B-tree temporaire
//! avant d'appliquer l'`OFFSET` — 0,5 à 1 s par page, quelle que soit la
//! page. L'ordre portait sur deux tables jointes : aucun index ne pouvait le
//! servir.
//!
//! # Ce que fait ce module
//!
//! - `tracks.cle_tri` porte l'ordre par défaut de la vue en UNE colonne :
//!   artiste, album, disque, numéro, pliés (minuscules, sans accents) et
//!   joints par un séparateur plus petit que tout caractère imprimable. Avec
//!   `t.id` derrière, `idx_tracks_cle_tri (cle_tri, id)` rend l'ordre TOTAL de
//!   la vue sans tri : une page lit l'index dans l'ordre et s'arrête à `limit`.
//! - la clé se calcule en Rust ([`cle_de_tri`]) : `unaccent()` est une
//!   fonction Rust côté SQLite, absente des outils externes — un déclencheur
//!   qui l'appellerait casserait toute écriture faite au `sqlite3`.
//! - elle est tenue à jour par [`TrackRepo::rafraichir_cles_de_tri`] : les
//!   clés ABSENTES (piste insérée depuis) avant chaque lecture, d'un coup
//!   d'index ; toutes les clés (artiste renommé, album retitré, piste
//!   renumérotée) en tâche de fond, au plus une fois par
//!   [`INTERVALLE_RAFRAICHISSEMENT_COMPLET`] quand la base a changé.
//! - le curseur ([`Curseur`]) encode la clé de tri de la DERNIÈRE ligne rendue :
//!   la page suivante part de là (`(cle_tri, id) > (?, ?)`), sans relire ce
//!   qui précède. Il porte aussi le rang de la ligne suivante, pour qu'un
//!   serveur dont les clés ne sont pas encore prêtes retombe sur l'`OFFSET`.
//!
//! # Ce qui change pour l'utilisateur
//!
//! Le pli : « Été » se range désormais avec les E, comme dans les tris par
//! colonne (`LOWER(unaccent(…))`, web#1716), et non plus après le Z. Les
//! LIGNES rendues ne changent pas : même socle, mêmes facettes.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::{SqlValue, ToSqlValue, TrackRepo};
use crate::TuneError;
use crate::db::facet_filter::Placeholders;

/// Entre deux champs de la clé : plus petit que tout caractère imprimable,
/// pour qu'« ab » + séparateur se range AVANT « abc » + séparateur — l'ordre
/// de la clé est celui du tuple (artiste, album, disque, numéro).
pub const SEPARATEUR: char = '\u{1}';

/// La colonne et son index — posés par la passe rejouée au démarrage, sur les
/// deux moteurs, sans numéro de migration (voir `migrations.rs`).
pub const INDEX_CLE_TRI: &str =
    "CREATE INDEX IF NOT EXISTS idx_tracks_cle_tri ON tracks(cle_tri, id)";

/// PostgreSQL : `COLLATE "C"`, l'ordre des OCTETS — celui de SQLite (BINARY)
/// et de `String` en Rust. Sans lui, la collation de la base (souvent
/// `fr_FR.UTF-8`) rangerait autrement que la clé n'a été pensée.
pub const COLONNE_CLE_TRI_PG: &str =
    "ALTER TABLE tracks ADD COLUMN IF NOT EXISTS cle_tri TEXT COLLATE \"C\"";

/// Au-delà, les clés manquantes ne se comblent pas pendant une requête : la
/// page part sur l'ancien chemin (plus lent, jamais faux) et le comblement
/// se fait en tâche de fond. C'est le cas du premier démarrage après la mise
/// à jour, où AUCUNE piste n'a encore de clé.
pub const PLAFOND_COMBLEMENT_EN_LIGNE: i64 = 5_000;

/// Un rafraîchissement COMPLET des clés (renommages) au plus une fois par
/// cet intervalle, et seulement si la base a changé.
pub const INTERVALLE_RAFRAICHISSEMENT_COMPLET: Duration = Duration::from_secs(120);

/// Les écritures se font par lots : une transaction courte laisse passer
/// les autres écrivains (le scan, la file) entre deux lots.
const LOT_D_ECRITURE: usize = 2_000;

/// Le pli d'un texte pour la clé : sans accents, puis en minuscules — le
/// `LOWER(unaccent(…))` des tris par colonne, en Rust.
pub fn plier(s: &str) -> String {
    crate::db::engine::fold_diacritics(s).to_lowercase()
}

/// `CAST(x AS INTEGER)` de SQLite, en Rust : un entier reste lui-même, un
/// réel est tronqué, un texte rend son préfixe numérique (« 1/2 » → 1, « A »
/// → 0) ; NULL reste NULL — et se range en tête, comme dans l'ancien tri.
fn entier_comme_sqlite(v: &SqlValue) -> Option<i64> {
    match v {
        SqlValue::Int(n) => Some(*n),
        SqlValue::Real(r) => Some(*r as i64),
        SqlValue::Bool(b) => Some(*b as i64),
        SqlValue::Text(s) => {
            let s = s.trim_start();
            let (signe, reste) = match s.as_bytes().first() {
                Some(b'-') => (-1, &s[1..]),
                Some(b'+') => (1, &s[1..]),
                _ => (1, s),
            };
            let chiffres: String = reste.chars().take_while(char::is_ascii_digit).collect();
            Some(chiffres.parse::<i64>().map(|n| signe * n).unwrap_or(0))
        }
        _ => None,
    }
}

/// Un numéro (disque, piste) sur dix chiffres : l'ordre des chaînes est
/// celui des nombres. NULL → chaîne vide, qui se range avant « 0000000000 »
/// (le séparateur qui suit est plus petit que « 0 »). Un négatif compte 0.
fn numero(n: Option<i64>) -> String {
    match n {
        None => String::new(),
        Some(n) => format!("{:010}", n.clamp(0, 9_999_999_999)),
    }
}

/// La clé de tri d'une piste : l'ordre par défaut de la vue (artiste, album,
/// disque, numéro), plié. `t.id` la départage dans les requêtes.
pub fn cle_de_tri(
    artiste: Option<&str>,
    album: Option<&str>,
    disque: Option<i64>,
    piste: Option<i64>,
) -> String {
    let mut cle = plier(artiste.unwrap_or(""));
    cle.push(SEPARATEUR);
    cle.push_str(&plier(album.unwrap_or("")));
    cle.push(SEPARATEUR);
    cle.push_str(&numero(disque));
    cle.push(SEPARATEUR);
    cle.push_str(&numero(piste));
    cle
}

/// Ce que la lecture a constaté des clés, retenu entre deux requêtes.
struct EtatDesCles {
    /// Le jeton des données sous lequel aucune clé ne manquait.
    jeton_complet: Option<(u64, i64)>,
    /// Le jeton vu au dernier rafraîchissement COMPLET lancé.
    jeton_du_dernier_complet: Option<(u64, i64)>,
    dernier_complet: Option<Instant>,
    complet_en_cours: bool,
}

static ETAT_DES_CLES: Mutex<EtatDesCles> = Mutex::new(EtatDesCles {
    jeton_complet: None,
    jeton_du_dernier_complet: None,
    dernier_complet: None,
    complet_en_cours: false,
});

impl TrackRepo {
    /// Recalcule `cle_tri` et n'écrit que les clés qui CHANGENT. Avec
    /// `seulement_absentes`, ne lit que les pistes sans clé (lecture par
    /// l'index : quelques millisecondes quand il n'y en a pas).
    ///
    /// Rend le nombre de clés écrites.
    pub fn rafraichir_cles_de_tri(&self, seulement_absentes: bool) -> Result<usize, TuneError> {
        let sql = format!(
            "SELECT t.id, ar.name, al.title, t.disc_number, t.track_number, t.cle_tri \
             FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id{}",
            if seulement_absentes {
                " WHERE t.cle_tri IS NULL"
            } else {
                ""
            }
        );
        let lignes = self.db.query_many(&sql, &[])?;
        let a_ecrire: Vec<(i64, String)> = lignes
            .iter()
            .filter_map(|l| {
                let id = l.first()?.as_i64()?;
                let cle = cle_de_tri(
                    l.get(1).and_then(SqlValue::as_str),
                    l.get(2).and_then(SqlValue::as_str),
                    l.get(3).and_then(entier_comme_sqlite),
                    l.get(4).and_then(entier_comme_sqlite),
                );
                (l.get(5).and_then(SqlValue::as_str) != Some(cle.as_str())).then_some((id, cle))
            })
            .collect();
        if a_ecrire.is_empty() {
            return Ok(0);
        }
        let mut ph = Placeholders::new(self.db.engine());
        let maj = format!(
            "UPDATE tracks SET cle_tri = {} WHERE id = {}",
            ph.take(),
            ph.take()
        );
        for lot in a_ecrire.chunks(LOT_D_ECRITURE) {
            self.db.write_tx(&mut |tx| {
                for (id, cle) in lot {
                    tx.execute(&maj, &[cle as &dyn ToSqlValue, id])?;
                }
                Ok(())
            })?;
        }
        Ok(a_ecrire.len())
    }

    /// Les clés de tri sont-elles prêtes à servir la page ? Comble au passage
    /// les clés absentes (pistes insérées depuis), et lance au besoin un
    /// rafraîchissement complet en tâche de fond.
    ///
    /// `false` : la colonne manque (base pas encore passée par la passe de
    /// démarrage) ou trop de clés manquent pour les combler pendant la
    /// requête — l'appelant prend l'ancien chemin, plus lent, jamais faux.
    pub fn assurer_les_cles_de_tri(&self) -> bool {
        let jeton = self.db.jeton_des_donnees();
        if let Some(j) = jeton
            && let Ok(etat) = ETAT_DES_CLES.lock()
            && etat.jeton_complet == Some(j)
        {
            return true;
        }
        let manquantes = match self
            .db
            .query_one("SELECT COUNT(*) FROM tracks WHERE cle_tri IS NULL", &[])
        {
            Ok(l) => l
                .and_then(|l| l.first().and_then(SqlValue::as_i64))
                .unwrap_or(0),
            Err(e) => {
                tracing::warn!(error = %e, "cles_de_tri_illisibles");
                return false;
            }
        };
        if manquantes > PLAFOND_COMBLEMENT_EN_LIGNE {
            self.rafraichir_en_fond(jeton, true);
            return false;
        }
        if manquantes > 0
            && let Err(e) = self.rafraichir_cles_de_tri(true)
        {
            tracing::warn!(error = %e, manquantes, "cles_de_tri_non_comblees");
            return false;
        }
        self.rafraichir_en_fond(jeton, false);
        // Relu APRÈS le comblement : notre propre écriture a changé le jeton.
        if let Some(j) = self.db.jeton_des_donnees()
            && let Ok(mut etat) = ETAT_DES_CLES.lock()
        {
            etat.jeton_complet = Some(j);
        }
        true
    }

    /// Lance le rafraîchissement COMPLET sur un fil à part, s'il n'en tourne
    /// pas déjà un et si la base a changé depuis le dernier, au plus une fois
    /// par [`INTERVALLE_RAFRAICHISSEMENT_COMPLET`] — sauf `urgent` (clés
    /// absentes en nombre : on comble tout de suite).
    fn rafraichir_en_fond(&self, jeton: Option<(u64, i64)>, urgent: bool) {
        {
            let Ok(mut etat) = ETAT_DES_CLES.lock() else {
                return;
            };
            if etat.complet_en_cours {
                return;
            }
            let du = etat
                .dernier_complet
                .is_none_or(|t| t.elapsed() >= INTERVALLE_RAFRAICHISSEMENT_COMPLET);
            let change = jeton.is_none() || etat.jeton_du_dernier_complet != jeton;
            if !urgent && !(du && change) {
                return;
            }
            etat.complet_en_cours = true;
            etat.jeton_du_dernier_complet = jeton;
        }
        let repo = TrackRepo::with_backend(self.db.clone());
        let lance = std::thread::Builder::new()
            .name("cles-de-tri".into())
            .spawn(move || {
                let debut = Instant::now();
                match repo.rafraichir_cles_de_tri(false) {
                    Ok(n) => tracing::info!(
                        ecrites = n,
                        ms = debut.elapsed().as_millis() as u64,
                        "cles_de_tri_rafraichies"
                    ),
                    Err(e) => tracing::warn!(error = %e, "cles_de_tri_rafraichissement_echec"),
                }
                if let Ok(mut etat) = ETAT_DES_CLES.lock() {
                    etat.complet_en_cours = false;
                    etat.dernier_complet = Some(Instant::now());
                }
            });
        if lance.is_err()
            && let Ok(mut etat) = ETAT_DES_CLES.lock()
        {
            etat.complet_en_cours = false;
        }
    }
}

/// Le curseur d'une page : où reprendre, pour QUEL ordre.
///
/// Opaque pour le client (base64url d'un petit JSON) : il le renvoie tel quel
/// dans `after`. Un curseur fabriqué pour un autre tri est refusé (400) — il
/// désignerait une position qui n'existe pas dans cet ordre.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curseur {
    /// Version du format.
    pub v: u8,
    /// La clé de colonne du tri (`""` pour l'ordre par défaut).
    pub s: String,
    /// Tri descendant.
    pub d: bool,
    /// Tri par colonne : la cellule est-elle « vide » (rangée en fin) ?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<i64>,
    /// Tri par colonne : la valeur triée de la dernière ligne.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<serde_json::Value>,
    /// `cle_tri` de la dernière ligne.
    pub k: String,
    /// `id` de la dernière ligne.
    pub i: i64,
    /// Rang de la ligne SUIVANTE : le repli par `OFFSET`.
    pub o: i64,
}

impl Curseur {
    pub fn encoder(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn decoder(brut: &str) -> Result<Self, String> {
        let octets = URL_SAFE_NO_PAD
            .decode(brut.trim())
            .map_err(|_| "curseur illisible".to_string())?;
        let c: Curseur =
            serde_json::from_slice(&octets).map_err(|_| "curseur illisible".to_string())?;
        if c.v != 1 || c.o < 0 {
            return Err("curseur d'un autre format".into());
        }
        Ok(c)
    }
}

/// Une valeur triée, du JSON du curseur vers le SQL.
pub(super) fn valeur_sql(v: &serde_json::Value) -> SqlValue {
    match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Int(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => SqlValue::Text(s.clone()),
        serde_json::Value::Bool(b) => SqlValue::Int(*b as i64),
        _ => SqlValue::Null,
    }
}

/// Une valeur triée, du SQL vers le JSON du curseur.
pub(super) fn valeur_json(v: &SqlValue) -> serde_json::Value {
    match v {
        SqlValue::Int(i) => serde_json::json!(i),
        SqlValue::Real(r) => serde_json::json!(r),
        SqlValue::Bool(b) => serde_json::json!(*b as i64),
        SqlValue::Text(s) => serde_json::json!(s),
        _ => serde_json::Value::Null,
    }
}

/// Les pistes que les deux replis coûteux du socle écartent (doublon
/// distant, copie de moindre qualité —
/// [`crate::db::facet_filter::sql_pistes_ecartees_par_le_socle`], exact par
/// construction), gardées sous le jeton des données : résolues UNE fois entre
/// deux écritures, et non à chaque page. Sans jeton (PostgreSQL), résolues à
/// chaque appel. `None` : la lecture a échoué, l'appelant pose le socle en SQL.
pub fn pistes_ecartees_gardees(
    db: &dyn crate::db::backend::DbBackend,
) -> Option<std::sync::Arc<std::collections::HashSet<i64>>> {
    type Garde = Option<((u64, i64), std::sync::Arc<std::collections::HashSet<i64>>)>;
    static GARDE: Mutex<Garde> = Mutex::new(None);
    // Jeton lu AVANT la lecture : une écriture entre les deux le périme.
    let jeton = db.jeton_des_donnees();
    if let Some(j) = jeton
        && let Ok(g) = GARDE.lock()
        && let Some((jj, ids)) = g.as_ref()
        && *jj == j
    {
        return Some(ids.clone());
    }
    let sql = crate::db::facet_filter::sql_pistes_ecartees_par_le_socle(db.engine());
    let ids: std::sync::Arc<std::collections::HashSet<i64>> = match db.query_many(&sql, &[]) {
        Ok(lignes) => std::sync::Arc::new(
            lignes
                .iter()
                .filter_map(|l| l.first().and_then(SqlValue::as_i64))
                .collect(),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "pistes_ecartees_non_resolues");
            return None;
        }
    };
    if let Some(j) = jeton
        && let Ok(mut g) = GARDE.lock()
    {
        *g = Some((j, ids.clone()));
    }
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_cle_suit_l_ordre_du_tuple() {
        let a = cle_de_tri(Some("AB"), Some("Z"), Some(1), Some(2));
        let b = cle_de_tri(Some("abc"), Some("A"), Some(1), Some(1));
        assert!(a < b, "« ab » doit précéder « abc » quel que soit l'album");
        let p9 = cle_de_tri(Some("x"), Some("y"), Some(1), Some(9));
        let p10 = cle_de_tri(Some("x"), Some("y"), Some(1), Some(10));
        assert!(p9 < p10, "9 avant 10 : numéros sur dix chiffres");
        let sans = cle_de_tri(Some("x"), Some("y"), Some(1), None);
        let zero = cle_de_tri(Some("x"), Some("y"), Some(1), Some(0));
        assert!(
            sans < zero,
            "un numéro NULL se range en tête, comme CAST(NULL)"
        );
    }

    #[test]
    fn la_cle_est_pliee() {
        assert_eq!(
            cle_de_tri(Some("Éla Ferré"), Some("ZÉNITH"), None, None),
            cle_de_tri(Some("ela ferre"), Some("zenith"), None, None)
        );
    }

    #[test]
    fn cast_comme_sqlite() {
        assert_eq!(entier_comme_sqlite(&SqlValue::Text("1/2".into())), Some(1));
        assert_eq!(entier_comme_sqlite(&SqlValue::Text("A".into())), Some(0));
        assert_eq!(entier_comme_sqlite(&SqlValue::Real(3.9)), Some(3));
        assert_eq!(entier_comme_sqlite(&SqlValue::Null), None);
    }

    #[test]
    fn le_curseur_fait_l_aller_retour() {
        let c = Curseur {
            v: 1,
            s: "title".into(),
            d: true,
            e: Some(0),
            x: Some(serde_json::json!("été")),
            k: cle_de_tri(Some("a"), Some("b"), Some(1), Some(2)),
            i: 42,
            o: 200,
        };
        assert_eq!(Curseur::decoder(&c.encoder()).unwrap(), c);
        assert!(Curseur::decoder("pas-un-curseur!").is_err());
    }
}
