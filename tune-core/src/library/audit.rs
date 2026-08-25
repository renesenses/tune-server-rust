//! Comparatif disque ↔ bibliothèque : pour un répertoire donné, chaque
//! fichier audio du disque et chaque piste en base sous ce répertoire est
//! classé, et le tout se rend en CSV lisible dans Excel.
//!
//! Le cœur (`classer`, `rendre_csv`) est PUR — aucune I/O — pour être
//! testable ligne à ligne : le walker (déjà durci contre les NAS gelés et
//! les montages absents) et la requête SQL restent dans la route.

use std::collections::HashMap;

/// Un fichier audio vu sur le disque (chemin déjà normalisé NFC).
#[derive(Debug, Clone)]
pub struct FichierDisque {
    pub chemin: String,
    pub taille: u64,
    pub mtime: u64,
}

/// Une piste de la base sous le répertoire audité (chemin tel qu'en base).
#[derive(Debug, Clone)]
pub struct PisteBdd {
    pub id: i64,
    pub chemin: String,
    pub titre: String,
    pub artiste: String,
    pub album: String,
    pub format: String,
    pub taille: u64,
    pub mtime: u64,
}

/// Statut d'une ligne du comparatif.
///
/// L'ordre de déclaration est l'ordre de tri du rapport : les problèmes
/// d'abord, les lignes saines à la fin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Statut {
    /// En base mais plus sur le disque : piste injouable.
    Fantome,
    /// Sur le disque mais jamais ingérée : le scan l'a ratée.
    HorsBibliotheque,
    /// Présente des deux côtés mais taille ou date divergent : le fichier a
    /// changé depuis le scan, les métadonnées en base sont périmées.
    Desynchronisee,
    /// Présente et identique des deux côtés.
    Indexee,
}

impl Statut {
    pub fn libelle(self) -> &'static str {
        match self {
            Statut::Fantome => "fantôme (en base, absent du disque)",
            Statut::HorsBibliotheque => "hors bibliothèque (sur disque, jamais ingéré)",
            Statut::Desynchronisee => "désynchronisée (fichier modifié depuis le scan)",
            Statut::Indexee => "indexée",
        }
    }
}

/// Une ligne du comparatif : toujours un chemin, et ce qu'on sait de chaque
/// côté.
#[derive(Debug, Clone)]
pub struct Ligne {
    pub statut: Statut,
    pub chemin: String,
    pub disque: Option<FichierDisque>,
    pub bdd: Option<PisteBdd>,
}

/// Classe l'union disque ∪ base par chemin.
///
/// La comparaison se fait sur les chemins TELS QUE FOURNIS : l'appelant doit
/// avoir appliqué la même normalisation des deux côtés (NFC — le scan stocke
/// du NFC, macOS liste du NFD ; sans ça chaque accent fabrique un faux
/// couple fantôme + hors-bibliothèque).
///
/// La désynchronisation se juge sur la taille OU le mtime. Un mtime de base
/// à 0 (jamais renseigné — anciennes ingestions) ne compte pas comme
/// divergence : on ne sait simplement pas.
pub fn classer(disque: Vec<FichierDisque>, bdd: Vec<PisteBdd>) -> Vec<Ligne> {
    let mut en_base: HashMap<String, PisteBdd> =
        bdd.into_iter().map(|p| (p.chemin.clone(), p)).collect();

    let mut lignes: Vec<Ligne> = Vec::new();
    for f in disque {
        match en_base.remove(&f.chemin) {
            Some(p) => {
                let taille_diverge = p.taille != 0 && p.taille != f.taille;
                let mtime_diverge = p.mtime != 0 && p.mtime != f.mtime;
                let statut = if taille_diverge || mtime_diverge {
                    Statut::Desynchronisee
                } else {
                    Statut::Indexee
                };
                lignes.push(Ligne {
                    statut,
                    chemin: f.chemin.clone(),
                    disque: Some(f),
                    bdd: Some(p),
                });
            }
            None => lignes.push(Ligne {
                statut: Statut::HorsBibliotheque,
                chemin: f.chemin.clone(),
                disque: Some(f),
                bdd: None,
            }),
        }
    }
    for (_, p) in en_base {
        lignes.push(Ligne {
            statut: Statut::Fantome,
            chemin: p.chemin.clone(),
            disque: None,
            bdd: Some(p),
        });
    }

    lignes.sort_by(|a, b| {
        a.statut
            .cmp(&b.statut)
            .then_with(|| a.chemin.cmp(&b.chemin))
    });
    lignes
}

fn date_iso(epoch: u64) -> String {
    if epoch == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(epoch as i64, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Rend le comparatif en CSV « Excel FR » : BOM UTF-8 (sans lui Excel lit
/// les accents en mojibake) et séparateur `;` (celui qu'Excel FR attend).
///
/// Les avertissements du walker (montage absent, dossier illisible…) sont
/// des LIGNES du fichier, statut « avertissement » : un rapport dont un
/// montage manquait sans le dire ferait passer 1 000 pistes saines pour des
/// fantômes — l'avertissement doit voyager AVEC les données.
pub fn rendre_csv(lignes: &[Ligne], avertissements: &[String]) -> String {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b';')
        .from_writer(Vec::new());

    let _ = wtr.write_record([
        "statut",
        "chemin",
        "taille_disque",
        "modifie_disque",
        "track_id",
        "titre",
        "artiste",
        "album",
        "format",
        "taille_base",
        "modifie_base",
    ]);
    for a in avertissements {
        let _ = wtr.write_record([
            "avertissement",
            a.as_str(),
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
    }
    for l in lignes {
        let (td, md) = l
            .disque
            .as_ref()
            .map(|f| (f.taille.to_string(), date_iso(f.mtime)))
            .unwrap_or_default();
        let vide = String::new();
        let (id, titre, artiste, album, format, tb, mb) = l
            .bdd
            .as_ref()
            .map(|p| {
                (
                    p.id.to_string(),
                    p.titre.clone(),
                    p.artiste.clone(),
                    p.album.clone(),
                    p.format.clone(),
                    p.taille.to_string(),
                    date_iso(p.mtime),
                )
            })
            .unwrap_or((
                vide.clone(),
                vide.clone(),
                vide.clone(),
                vide.clone(),
                vide.clone(),
                vide.clone(),
                vide,
            ));
        let _ = wtr.write_record([
            l.statut.libelle(),
            &l.chemin,
            &td,
            &md,
            &id,
            &titre,
            &artiste,
            &album,
            &format,
            &tb,
            &mb,
        ]);
    }

    let corps = String::from_utf8(wtr.into_inner().unwrap_or_default()).unwrap_or_default();
    format!("\u{feff}{corps}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd(chemin: &str, taille: u64, mtime: u64) -> FichierDisque {
        FichierDisque {
            chemin: chemin.into(),
            taille,
            mtime,
        }
    }

    fn pb(chemin: &str, taille: u64, mtime: u64) -> PisteBdd {
        PisteBdd {
            id: 7,
            chemin: chemin.into(),
            titre: "Titre; piégé".into(),
            artiste: "A".into(),
            album: "B".into(),
            format: "flac".into(),
            taille,
            mtime,
        }
    }

    #[test]
    fn classement_des_quatre_statuts() {
        let lignes = classer(
            vec![
                fd("/m/ok.flac", 10, 100),
                fd("/m/change.flac", 11, 200),
                fd("/m/nouveau.flac", 12, 300),
            ],
            vec![
                pb("/m/ok.flac", 10, 100),
                pb("/m/change.flac", 11, 150), // mtime diverge
                pb("/m/disparu.flac", 13, 400),
            ],
        );
        let par_statut: Vec<(Statut, &str)> = lignes
            .iter()
            .map(|l| (l.statut, l.chemin.as_str()))
            .collect();
        // Tri : problèmes d'abord, chemins alphabétiques dans chaque statut.
        assert_eq!(
            par_statut,
            vec![
                (Statut::Fantome, "/m/disparu.flac"),
                (Statut::HorsBibliotheque, "/m/nouveau.flac"),
                (Statut::Desynchronisee, "/m/change.flac"),
                (Statut::Indexee, "/m/ok.flac"),
            ]
        );
    }

    #[test]
    fn un_mtime_de_base_inconnu_ne_fabrique_pas_de_desynchronisation() {
        // mtime 0 en base = jamais renseigné : la piste reste « indexée »,
        // sinon toute vieille ingestion passerait faussement en désync.
        let lignes = classer(vec![fd("/m/a.flac", 10, 100)], vec![pb("/m/a.flac", 10, 0)]);
        assert_eq!(lignes[0].statut, Statut::Indexee);
        // Mais une TAILLE divergente reste une désynchronisation, mtime connu
        // ou pas.
        let lignes = classer(vec![fd("/m/a.flac", 99, 100)], vec![pb("/m/a.flac", 10, 0)]);
        assert_eq!(lignes[0].statut, Statut::Desynchronisee);
    }

    #[test]
    fn le_csv_porte_le_bom_les_avertissements_et_echappe_les_separateurs() {
        let lignes = classer(
            vec![fd("/m/a.flac", 10, 100)],
            vec![pb("/m/a.flac", 10, 100)],
        );
        let csv = rendre_csv(&lignes, &["montage absent : /nas".to_string()]);
        assert!(csv.starts_with('\u{feff}'), "BOM UTF-8 requis pour Excel");
        assert!(csv.contains("avertissement;montage absent : /nas"));
        // Le titre contient un `;` : la lib doit l'avoir mis entre guillemets.
        assert!(csv.contains("\"Titre; piégé\""));
        // mtime 100 s d'epoch : la date se rend en ISO lisible.
        assert!(csv.contains("1970-01-01 00:01:40"));
        assert!(csv.contains("indexée"));
    }
}
