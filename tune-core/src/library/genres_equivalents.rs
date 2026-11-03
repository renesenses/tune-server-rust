//! Genres équivalents entre langues (#3836, fil 1751).
//!
//! « Progressive rock » et « Rock progressif » sont le MÊME genre. Ambiance
//! les traitait comme deux requêtes, et une bibliothèque où les balises sont
//! tantôt françaises, tantôt anglaises ne se laissait pas regrouper.
//!
//! La correspondance vit dans une table de DONNÉES versionnée,
//! `genres_equivalents.json`, à côté de ce module : un groupe par genre, un
//! nom canonique (graphie anglaise de la hiérarchie, [`super::genre_tree`]) et
//! ses variantes par langue. Les noms de la hiérarchie forment chacun un
//! groupe d'office, même absents de la table.
//!
//! Cette table ne sert qu'à LIRE : elle ne réécrit aucune balise, ni en base
//! ni dans les fichiers. Deux lecteurs :
//!
//! * la requête d'Ambiance ([`crate::ai::translate::nom_de_genre_en_anglais`]) :
//!   un nom de genre part vers la tour texte du CLAP sous son nom canonique ;
//! * la réponse d'Ambiance (`POST /library/search/acoustic`) : quand la
//!   requête est un nom de genre, les pistes dont une étiquette de genre est
//!   équivalente ([`pistes_du_groupe`]) passent devant, et la réponse dit le
//!   genre reconnu ([`croiser`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use serde::Deserialize;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::facet_filter::Placeholders;

/// La table, embarquée dans le binaire : un fichier de données relu par la
/// revue, pas un `const` noyé dans le code.
const TABLE_JSON: &str = include_str!("genres_equivalents.json");

#[derive(Deserialize)]
struct TableBrute {
    version: u32,
    groupes: Vec<GroupeBrut>,
}

#[derive(Deserialize)]
struct GroupeBrut {
    canonique: String,
    #[serde(default)]
    variantes: std::collections::BTreeMap<String, Vec<String>>,
}

/// Un genre et toutes ses graphies.
#[derive(Debug)]
pub struct Groupe {
    /// Nom canonique : celui qui part vers la tour texte et que l'écran affiche.
    pub canonique: String,
    /// Variantes déclarées, `(langue, graphie)`, dans l'ordre de la table.
    pub variantes: Vec<(String, String)>,
    /// Toutes les graphies repliées (canonique compris) : la clé de comparaison.
    plis: HashSet<String>,
}

impl Groupe {
    /// Cette étiquette de genre (une seule valeur, pas une liste) est-elle de
    /// ce groupe ?
    pub fn contient(&self, etiquette: &str) -> bool {
        self.plis.contains(&replier(etiquette))
    }

    /// Les graphies à montrer : le canonique puis les variantes, sans doublon.
    pub fn graphies(&self) -> Vec<&str> {
        let mut vues = HashSet::new();
        std::iter::once(self.canonique.as_str())
            .chain(self.variantes.iter().map(|(_, g)| g.as_str()))
            .filter(|g| vues.insert(replier(g)))
            .collect()
    }
}

struct Table {
    version: u32,
    groupes: Vec<Groupe>,
    /// Graphie repliée → indice du groupe.
    index: HashMap<String, usize>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| construire(TABLE_JSON).expect("genres_equivalents.json illisible"))
}

fn ranger(
    groupes: &mut Vec<Groupe>,
    index: &mut HashMap<String, usize>,
    g: Groupe,
) -> Result<(), String> {
    let i = groupes.len();
    for pli in &g.plis {
        if let Some(&j) = index.get(pli) {
            return Err(format!(
                "« {pli} » appartient à deux groupes : {} et {}",
                groupes[j].canonique, g.canonique
            ));
        }
        index.insert(pli.clone(), i);
    }
    groupes.push(g);
    Ok(())
}

fn construire(json: &str) -> Result<Table, String> {
    let brute: TableBrute = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut groupes: Vec<Groupe> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for gb in brute.groupes {
        let variantes: Vec<(String, String)> = gb
            .variantes
            .into_iter()
            .flat_map(|(langue, noms)| noms.into_iter().map(move |n| (langue.clone(), n)))
            .collect();
        let plis: HashSet<String> = std::iter::once(replier(&gb.canonique))
            .chain(variantes.iter().map(|(_, n)| replier(n)))
            .collect();
        ranger(
            &mut groupes,
            &mut index,
            Groupe {
                canonique: gb.canonique,
                variantes,
                plis,
            },
        )?;
    }
    // Les noms de la hiérarchie sont des genres d'office : « Hard Rock » se
    // regroupe avec « hard-rock » sans qu'il faille l'écrire dans la table.
    for nom in super::genre_tree::noms() {
        let pli = replier(nom);
        if index.contains_key(&pli) {
            continue;
        }
        ranger(
            &mut groupes,
            &mut index,
            Groupe {
                canonique: nom.to_string(),
                variantes: Vec::new(),
                plis: HashSet::from([pli]),
            },
        )?;
    }
    Ok(Table {
        version: brute.version,
        groupes,
        index,
    })
}

/// Version de la table embarquée (champ `version` du JSON).
pub fn version() -> u32 {
    table().version
}

/// Replie une saisie pour la comparer : minuscules, accents retirés, tirets
/// et soulignés en espaces, espaces simples.
pub fn replier(q: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let sans_accents: String = q
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    sans_accents
        .to_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le groupe d'une saisie qui est EXACTEMENT un nom de genre (la saisie
/// entière, jamais un fragment), dans n'importe quelle langue de la table.
pub fn groupe_de(nom: &str) -> Option<&'static Groupe> {
    let t = table();
    t.index.get(&replier(nom)).map(|&i| &t.groupes[i])
}

/// Le nom canonique d'une saisie qui est exactement un nom de genre.
pub fn canonique(nom: &str) -> Option<&'static str> {
    groupe_de(nom).map(|g| g.canonique.as_str())
}

/// Les pistes dont UNE des étiquettes de genre (colonne `genre` découpée
/// comme la facette, ou tableau `genres`) appartient au groupe. Triées par
/// id. Lecture seule.
///
/// Le repli (accents compris) se fait en Rust : `LOWER` ne retire pas les
/// accents, et ne le fait pas pareil sur les deux moteurs. On lit donc les
/// valeurs DISTINCTES des deux colonnes — quelques centaines dans une
/// bibliothèque réelle —, on garde celles du groupe, puis on relit les ids
/// par égalité stricte sur ces valeurs brutes.
pub fn pistes_du_groupe(db: &Arc<dyn DbBackend>, groupe: &Groupe) -> Vec<i64> {
    let lignes = match db.query_many(
        "SELECT DISTINCT genre, genres FROM tracks \
         WHERE (genre IS NOT NULL AND genre <> '') OR (genres IS NOT NULL AND genres <> '')",
        &[],
    ) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "genres_equivalents_lecture_impossible");
            return Vec::new();
        }
    };
    let mut genres_retenus: Vec<String> = Vec::new();
    let mut tableaux_retenus: Vec<String> = Vec::new();
    for l in &lignes {
        let genre = l.first().and_then(|v| v.as_string());
        let genres = l.get(1).and_then(|v| v.as_string());
        if let Some(g) = genre.as_deref()
            && crate::db::album_repo::jetons_de_genre(Some(g), None)
                .iter()
                .any(|j| groupe.contient(j))
            && !genres_retenus.iter().any(|x| x == g)
        {
            genres_retenus.push(g.to_string());
        }
        if let Some(gs) = genres.as_deref()
            && crate::db::album_repo::jetons_de_genre(None, Some(gs))
                .iter()
                .any(|j| groupe.contient(j))
            && !tableaux_retenus.iter().any(|x| x == gs)
        {
            tableaux_retenus.push(gs.to_string());
        }
    }
    let mut ph = Placeholders::new(db.engine());
    let conds: Vec<String> = [
        ph.in_list("genre", genres_retenus.len()),
        ph.in_list("genres", tableaux_retenus.len()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if conds.is_empty() {
        return Vec::new();
    }
    let valeurs: Vec<&dyn ToSqlValue> = genres_retenus
        .iter()
        .chain(tableaux_retenus.iter())
        .map(|v| v as &dyn ToSqlValue)
        .collect();
    let sql = format!(
        "SELECT id FROM tracks WHERE {} ORDER BY id",
        conds.join(" OR ")
    );
    match db.query_many(&sql, &valeurs) {
        Ok(l) => l
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "genres_equivalents_lecture_impossible");
            Vec::new()
        }
    }
}

/// Une piste du classement d'Ambiance, une fois croisé avec les étiquettes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PisteClassee {
    pub id: i64,
    /// Cosinus CLAP ; `None` pour une piste étiquetée du genre mais pas encore
    /// analysée.
    pub similarite: Option<f32>,
    /// La piste porte une étiquette de genre du groupe demandé.
    pub du_genre: bool,
}

/// Part de la page gardée purement acoustique quand la requête est un genre
/// (décision de Bertrand du 13/10, #3836) : environ un quart, pour la
/// découverte. Arrondi à l'entier INFÉRIEUR : `limit / 4`, donc au moins une
/// piste dès que `limit >= 4`, aucune en dessous.
pub fn part_acoustique(limit: usize) -> usize {
    limit / 4
}

/// Croise le classement acoustique (cosinus décroissant) avec les pistes
/// étiquetées du genre demandé, pour une page de `limit` pistes :
///
/// 1. en tête, les pistes étiquetées du genre : d'abord celles qui sont
///    analysées, dans l'ordre acoustique, puis celles qui ne le sont pas
///    encore (aucun son à comparer), par id — au plus `limit - part_acoustique(limit)` ;
/// 2. puis les meilleures pistes HORS genre par le son, qui complètent la page
///    (au moins `part_acoustique(limit)` si le classement en a assez) ;
/// 3. au-delà de la page, le reste des pistes du genre, puis le reste du
///    classement, inchangés.
///
/// Sans piste étiquetée, le classement ressort tel quel.
pub fn croiser(classement: &[(i64, f32)], etiquetees: &[i64], limit: usize) -> Vec<PisteClassee> {
    let du_genre: HashSet<i64> = etiquetees.iter().copied().collect();
    let analysees: HashSet<i64> = classement.iter().map(|(id, _)| *id).collect();
    let mut genre: Vec<PisteClassee> = classement
        .iter()
        .filter(|(id, _)| du_genre.contains(id))
        .map(|&(id, s)| PisteClassee {
            id,
            similarite: Some(s),
            du_genre: true,
        })
        .collect();
    let mut sans_son: Vec<i64> = etiquetees
        .iter()
        .copied()
        .filter(|id| !analysees.contains(id))
        .collect();
    sans_son.sort_unstable();
    sans_son.dedup();
    genre.extend(sans_son.into_iter().map(|id| PisteClassee {
        id,
        similarite: None,
        du_genre: true,
    }));
    let hors_genre: Vec<PisteClassee> = classement
        .iter()
        .filter(|(id, _)| !du_genre.contains(id))
        .map(|&(id, s)| PisteClassee {
            id,
            similarite: Some(s),
            du_genre: false,
        })
        .collect();
    if genre.is_empty() {
        return hors_genre;
    }
    let en_tete = limit
        .saturating_sub(part_acoustique(limit))
        .min(genre.len());
    let decouverte = limit.saturating_sub(en_tete).min(hors_genre.len());
    let mut sortie = Vec::with_capacity(genre.len() + hors_genre.len());
    sortie.extend_from_slice(&genre[..en_tete]);
    sortie.extend_from_slice(&hors_genre[..decouverte]);
    sortie.extend_from_slice(&genre[en_tete..]);
    sortie.extend_from_slice(&hors_genre[decouverte..]);
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Track;
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_repo::TrackRepo;

    #[test]
    fn la_table_embarquee_se_lit_et_porte_une_version() {
        assert!(version() >= 1);
        assert!(table().groupes.len() > 30);
    }

    /// Une graphie dans deux groupes rendrait le regroupement arbitraire :
    /// la construction refuse la table.
    #[test]
    fn une_graphie_dans_deux_groupes_est_refusee() {
        let json = r#"{"version":1,"groupes":[
            {"canonique":"Classical","variantes":{"fr":["Classique"]}},
            {"canonique":"Classic Rock","variantes":{"fr":["classique"]}}]}"#;
        let e = construire(json).err().expect("doublon accepté");
        assert!(e.contains("classique"), "{e}");
    }

    /// Fil 1751 : les deux saisies de JeromeQ sont le même genre, quelles que
    /// soient la casse, les accents et les tirets.
    #[test]
    fn rock_progressif_et_progressive_rock_sont_un_seul_groupe() {
        for saisie in [
            "Progressive rock",
            "PROGRESSIVE ROCK",
            "Rock progressif",
            "rock-progressif",
            "Prog Rock",
        ] {
            assert_eq!(canonique(saisie), Some("Progressive Rock"), "{saisie:?}");
        }
        assert_eq!(canonique("Opéra"), Some("Opera"));
        assert_eq!(canonique("opera"), Some("Opera"));
        // Un nom de la hiérarchie, absent de la table, est un groupe d'office.
        assert_eq!(canonique("hard-rock"), Some("Hard Rock"));
        assert_eq!(canonique("post rock"), Some("Post-Rock"));
    }

    #[test]
    fn une_phrase_ou_une_humeur_n_est_pas_un_genre() {
        for saisie in ["rock progressif des années 70", "romantique", "zouk", ""] {
            assert_eq!(canonique(saisie), None, "{saisie:?}");
        }
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn piste(
        db: &Arc<dyn DbBackend>,
        titre: &str,
        genre: Option<&str>,
        genres: Option<&str>,
    ) -> i64 {
        let mut t = Track::new(titre.into());
        t.file_path = Some(format!("/m/{titre}.flac"));
        t.genre = genre.map(str::to_string);
        t.genres = genres.map(str::to_string);
        TrackRepo::with_backend(db.clone()).create(&t).unwrap()
    }

    /// Les étiquettes françaises et anglaises du même genre se retrouvent
    /// ensemble, dans la colonne comme dans le tableau, et rien d'autre.
    #[test]
    fn les_pistes_du_groupe_reunissent_les_etiquettes_des_deux_langues() {
        let db = base();
        let en = piste(&db, "Supper's Ready", Some("Progressive Rock"), None);
        let fr = piste(&db, "Roundabout", Some("Rock progressif"), None);
        let liste = piste(&db, "Firth of Fifth", Some("Rock; Rock Progressif"), None);
        let tableau = piste(
            &db,
            "Close to the Edge",
            None,
            Some(r#"["Art Rock","prog rock"]"#),
        );
        let _hard = piste(&db, "Highway Star", Some("Hard Rock"), None);
        let _rock = piste(&db, "Black Dog", Some("Rock"), Some(r#"["Rock"]"#));
        let _phrase = piste(&db, "Autre", Some("rock progressif des années 70"), None);
        let groupe = groupe_de("Progressive rock").unwrap();
        assert_eq!(pistes_du_groupe(&db, groupe), vec![en, fr, liste, tableau]);
        let lu = TrackRepo::with_backend(db.clone())
            .get_multiple(&[fr])
            .unwrap();
        assert_eq!(
            lu[0].genre.as_deref(),
            Some("Rock progressif"),
            "aucune balise réécrite"
        );
    }

    #[test]
    fn croiser_met_le_genre_devant_sans_perdre_l_ordre_acoustique() {
        let classement = [(10, 0.9), (20, 0.8), (30, 0.7), (40, 0.6)];
        // Page large (limit 8 → 2 places acoustiques) : tout le genre tient en tête.
        let sortie = croiser(&classement, &[40, 20, 99], 8);
        let ordre: Vec<(i64, bool)> = sortie.iter().map(|p| (p.id, p.du_genre)).collect();
        assert_eq!(
            ordre,
            vec![(20, true), (40, true), (99, true), (10, false), (30, false)]
        );
        assert_eq!(sortie[2].similarite, None, "piste étiquetée non analysée");
        assert_eq!(sortie[0].similarite, Some(0.8));
        // Sans piste étiquetée : le classement d'avant, à l'identique.
        let inchange: Vec<i64> = croiser(&classement, &[], 8).iter().map(|p| p.id).collect();
        assert_eq!(inchange, vec![10, 20, 30, 40]);
    }

    /// Décision de Bertrand (13/10) : une requête-genre met le genre en tête,
    /// mais environ un quart de la page reste purement acoustique (découverte).
    #[test]
    fn un_quart_de_la_page_reste_acoustique_3836() {
        assert_eq!(part_acoustique(3), 0);
        assert_eq!(part_acoustique(4), 1);
        assert_eq!(part_acoustique(20), 5);
        // 12 pistes classées par le son (1 la plus proche) ; 8 du genre, plus
        // que les 6 places de tête d'une page de 8.
        let classement: Vec<(i64, f32)> = (1..=12).map(|i| (i, 1.0 - i as f32 / 100.0)).collect();
        let etiquetees = [2, 3, 5, 6, 8, 9, 11, 12];
        let sortie = croiser(&classement, &etiquetees, 8);
        let ids: Vec<i64> = sortie.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![2, 3, 5, 6, 8, 9, 1, 4, 11, 12, 7, 10],
            "6 du genre en tête, 2 meilleures hors genre, puis le reste"
        );
        let page = &sortie[..8];
        assert_eq!(
            page.iter().filter(|p| !p.du_genre).count(),
            2,
            "un quart acoustique"
        );
        // Peu de pistes du genre : le son complète la page, comme avant.
        let peu: Vec<i64> = croiser(&classement, &[5], 4)
            .iter()
            .take(4)
            .map(|p| p.id)
            .collect();
        assert_eq!(peu, vec![5, 1, 2, 3]);
    }
}
