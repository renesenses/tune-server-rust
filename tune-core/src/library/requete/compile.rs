//! Le compilateur de l'arbre [`Expr`] vers une condition SQL **paramétrée**
//! (#7848), pour SQLite et PostgreSQL.
//!
//! Aucune valeur tapée par l'utilisateur n'entre dans le texte SQL : chacune
//! devient un `?` et une [`SqlValue`] liée. Le texte SQL n'est fait que de
//! fragments constants de ce fichier. Le backend PostgreSQL traduit les `?`
//! en `$n` (`translate_placeholders`).
//!
//! La condition porte sur les alias de [`FROM_DES_PISTES`] : `t` (pistes),
//! `al` (albums), `ar` (artiste de la piste). Le texte se compare plié, casse
//! et accents ignorés, comme la recherche de la bibliothèque :
//! `LOWER(unaccent(colonne)) LIKE LOWER(unaccent(?))`, la fonction `unaccent`
//! étant enregistrée sur chaque connexion SQLite et fournie par l'extension
//! du même nom sur PostgreSQL.

use super::analyse::{Champ, ChampNombre, ChampTexte, Comparaison, Expr, Role, Test};
use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::Engine;
use crate::db::track_repo::{echapper_jokers_like, like_escape_clause};

/// Les tables que suppose la condition compilée.
pub const FROM_DES_PISTES: &str = "tracks t \
     LEFT JOIN albums al ON al.id = t.album_id \
     LEFT JOIN artists ar ON ar.id = t.artist_id";

/// La plus grande page que rend [`pistes_correspondantes`]. Une limite
/// négative n'y veut jamais dire « sans limite », comme en SQLite.
pub const LIMITE_MAX: i64 = 1_000;

/// Ce dont la compilation a besoin en plus de l'arbre.
#[derive(Debug, Clone, Copy)]
pub struct Contexte {
    pub moteur: Engine,
    /// Le profil dont on lit les notes et les favoris.
    pub profil: i64,
}

/// Une condition SQL et ses paramètres, dans l'ordre des `?`.
#[derive(Debug, Clone)]
pub struct Condition {
    pub sql: String,
    pub parametres: Vec<SqlValue>,
}

impl Condition {
    /// Les paramètres sous la forme qu'attendent `query_many` et `execute`.
    pub fn liens(&self) -> Vec<&dyn ToSqlValue> {
        self.parametres
            .iter()
            .map(|v| v as &dyn ToSqlValue)
            .collect()
    }
}

/// Compile l'arbre en condition SQL paramétrée.
pub fn compiler(expr: &Expr, contexte: Contexte) -> Condition {
    let mut c = Compilateur {
        contexte,
        parametres: Vec::new(),
    };
    let sql = c.expr(expr);
    Condition {
        sql,
        parametres: c.parametres,
    }
}

/// Les identifiants des pistes qui satisfont la requête, triés, paginés.
/// La limite est ramenée dans `0..=LIMITE_MAX`.
pub fn pistes_correspondantes(
    db: &dyn DbBackend,
    expr: &Expr,
    profil: i64,
    limite: i64,
    decalage: i64,
) -> Result<Vec<i64>, String> {
    let mut condition = compiler(
        expr,
        Contexte {
            moteur: db.engine(),
            profil,
        },
    );
    let sql = format!(
        "SELECT t.id FROM {FROM_DES_PISTES} WHERE {} ORDER BY t.id LIMIT ? OFFSET ?",
        condition.sql
    );
    condition
        .parametres
        .push(SqlValue::Int(limite.clamp(0, LIMITE_MAX)));
    condition.parametres.push(SqlValue::Int(decalage.max(0)));
    let lignes = db.query_many(&sql, &condition.liens())?;
    Ok(lignes
        .iter()
        .filter_map(|l| l.first().and_then(SqlValue::as_i64))
        .collect())
}

/// Le nombre de pistes qui satisfont la requête.
pub fn nombre_de_pistes(db: &dyn DbBackend, expr: &Expr, profil: i64) -> Result<i64, String> {
    let condition = compiler(
        expr,
        Contexte {
            moteur: db.engine(),
            profil,
        },
    );
    let sql = format!(
        "SELECT COUNT(*) FROM {FROM_DES_PISTES} WHERE {}",
        condition.sql
    );
    let ligne = db.query_one(&sql, &condition.liens())?;
    Ok(ligne
        .and_then(|l| l.first().and_then(SqlValue::as_i64))
        .unwrap_or(0))
}

struct Compilateur {
    contexte: Contexte,
    parametres: Vec<SqlValue>,
}

impl Compilateur {
    /// Un paramètre texte. PostgreSQL type un `$n` d'après son contexte ;
    /// dans `unaccent($n)` le type serait déduit de la fonction, on le fixe
    /// donc explicitement pour ne dépendre d'aucune résolution de surcharge.
    fn texte(&mut self, valeur: String) -> &'static str {
        self.parametres.push(SqlValue::Text(valeur));
        match self.contexte.moteur {
            Engine::Sqlite => "?",
            Engine::Postgres => "CAST(? AS TEXT)",
        }
    }

    fn entier(&mut self, valeur: i64) -> &'static str {
        self.parametres.push(SqlValue::Int(valeur));
        "?"
    }

    /// `LOWER(unaccent(colonne)) LIKE …` : `contient` ajoute les jokers
    /// autour de la valeur échappée ; sinon le texte doit être égal.
    fn texte_plie(&mut self, colonne: &str, valeur: &str, contient: bool) -> String {
        let motif = if contient {
            format!("%{}%", echapper_jokers_like(valeur))
        } else {
            echapper_jokers_like(valeur)
        };
        let lien = self.texte(motif);
        format!(
            "LOWER(unaccent({colonne})) LIKE LOWER(unaccent({lien})){}",
            like_escape_clause()
        )
    }

    fn expr(&mut self, expr: &Expr) -> String {
        match expr {
            Expr::Terme(mot) => {
                let membres: Vec<String> = ["t.title", "ar.name", "al.title"]
                    .iter()
                    .map(|col| self.texte_plie(col, mot, true))
                    .collect();
                format!("({})", membres.join(" OR "))
            }
            Expr::Et(membres) => self.liste(membres, " AND "),
            Expr::Ou(membres) => self.liste(membres, " OR "),
            // Une condition sur une colonne NULL vaut NULL, et `NOT NULL`
            // aussi : sans le COALESCE, « -genre:jazz » écarterait les pistes
            // sans genre (leçon de #6250).
            Expr::Non(dedans) => format!("(NOT COALESCE({}, 1 = 0))", self.expr(dedans)),
            Expr::Condition { champ, test } => self.condition(*champ, test),
        }
    }

    fn liste(&mut self, membres: &[Expr], liant: &str) -> String {
        let sql: Vec<String> = membres.iter().map(|m| self.expr(m)).collect();
        format!("({})", sql.join(liant))
    }

    fn condition(&mut self, champ: Champ, test: &Test) -> String {
        match champ {
            Champ::Texte(t) => {
                let colonne = colonne_texte(t);
                match test {
                    Test::Contient(v) => self.texte_plie(colonne, v, true),
                    Test::Est(v) => self.texte_plie(colonne, v, false),
                    _ => impossible("un champ texte comparé avec < ou >"),
                }
            }
            Champ::Nombre(n) => self.nombre(n, test),
            Champ::Role(r) => self.role(r, test),
            Champ::Aime => {
                let profil = self.entier(self.contexte.profil);
                let existe = format!(
                    "EXISTS (SELECT 1 FROM favorites f WHERE f.item_type = 'track' \
                     AND f.item_id = t.id AND f.profile_id = {profil})"
                );
                match test {
                    Test::Booleen(false) => format!("(NOT {existe})"),
                    _ => existe,
                }
            }
        }
    }

    /// L'expression numérique d'un champ. La note se lit pour le profil
    /// (paramètre lié) : elle peut donc apparaître deux fois dans une plage.
    fn colonne_nombre(&mut self, champ: ChampNombre) -> String {
        match champ {
            // Go du 13/11 — une année à 0 vaut « pas d'année », des deux côtés.
            ChampNombre::Annee => crate::db::annee_de_la_piste::ANNEE_DE_LA_PISTE.to_string(),
            ChampNombre::Frequence => "t.sample_rate".to_string(),
            ChampNombre::Resolution => "t.bit_depth".to_string(),
            ChampNombre::Canaux => "t.channels".to_string(),
            ChampNombre::Duree => "t.duration_ms".to_string(),
            ChampNombre::NumeroDePiste => "t.track_number".to_string(),
            ChampNombre::NumeroDeDisque => "t.disc_number".to_string(),
            ChampNombre::Note => {
                let profil = self.entier(self.contexte.profil);
                format!(
                    "COALESCE((SELECT nr.rating FROM album_ratings nr \
                     WHERE nr.album_id = t.album_id AND nr.profile_id = {profil}), t.rating)"
                )
            }
        }
    }

    fn nombre(&mut self, champ: ChampNombre, test: &Test) -> String {
        // La durée se tape en secondes et se range en millisecondes.
        let echelle = |v: i64| {
            if champ == ChampNombre::Duree {
                v.saturating_mul(1000)
            } else {
                v
            }
        };
        match test {
            Test::Compare(op, v) => {
                let colonne = self.colonne_nombre(champ);
                let signe = match op {
                    Comparaison::Egal => "=",
                    Comparaison::Plus => ">",
                    Comparaison::PlusOuEgal => ">=",
                    Comparaison::Moins => "<",
                    Comparaison::MoinsOuEgal => "<=",
                };
                let lien = self.entier(echelle(*v));
                format!("{colonne} {signe} {lien}")
            }
            Test::Plage(bas, haut) => {
                let mut bornes = Vec::new();
                if let Some(b) = bas {
                    let colonne = self.colonne_nombre(champ);
                    let lien = self.entier(echelle(*b));
                    bornes.push(format!("{colonne} >= {lien}"));
                }
                if let Some(h) = haut {
                    let colonne = self.colonne_nombre(champ);
                    let lien = self.entier(echelle(*h));
                    bornes.push(format!("{colonne} <= {lien}"));
                }
                if bornes.is_empty() {
                    impossible("une plage sans borne")
                } else {
                    format!("({})", bornes.join(" AND "))
                }
            }
            _ => impossible("un champ numérique avec un test de texte"),
        }
    }

    /// `composer:bach` : un crédit de la piste, d'un des rôles du champ, dont
    /// le nom contient (ou est) la valeur.
    fn role(&mut self, role: Role, test: &Test) -> String {
        let (valeur, contient) = match test {
            Test::Contient(v) => (v, true),
            Test::Est(v) => (v, false),
            _ => return impossible("un rôle avec une plage ou une comparaison"),
        };
        let roles: Vec<&str> = role
            .valeurs_en_base()
            .iter()
            .map(|r| self.texte((*r).to_string()))
            .collect();
        let nom = self.texte_plie("tc.artist_name", valeur, contient);
        format!(
            "EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = t.id \
             AND LOWER(tc.role) IN ({}) AND {nom})",
            roles.join(", ")
        )
    }
}

/// Condition toujours fausse, portable (pas de littéral booléen en SQLite).
const FAUX: &str = "(1 = 0)";

/// Une combinaison que l'analyseur refuse. En production elle ne rend rien
/// (`FAUX`) ; en debug elle échoue, pour qu'une évolution de l'analyseur ne
/// produise pas en silence une requête vide.
fn impossible(quoi: &str) -> String {
    debug_assert!(false, "combinaison refusée par l'analyseur : {quoi}");
    FAUX.to_string()
}

fn colonne_texte(champ: ChampTexte) -> &'static str {
    match champ {
        ChampTexte::Artiste => "ar.name",
        ChampTexte::ArtisteAlbum => "t.album_artist",
        ChampTexte::Album => "al.title",
        ChampTexte::Titre => "t.title",
        ChampTexte::Genre => "t.genre",
        ChampTexte::Format => "t.format",
        ChampTexte::Label => "t.label",
    }
}
