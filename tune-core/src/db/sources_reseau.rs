//! La table `sources_reseau` (#8189) : les sources lues par le moteur sans
//! montage. Les identifiants n'y entrent que SCELLÉS
//! (`sources_reseau::identifiants`) ; ce dépôt ne les ouvre jamais et ne les
//! rend dans aucune liste.

use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};

/// Une source telle que la base la garde. `identifiants_scelles` ne sort pas
/// d'ici vers une route : voir [`SourceReseau::a_des_identifiants`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceReseau {
    pub id: String,
    pub type_source: String,
    pub nom: String,
    pub hote: String,
    pub port: Option<i64>,
    pub partage: String,
    pub racine: String,
    #[serde(skip_serializing)]
    pub identifiants_scelles: Option<String>,
    pub etat: String,
    pub derniere_erreur: Option<String>,
    pub dernier_scan: Option<String>,
    #[serde(skip_serializing)]
    pub reprise_scan: Option<String>,
    pub cree_le: String,
}

impl SourceReseau {
    pub fn a_des_identifiants(&self) -> bool {
        self.identifiants_scelles.is_some()
    }

    /// La racine virtuelle de la bibliothèque (`tune-src://<id>/<racine>`).
    pub fn racine_virtuelle(&self) -> String {
        crate::sources_reseau::chemin_distant(&self.id, &self.racine)
    }
}

/// Ce qu'il faut pour créer une source.
#[derive(Debug, Clone, Default)]
pub struct NouvelleSource {
    pub type_source: String,
    pub nom: String,
    pub hote: String,
    pub port: Option<i64>,
    pub partage: String,
    pub racine: String,
    pub identifiants_scelles: Option<String>,
}

const COLONNES: &str = "id, type_source, nom, hote, port, partage, racine, identifiants_scelles, \
                        etat, derniere_erreur, dernier_scan, reprise_scan, cree_le";

fn ligne(r: &[SqlValue]) -> Option<SourceReseau> {
    Some(SourceReseau {
        id: r.first()?.as_string()?,
        type_source: r.get(1)?.as_string().unwrap_or_default(),
        nom: r.get(2)?.as_string().unwrap_or_default(),
        hote: r.get(3)?.as_string().unwrap_or_default(),
        port: r.get(4).and_then(|v| v.as_i64()),
        partage: r.get(5)?.as_string().unwrap_or_default(),
        racine: r.get(6)?.as_string().unwrap_or_default(),
        identifiants_scelles: r.get(7).and_then(|v| v.as_string()),
        etat: r.get(8)?.as_string().unwrap_or_default(),
        derniere_erreur: r.get(9).and_then(|v| v.as_string()),
        dernier_scan: r.get(10).and_then(|v| v.as_string()),
        reprise_scan: r.get(11).and_then(|v| v.as_string()),
        cree_le: r.get(12)?.as_string().unwrap_or_default(),
    })
}

fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub struct SourcesReseauRepo {
    db: Arc<dyn DbBackend>,
}

impl SourcesReseauRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn creer(&self, n: &NouvelleSource) -> Result<SourceReseau, String> {
        // Court, sans tiret au bord : il entre dans les chemins virtuels.
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let cree_le = maintenant();
        self.db.execute(
            "INSERT INTO sources_reseau (id, type_source, nom, hote, port, partage, racine, \
             identifiants_scelles, etat, cree_le) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'nouvelle', ?)",
            &[
                &id as &dyn ToSqlValue,
                &n.type_source,
                &n.nom,
                &n.hote,
                &n.port,
                &n.partage,
                &n.racine.trim_matches('/').to_string(),
                &n.identifiants_scelles,
                &cree_le,
            ],
        )?;
        self.obtenir(&id)?
            .ok_or_else(|| "source créée introuvable".to_string())
    }

    pub fn obtenir(&self, id: &str) -> Result<Option<SourceReseau>, String> {
        let r = self.db.query_one(
            &format!("SELECT {COLONNES} FROM sources_reseau WHERE id = ?"),
            &[&id as &dyn ToSqlValue],
        )?;
        Ok(r.as_deref().and_then(ligne))
    }

    pub fn lister(&self) -> Result<Vec<SourceReseau>, String> {
        let rows = self.db.query_many(
            &format!("SELECT {COLONNES} FROM sources_reseau ORDER BY cree_le, id"),
            &[],
        )?;
        Ok(rows.iter().filter_map(|r| ligne(r)).collect())
    }

    /// `Ok(false)` si la source n'existe pas.
    pub fn supprimer(&self, id: &str) -> Result<bool, String> {
        if self.obtenir(id)?.is_none() {
            return Ok(false);
        }
        self.db.execute(
            "DELETE FROM sources_reseau WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )?;
        Ok(true)
    }

    pub fn poser_etat(&self, id: &str, etat: &str, erreur: Option<&str>) -> Result<(), String> {
        self.db.execute(
            "UPDATE sources_reseau SET etat = ?, derniere_erreur = ? WHERE id = ?",
            &[&etat as &dyn ToSqlValue, &erreur, &id],
        )?;
        Ok(())
    }

    pub fn noter_le_scan(&self, id: &str) -> Result<(), String> {
        let quand = maintenant();
        self.db.execute(
            "UPDATE sources_reseau SET dernier_scan = ? WHERE id = ?",
            &[&quand as &dyn ToSqlValue, &id],
        )?;
        Ok(())
    }

    pub fn poser_reprise(&self, id: &str, reprise: Option<&str>) -> Result<(), String> {
        self.db.execute(
            "UPDATE sources_reseau SET reprise_scan = ? WHERE id = ?",
            &[&reprise as &dyn ToSqlValue, &id],
        )?;
        Ok(())
    }

    /// Les racines virtuelles de toutes les sources configurées, pour le
    /// parcours de bibliothèque.
    pub fn racines(&self) -> Vec<String> {
        self.lister()
            .unwrap_or_default()
            .iter()
            .map(SourceReseau::racine_virtuelle)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn creer_lister_etat_reprise_supprimer() {
        let repo = SourcesReseauRepo::with_backend(base());
        let s = repo
            .creer(&NouvelleSource {
                type_source: "webdav".into(),
                nom: "NAS".into(),
                hote: "nas.local".into(),
                racine: "/Musique/".into(),
                identifiants_scelles: Some("{\"v\":1}".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(s.racine, "Musique");
        assert_eq!(s.etat, "nouvelle");
        assert!(crate::sources_reseau::identifiant_valide(&s.id), "{}", s.id);
        assert_eq!(s.racine_virtuelle(), format!("tune-src://{}/Musique", s.id));
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("identifiants_scelles").is_none(), "jamais rendu");
        assert_eq!(repo.lister().unwrap().len(), 1);
        repo.poser_etat(&s.id, "injoignable", Some("injoignable"))
            .unwrap();
        repo.poser_reprise(&s.id, Some("{}")).unwrap();
        let s2 = repo.obtenir(&s.id).unwrap().unwrap();
        assert_eq!(s2.etat, "injoignable");
        assert_eq!(s2.reprise_scan.as_deref(), Some("{}"));
        assert_eq!(repo.racines(), vec![s.racine_virtuelle()]);
        assert!(repo.supprimer(&s.id).unwrap());
        assert!(!repo.supprimer(&s.id).unwrap());
    }
}
