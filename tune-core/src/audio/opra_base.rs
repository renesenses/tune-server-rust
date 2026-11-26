//! rc14 — la base OPRA vue du serveur : chercher un casque par marque,
//! modèle et variante, et lire sa correction pour le greffon TESS « Casque »
//! (`equalizer-headphone`).
//!
//! ## La source et sa licence
//!
//! [OPRA](https://github.com/opra-project/OPRA) (Open Profiles for Revealing
//! Audio) publie toute sa base en un fichier, `dist/database_v1.jsonl` : une
//! ligne JSON par objet, de trois types :
//!
//! ```text
//! {"type":"vendor","id":"sennheiser","data":{"name":"Sennheiser"}}
//! {"type":"product","id":"sennheiser::hd650","data":{"name":"HD650","vendor_id":"sennheiser","subtype":"over_the_ear",…}}
//! {"type":"eq","id":"sennheiser:hd650::oratory1990_harman_target","data":{"author":"oratory1990","details":"Harman Target","product_id":"sennheiser::hd650","type":"parametric_eq","parameters":{…}}}
//! ```
//!
//! Le jeu de données est sous licence **CC BY-SA 4.0** (`LICENSE.md` du
//! dépôt, section « Dataset » ; le code d'OPRA est MIT, et les corrections
//! reprises d'AutoEq sont MIT). Tune ne redistribue pas la base : le SERVEUR
//! la télécharge chez OPRA et en garde une copie pour lui. Chaque réponse
//! nomme la source, la licence et l'auteur de la correction (attribution).
//!
//! Une « variante » est une correction (`eq`) d'un produit : son auteur
//! (oratory1990, AutoEQ…) et sa cible (`details` : « Harman Target »…).
//!
//! Comme pour AutoEq ([`super::autoeq_base`]) : le navigateur ne touche pas à
//! GitHub ; sans réseau, la copie sert, même périmée ; sans réseau ni copie,
//! l'erreur le dit au lieu d'une liste vide.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;

use super::autoeq::ProfilAutoEq;
use super::autoeq_base::{TelechargeurAutoEq, TelechargeurHttp};

/// La base OPRA, telle que GitHub la sert en texte brut.
pub const BASE_OPRA: &str =
    "https://raw.githubusercontent.com/opra-project/OPRA/main/dist/database_v1.jsonl";
/// Le dépôt, cité avec chaque correction.
pub const DEPOT_OPRA: &str = "https://github.com/opra-project/OPRA";
/// La licence du jeu de données OPRA.
pub const LICENCE_OPRA: &str = "CC BY-SA 4.0";
/// La base change quelques fois par mois : une semaine de copie.
pub const FRAICHEUR_PAR_DEFAUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// ≈ 12,6 Mo en novembre 2026 ; au-delà de 64 Mo, ce n'est plus la base.
pub const TAILLE_MAX_BASE: usize = 64 * 1024 * 1024;

const FICHIER_BASE: &str = "database_v1.jsonl";

/// Une correction d'un produit (une « variante »).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VarianteOpra {
    /// L'identifiant OPRA de la correction (`sennheiser:hd650::oratory1990_harman_target`).
    pub id: String,
    /// L'auteur de la correction (« oratory1990 », « AutoEQ »…).
    pub author: String,
    /// La cible ou le banc (« Harman Target », « Measured by … »).
    pub details: String,
    /// Le lien que l'auteur publie avec sa correction, s'il y en a un.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Un produit de la base et ses corrections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProduitOpra {
    /// L'identifiant OPRA du produit (`sennheiser::hd650`).
    pub id: String,
    /// La marque (« Sennheiser »).
    pub vendor: String,
    /// Le modèle (« HD650 »).
    pub name: String,
    /// La forme (`over_the_ear`, `in_ear`, `on_ear`, `earbuds`).
    pub form: String,
    /// Les corrections publiées pour ce produit.
    pub variants: Vec<VarianteOpra>,
}

/// La base lue : les produits qui ont au moins une correction, et la ligne
/// brute de chaque correction (relue par [`super::opra::analyser`]).
#[derive(Debug, Default)]
pub struct BaseOpra {
    pub produits: Vec<ProduitOpra>,
    /// id de correction → (rang du produit, ligne JSONL brute).
    corrections: HashMap<String, (usize, String)>,
}

impl BaseOpra {
    /// Le nombre de corrections lues.
    pub fn nb_corrections(&self) -> usize {
        self.corrections.len()
    }

    /// Le produit et la variante d'une correction, avec sa ligne brute.
    pub fn correction(&self, id: &str) -> Option<(&ProduitOpra, &VarianteOpra, &str)> {
        let (rang, ligne) = self.corrections.get(id)?;
        let produit = self.produits.get(*rang)?;
        let variante = produit.variants.iter().find(|v| v.id == id)?;
        Some((produit, variante, ligne.as_str()))
    }
}

/// Lit `database_v1.jsonl`. Une ligne illisible est sautée (la base reste
/// utilisable) ; un produit sans correction n'est pas retenu : il n'y a rien
/// à appliquer.
pub fn analyser_base(texte: &str) -> BaseOpra {
    let mut marques: HashMap<String, String> = HashMap::new();
    let mut produits: Vec<ProduitOpra> = Vec::new();
    let mut rang_produit: HashMap<String, usize> = HashMap::new();
    let mut eqs: Vec<(String, String, VarianteOpra)> = Vec::new();

    for ligne in texte.lines() {
        let ligne = ligne.trim();
        if ligne.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(ligne) else {
            continue;
        };
        let (Some(genre), Some(id)) = (
            v.get("type").and_then(Value::as_str),
            v.get("id").and_then(Value::as_str),
        ) else {
            continue;
        };
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let texte_de = |cle: &str| {
            data.get(cle)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string()
        };
        match genre {
            "vendor" => {
                marques.insert(id.to_string(), texte_de("name"));
            }
            "product" => {
                rang_produit.insert(id.to_string(), produits.len());
                produits.push(ProduitOpra {
                    id: id.to_string(),
                    // La marque est résolue après coup : l'ordre des lignes
                    // n'est pas garanti.
                    vendor: texte_de("vendor_id"),
                    name: texte_de("name"),
                    form: texte_de("subtype"),
                    variants: Vec::new(),
                });
            }
            "eq" => {
                let produit = texte_de("product_id");
                if produit.is_empty() {
                    continue;
                }
                let lien = texte_de("link");
                eqs.push((
                    produit,
                    ligne.to_string(),
                    VarianteOpra {
                        id: id.to_string(),
                        author: texte_de("author"),
                        details: texte_de("details"),
                        link: (!lien.is_empty()).then_some(lien),
                    },
                ));
            }
            _ => {}
        }
    }

    for p in &mut produits {
        if let Some(nom) = marques.get(&p.vendor).filter(|n| !n.is_empty()) {
            p.vendor = nom.clone();
        }
    }
    let mut corrections = HashMap::new();
    for (produit, ligne, variante) in eqs {
        let Some(&rang) = rang_produit.get(&produit) else {
            continue;
        };
        corrections.insert(variante.id.clone(), (rang, ligne));
        produits[rang].variants.push(variante);
    }
    // Ne garder que les produits corrigés, en recalculant les rangs.
    let mut gardes = Vec::with_capacity(produits.len());
    let mut nouveau_rang = HashMap::new();
    for (ancien, p) in produits.into_iter().enumerate() {
        if !p.variants.is_empty() {
            nouveau_rang.insert(ancien, gardes.len());
            gardes.push(p);
        }
    }
    let corrections = corrections
        .into_iter()
        .filter_map(|(id, (rang, ligne))| nouveau_rang.get(&rang).map(|&r| (id, (r, ligne))))
        .collect();
    BaseOpra {
        produits: gardes,
        corrections,
    }
}

/// Minuscules, sans accents.
fn normaliser(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Sans espaces ni ponctuation : « HD650 » trouve « HD 650 », « HD-650 ».
fn compacter(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Les produits dont la marque, le modèle ou une variante contiennent chaque
/// mot de `requete`, ou dont « marque modèle » compacté contient la requête
/// compactée. Passent devant : le modèle exact, puis les noms qui commencent
/// par la requête, puis les plus courts. Rend au plus `limite` produits et le
/// nombre total trouvé.
pub fn rechercher<'a>(
    base: &'a BaseOpra,
    requete: &str,
    limite: usize,
) -> (Vec<&'a ProduitOpra>, usize) {
    let q = normaliser(requete);
    let mots: Vec<&str> = q.split_whitespace().collect();
    let q_compacte = compacter(&q);
    if q_compacte.is_empty() {
        return (Vec::new(), 0);
    }
    let mut trouves: Vec<(u8, &ProduitOpra)> = base
        .produits
        .iter()
        .filter_map(|p| {
            let nom = normaliser(&p.name);
            let complet = format!("{} {nom}", normaliser(&p.vendor));
            let nom_compacte = compacter(&nom);
            let complet_compacte = compacter(&complet);
            let mut texte = complet.clone();
            for v in &p.variants {
                texte.push(' ');
                texte.push_str(&normaliser(&v.author));
                texte.push(' ');
                texte.push_str(&normaliser(&v.details));
            }
            let par_mots = mots.iter().all(|m| texte.contains(m));
            let compacte = complet_compacte.contains(&q_compacte);
            if !par_mots && !compacte {
                return None;
            }
            let rang = if nom_compacte == q_compacte || complet_compacte == q_compacte {
                0
            } else if nom.starts_with(&q)
                || complet.starts_with(&q)
                || nom_compacte.starts_with(&q_compacte)
                || complet_compacte.starts_with(&q_compacte)
            {
                1
            } else {
                2
            };
            Some((rang, p))
        })
        .collect();
    trouves.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then(a.name.len().cmp(&b.name.len()))
            .then_with(|| a.vendor.cmp(&b.vendor))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = trouves.len();
    (
        trouves.into_iter().take(limite).map(|(_, p)| p).collect(),
        total,
    )
}

/// Ce qui empêche de chercher ou d'appliquer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurBaseOpra {
    /// Ni réseau, ni copie locale.
    #[error("la base OPRA est injoignable et aucune copie locale n'existe : {0}")]
    HorsLigne(String),
    /// L'identifiant demandé n'est pas dans la base.
    #[error("correction OPRA inconnue : {0}")]
    ProfilInconnu(String),
    /// Ce qui est arrivé n'est pas la base OPRA.
    #[error("la base OPRA reçue est illisible")]
    BaseIllisible,
    /// La correction existe mais Tune ne sait pas la jouer telle quelle
    /// (filtre ou pente non pris en charge) : `cle` est celle de
    /// [`super::autoeq::ErreurAutoEq::cle`].
    #[error("correction OPRA non prise en charge ({cle}) : {detail}")]
    ProfilNonPrisEnCharge { cle: &'static str, detail: String },
}

impl ErreurBaseOpra {
    /// La clé stable que le client traduit.
    pub fn cle(&self) -> &'static str {
        match self {
            Self::HorsLigne(_) => "opra_unreachable",
            Self::ProfilInconnu(_) => "opra_profile_unknown",
            Self::BaseIllisible => "opra_database_unreadable",
            Self::ProfilNonPrisEnCharge { .. } => "opra_profile_unsupported",
        }
    }
}

/// Une correction choisie, prête à appliquer.
#[derive(Debug, Clone)]
pub struct CorrectionOpra {
    pub produit: ProduitOpra,
    pub variante: VarianteOpra,
    pub profil: ProfilAutoEq,
}

struct BaseEnMemoire {
    base: Arc<BaseOpra>,
    releve: SystemTime,
}

/// La base OPRA vue du serveur, avec sa copie locale.
pub struct CatalogueOpra {
    url: String,
    telechargeur: Arc<dyn TelechargeurAutoEq>,
    /// `None` : pas de copie sur disque (base `:memory:`, témoins).
    dossier: Option<PathBuf>,
    fraicheur: Duration,
    base: tokio::sync::Mutex<Option<BaseEnMemoire>>,
}

impl CatalogueOpra {
    pub fn new(
        url: impl Into<String>,
        telechargeur: Arc<dyn TelechargeurAutoEq>,
        dossier: Option<PathBuf>,
    ) -> Self {
        Self {
            url: url.into(),
            telechargeur,
            dossier,
            fraicheur: FRAICHEUR_PAR_DEFAUT,
            base: tokio::sync::Mutex::new(None),
        }
    }

    /// Le catalogue de production : GitHub, copie dans `dossier`.
    pub fn production(dossier: Option<PathBuf>) -> Self {
        Self::new(BASE_OPRA, Arc::new(TelechargeurHttp::new()), dossier)
    }

    pub fn avec_fraicheur(mut self, fraicheur: Duration) -> Self {
        self.fraicheur = fraicheur;
        self
    }

    fn frais(&self, releve: SystemTime) -> bool {
        SystemTime::now()
            .duration_since(releve)
            .is_ok_and(|age| age < self.fraicheur)
    }

    /// La base : la mémoire si elle est fraîche, sinon le réseau, et à défaut
    /// de réseau la copie la plus récente, même périmée.
    pub async fn base(&self) -> Result<Arc<BaseOpra>, ErreurBaseOpra> {
        let mut garde = self.base.lock().await;
        if let Some(b) = garde.as_ref()
            && self.frais(b.releve)
        {
            return Ok(b.base.clone());
        }
        if garde.is_none()
            && let Some((texte, releve)) = self.lire_copie()
        {
            let base = analyser_base(&texte);
            if !base.produits.is_empty() {
                *garde = Some(BaseEnMemoire {
                    base: Arc::new(base),
                    releve,
                });
                if let Some(b) = garde.as_ref()
                    && self.frais(b.releve)
                {
                    return Ok(b.base.clone());
                }
            }
        }
        match self
            .telechargeur
            .telecharger(&self.url, TAILLE_MAX_BASE)
            .await
        {
            Ok(texte) => {
                let base = analyser_base(&texte);
                if base.produits.is_empty() {
                    tracing::warn!(octets = texte.len(), "opra_base_illisible");
                    return garde
                        .as_ref()
                        .map(|b| b.base.clone())
                        .ok_or(ErreurBaseOpra::BaseIllisible);
                }
                tracing::info!(
                    produits = base.produits.len(),
                    corrections = base.nb_corrections(),
                    "opra_base_telechargee"
                );
                self.ecrire_copie(&texte);
                let base = Arc::new(base);
                *garde = Some(BaseEnMemoire {
                    base: base.clone(),
                    releve: SystemTime::now(),
                });
                Ok(base)
            }
            Err(erreur) => match garde.as_ref() {
                Some(b) => {
                    tracing::warn!(%erreur, "opra_hors_ligne_copie_servie");
                    Ok(b.base.clone())
                }
                None => {
                    tracing::warn!(%erreur, "opra_hors_ligne_sans_copie");
                    Err(ErreurBaseOpra::HorsLigne(erreur))
                }
            },
        }
    }

    /// Chercher un casque. Rend au plus `limite` produits et le total trouvé.
    pub async fn rechercher(
        &self,
        requete: &str,
        limite: usize,
    ) -> Result<(Vec<ProduitOpra>, usize), ErreurBaseOpra> {
        let base = self.base().await?;
        let (trouves, total) = rechercher(&base, requete, limite);
        Ok((trouves.into_iter().cloned().collect(), total))
    }

    /// La correction `id`, lue par [`super::opra::analyser`] : refusée plutôt
    /// que rognée si Tune ne sait pas la jouer telle quelle.
    pub async fn correction(&self, id: &str) -> Result<CorrectionOpra, ErreurBaseOpra> {
        let base = self.base().await?;
        correction_de(&base, id)
    }

    fn lire_copie(&self) -> Option<(String, SystemTime)> {
        let chemin = self.dossier.as_ref()?.join(FICHIER_BASE);
        let releve = std::fs::metadata(&chemin).and_then(|m| m.modified()).ok()?;
        let texte = std::fs::read_to_string(&chemin).ok()?;
        Some((texte, releve))
    }

    /// Écrit la copie (fichier temporaire puis renommage). Un échec n'est
    /// qu'une copie de moins : il est tracé, pas remonté.
    fn ecrire_copie(&self, texte: &str) {
        let Some(dossier) = self.dossier.as_ref() else {
            return;
        };
        if let Err(err) = ecrire_atomiquement(&dossier.join(FICHIER_BASE), texte) {
            tracing::warn!(%err, "opra_copie_locale_impossible");
        }
    }
}

/// La correction `id` d'une base déjà lue.
pub fn correction_de(base: &BaseOpra, id: &str) -> Result<CorrectionOpra, ErreurBaseOpra> {
    let (produit, variante, ligne) = base
        .correction(id)
        .ok_or_else(|| ErreurBaseOpra::ProfilInconnu(id.to_string()))?;
    let profil =
        super::opra::analyser(ligne).map_err(|e| ErreurBaseOpra::ProfilNonPrisEnCharge {
            cle: e.cle(),
            detail: e.to_string(),
        })?;
    let mut produit = produit.clone();
    // La variante voyage à part : le produit rendu ne répète pas toute la liste.
    produit.variants.clear();
    Ok(CorrectionOpra {
        produit,
        variante: variante.clone(),
        profil,
    })
}

fn ecrire_atomiquement(chemin: &Path, texte: &str) -> std::io::Result<()> {
    if let Some(parent) = chemin.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let provisoire = chemin.with_extension("partiel");
    std::fs::write(&provisoire, texte)?;
    std::fs::rename(&provisoire, chemin)
}

#[cfg(test)]
#[path = "opra_base_tests.rs"]
pub(crate) mod tests;
