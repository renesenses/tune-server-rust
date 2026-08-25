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
    /// Empreinte audio du scan (64 Ko à 25 % du fichier) ; vide si jamais
    /// calculée. Sert à reconnaître un fichier DÉPLACÉ : même audio,
    /// nouveau chemin.
    pub hash: String,
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
    /// Même audio qu'une piste « fantôme » : le fichier a été déplacé ou
    /// renommé. Réparable d'une mise à jour de chemin, sans re-scan.
    Deplacee,
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
            Statut::Deplacee => "déplacée (même audio qu'une piste fantôme)",
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

/// Réconcilie les DÉPLACÉS : un fichier « hors bibliothèque » dont
/// l'empreinte audio correspond à celle d'une piste « fantôme » est le même
/// fichier, ailleurs. Les deux lignes fusionnent en une ligne « déplacée »
/// (chemin = le nouveau, base = l'ancienne piste, dont le chemin devient
/// `ancien_chemin` dans le CSV).
///
/// L'appariement n'a lieu que si l'empreinte est UNIQUE de chaque côté :
/// deux copies du même album partagent la même empreinte, et deviner
/// laquelle a bougé fabriquerait une fausse réparation. Dans le doute, les
/// lignes restent fantôme + hors bibliothèque.
///
/// `hashes_disque` : empreintes des seuls fichiers hors bibliothèque —
/// l'appelant ne hache que les candidats, jamais toute la bibliothèque.
pub fn apparier_deplaces(
    lignes: Vec<Ligne>,
    hashes_disque: &HashMap<String, String>,
) -> Vec<Ligne> {
    // Empreinte → indices candidats, de chaque côté.
    let mut fantomes_par_hash: HashMap<String, Vec<usize>> = HashMap::new();
    let mut candidats_par_hash: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, l) in lignes.iter().enumerate() {
        match l.statut {
            Statut::Fantome => {
                if let Some(p) = &l.bdd
                    && !p.hash.is_empty()
                {
                    fantomes_par_hash.entry(p.hash.clone()).or_default().push(i);
                }
            }
            Statut::HorsBibliotheque => {
                if let Some(h) = hashes_disque.get(&l.chemin)
                    && !h.is_empty()
                {
                    candidats_par_hash.entry(h.clone()).or_default().push(i);
                }
            }
            _ => {}
        }
    }

    // (candidat, fantôme) — uniquement les correspondances 1↔1.
    let mut promotions: Vec<(usize, usize)> = Vec::new();
    for (hash, fantomes) in &fantomes_par_hash {
        if let Some(cands) = candidats_par_hash.get(hash)
            && fantomes.len() == 1
            && cands.len() == 1
        {
            promotions.push((cands[0], fantomes[0]));
        }
    }

    let mut slots: Vec<Option<Ligne>> = lignes.into_iter().map(Some).collect();
    for (cand_i, fant_i) in promotions {
        let Some(fantome) = slots[fant_i].take() else {
            continue;
        };
        if let Some(cand) = slots[cand_i].as_mut() {
            cand.statut = Statut::Deplacee;
            cand.bdd = fantome.bdd; // l'ancienne piste — son chemin = ancien_chemin
        }
    }

    let mut out: Vec<Ligne> = slots.into_iter().flatten().collect();
    out.sort_by(|a, b| {
        a.statut
            .cmp(&b.statut)
            .then_with(|| a.chemin.cmp(&b.chemin))
    });
    out
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
        "ancien_chemin",
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
        // L'ancien chemin n'a de sens que pour une piste déplacée : ailleurs,
        // le chemin de la base EST celui de la colonne « chemin ».
        let ancien = match (l.statut, l.bdd.as_ref()) {
            (Statut::Deplacee, Some(p)) => p.chemin.clone(),
            _ => String::new(),
        };
        let _ = wtr.write_record([
            l.statut.libelle(),
            &l.chemin,
            &ancien,
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
        pbh(chemin, taille, mtime, "")
    }

    fn pbh(chemin: &str, taille: u64, mtime: u64, hash: &str) -> PisteBdd {
        PisteBdd {
            id: 7,
            chemin: chemin.into(),
            titre: "Titre; piégé".into(),
            artiste: "A".into(),
            album: "B".into(),
            format: "flac".into(),
            taille,
            mtime,
            hash: hash.into(),
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
    fn un_fichier_deplace_fusionne_fantome_et_hors_bibliotheque() {
        let lignes = classer(
            vec![fd("/m/nouveau/x.flac", 10, 100)],
            vec![pbh("/m/ancien/x.flac", 10, 100, "abc123")],
        );
        let hashes: HashMap<String, String> =
            [("/m/nouveau/x.flac".to_string(), "abc123".to_string())].into();
        let lignes = apparier_deplaces(lignes, &hashes);
        assert_eq!(lignes.len(), 1);
        assert_eq!(lignes[0].statut, Statut::Deplacee);
        assert_eq!(lignes[0].chemin, "/m/nouveau/x.flac");
        assert_eq!(
            lignes[0].bdd.as_ref().unwrap().chemin,
            "/m/ancien/x.flac",
            "la ligne déplacée garde l'ancienne piste pour la colonne ancien_chemin"
        );
        // Et le CSV rend l'ancien chemin dans sa colonne.
        let csv = rendre_csv(&lignes, &[]);
        assert!(csv.contains("/m/nouveau/x.flac;/m/ancien/x.flac"));
    }

    #[test]
    fn une_empreinte_ambigue_ne_fabrique_pas_de_deplacement() {
        // Deux copies du même album : même empreinte des deux côtés. Deviner
        // laquelle a bougé serait une fausse réparation — on ne touche à rien.
        let lignes = classer(
            vec![fd("/m/a.flac", 10, 100), fd("/m/b.flac", 10, 100)],
            vec![pbh("/m/vieux.flac", 10, 100, "dup")],
        );
        let hashes: HashMap<String, String> = [
            ("/m/a.flac".to_string(), "dup".to_string()),
            ("/m/b.flac".to_string(), "dup".to_string()),
        ]
        .into();
        let lignes = apparier_deplaces(lignes, &hashes);
        assert!(lignes.iter().all(|l| l.statut != Statut::Deplacee));
        assert_eq!(
            lignes
                .iter()
                .filter(|l| l.statut == Statut::Fantome)
                .count(),
            1
        );
        // Une empreinte de base VIDE ne s'apparie jamais non plus.
        let lignes = classer(
            vec![fd("/m/c.flac", 10, 100)],
            vec![pbh("/m/vieux2.flac", 10, 100, "")],
        );
        let hashes: HashMap<String, String> = [("/m/c.flac".to_string(), "".to_string())].into();
        let lignes = apparier_deplaces(lignes, &hashes);
        assert!(lignes.iter().all(|l| l.statut != Statut::Deplacee));
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
