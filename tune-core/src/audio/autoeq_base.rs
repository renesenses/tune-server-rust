//! La base AutoEq en ligne (#6388, fil 2282) : chercher un casque par son
//! nom et importer sa correction sans télécharger le fichier à la main.
//!
//! ## La source
//!
//! Le dépôt public [jaakkopasanen/AutoEq](https://github.com/jaakkopasanen/AutoEq)
//! (licence MIT) range ses corrections sous `results/` :
//!
//! - `results/INDEX.md` liste chaque profil, une ligne par couple casque ×
//!   source de mesure :
//!   `- [Sennheiser HD 650](./oratory1990/over-ear/Sennheiser%20HD%20650) by oratory1990`
//!   (ou `… by crinacle on GRAS 43AG-7` quand la source a plusieurs bancs) ;
//! - le profil lui-même est `<dossier>/<nom> ParametricEQ.txt`, le format que
//!   lit déjà [`crate::audio::autoeq::analyser`].
//!
//! Le même casque y figure souvent plusieurs fois (oratory1990, crinacle,
//! Rtings…) : la recherche les rend toutes, avec leur source, et c'est
//! l'utilisateur qui choisit.
//!
//! ## C'est le SERVEUR qui télécharge, et il garde une copie
//!
//! Le navigateur ne touche pas à GitHub : le serveur lit l'index et le
//! profil choisi, puis les range dans son dossier de données. Sans réseau,
//! la recherche et l'import se servent de cette copie, même périmée ; sans
//! réseau ET sans copie, l'erreur le dit ([`ErreurBaseAutoEq::HorsLigne`])
//! au lieu de rendre une liste vide qui ferait croire que le casque
//! n'existe pas.
//!
//! Seul un chemin PRÉSENT dans l'index peut être téléchargé : le client
//! désigne un profil de la liste, il ne fabrique pas d'URL.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// La racine des résultats AutoEq, telle que GitHub la sert en texte brut.
pub const BASE_AUTOEQ: &str =
    "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/";

/// L'index AutoEq ne bouge que quelques fois par mois : une semaine de copie.
pub const FRAICHEUR_PAR_DEFAUT: Duration = Duration::from_secs(7 * 24 * 3600);

/// Au-delà, ce n'est plus l'index AutoEq (≈ 850 Ko en 2026).
pub const TAILLE_MAX_INDEX: usize = 16 * 1024 * 1024;
/// Un profil ParametricEQ fait quelques centaines d'octets.
pub const TAILLE_MAX_PROFIL: usize = 64 * 1024;

const SUFFIXE_PROFIL: &str = " ParametricEQ.txt";
const FICHIER_INDEX: &str = "INDEX.md";

/// Un profil de la base : un casque, mesuré par une source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntreeAutoEq {
    /// Le modèle, tel qu'AutoEq le nomme (« Sennheiser HD 650 »).
    #[serde(rename = "name")]
    pub nom: String,
    /// La source de mesure (« oratory1990 », « crinacle »…).
    pub source: String,
    /// Le banc de mesure, quand la source en a plusieurs (« GRAS 43AG-7 »).
    #[serde(rename = "rig")]
    pub banc: Option<String>,
    /// Le dossier de forme (« over-ear », « in-ear », « 711 in-ear »…).
    #[serde(rename = "form")]
    pub forme: String,
    /// Le dossier du profil sous `results/`, décodé : ce que le client
    /// renvoie pour l'importer.
    #[serde(rename = "path")]
    pub chemin: String,
}

impl EntreeAutoEq {
    /// L'URL du fichier `… ParametricEQ.txt` sous `base`.
    pub fn url_du_profil(&self, base: &str) -> String {
        let mut url = base.trim_end_matches('/').to_string();
        for segment in self.chemin.split('/') {
            url.push('/');
            url.push_str(&urlencoding::encode(segment));
        }
        url.push('/');
        url.push_str(&urlencoding::encode(&format!(
            "{}{SUFFIXE_PROFIL}",
            self.nom
        )));
        url
    }
}

/// Lit `results/INDEX.md`. Une ligne qui n'a pas la forme attendue est
/// écartée, pas fatale : l'index grossit sans prévenir.
pub fn analyser_index(texte: &str) -> Vec<EntreeAutoEq> {
    texte.lines().filter_map(analyser_ligne).collect()
}

fn analyser_ligne(ligne: &str) -> Option<EntreeAutoEq> {
    let reste = ligne.trim_end().strip_prefix("- [")?;
    let (nom, reste) = reste.split_once("](./")?;
    // Le chemin est encodé (`%20`) : « ) by » ne peut pas y figurer.
    let (chemin_encode, par) = reste.split_once(") by ")?;
    let segments: Vec<String> = chemin_encode
        .split('/')
        .map(|s| urlencoding::decode(s).map(|d| d.into_owned()))
        .collect::<Result<_, _>>()
        .ok()?;
    let [source, forme, dossier] = segments.as_slice() else {
        return None;
    };
    if segments
        .iter()
        .any(|s| s.is_empty() || s == "." || s == ".." || s.contains('\\'))
    {
        return None;
    }
    // Le fichier porte le nom du dossier ; l'index affiche le même.
    if dossier != nom {
        return None;
    }
    let banc = par
        .split_once(" on ")
        .map(|(_, b)| b.trim().to_string())
        .filter(|b| !b.is_empty());
    Some(EntreeAutoEq {
        nom: nom.to_string(),
        source: source.clone(),
        banc,
        forme: forme.clone(),
        chemin: segments.join("/"),
    })
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

/// Les profils dont le nom (ou la source) contient chaque mot de `requete`,
/// ou dont le nom compacté contient la requête compactée. Les noms qui
/// COMMENCENT par la requête passent devant, puis les plus courts. Rend
/// au plus `limite` profils et le nombre total trouvé.
pub fn rechercher<'a>(
    entrees: &'a [EntreeAutoEq],
    requete: &str,
    limite: usize,
) -> (Vec<&'a EntreeAutoEq>, usize) {
    let q = normaliser(requete);
    let mots: Vec<&str> = q.split_whitespace().collect();
    let q_compacte = compacter(&q);
    if q_compacte.is_empty() {
        return (Vec::new(), 0);
    }
    let mut trouves: Vec<(u8, &EntreeAutoEq)> = entrees
        .iter()
        .filter_map(|e| {
            let nom = normaliser(&e.nom);
            let nom_compacte = compacter(&nom);
            let texte = format!("{nom} {}", normaliser(&e.source));
            let par_mots = mots.iter().all(|m| texte.contains(m));
            let compacte = nom_compacte.contains(&q_compacte);
            if !par_mots && !compacte {
                return None;
            }
            let rang = if nom_compacte == q_compacte {
                0
            } else if nom.starts_with(&q) || nom_compacte.starts_with(&q_compacte) {
                1
            } else {
                2
            };
            Some((rang, e))
        })
        .collect();
    trouves.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then(a.nom.len().cmp(&b.nom.len()))
            .then_with(|| a.nom.cmp(&b.nom))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.chemin.cmp(&b.chemin))
    });
    let total = trouves.len();
    (
        trouves.into_iter().take(limite).map(|(_, e)| e).collect(),
        total,
    )
}

/// Ce qui empêche de chercher ou d'importer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurBaseAutoEq {
    /// Ni réseau, ni copie locale.
    #[error("la base AutoEq est injoignable et aucune copie locale n'existe : {0}")]
    HorsLigne(String),
    /// Le chemin demandé n'est pas dans l'index.
    #[error("profil AutoEq inconnu : {0}")]
    ProfilInconnu(String),
    /// Ce qui est arrivé n'est pas l'index AutoEq.
    #[error("l'index AutoEq reçu est illisible")]
    IndexIllisible,
}

/// Ce qui va chercher un fichier de la base. Un trait, pour que les témoins
/// se passent du réseau.
#[async_trait]
pub trait TelechargeurAutoEq: Send + Sync {
    async fn telecharger(&self, url: &str, taille_max: usize) -> Result<String, String>;
}

/// Le téléchargeur de production.
pub struct TelechargeurHttp {
    client: reqwest::Client,
}

impl TelechargeurHttp {
    pub fn new() -> Self {
        let client = crate::http::client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("Tune (https://mozaiklabs.fr)")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for TelechargeurHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TelechargeurAutoEq for TelechargeurHttp {
    async fn telecharger(&self, url: &str, taille_max: usize) -> Result<String, String> {
        let r = self
            .client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?;
        if r.content_length().is_some_and(|n| n as usize > taille_max) {
            return Err(format!("réponse trop volumineuse ({url})"));
        }
        let octets = r.bytes().await.map_err(|e| e.to_string())?;
        if octets.len() > taille_max {
            return Err(format!("réponse trop volumineuse ({url})"));
        }
        String::from_utf8(octets.to_vec()).map_err(|e| e.to_string())
    }
}

struct IndexEnMemoire {
    entrees: Arc<Vec<EntreeAutoEq>>,
    releve: SystemTime,
}

/// La base AutoEq vue du serveur : l'index et les profils, avec leur copie
/// locale.
pub struct CatalogueAutoEq {
    base: String,
    telechargeur: Arc<dyn TelechargeurAutoEq>,
    /// `None` : pas de copie sur disque (base `:memory:`, témoins).
    dossier: Option<PathBuf>,
    fraicheur: Duration,
    index: tokio::sync::Mutex<Option<IndexEnMemoire>>,
}

impl CatalogueAutoEq {
    pub fn new(
        base: impl Into<String>,
        telechargeur: Arc<dyn TelechargeurAutoEq>,
        dossier: Option<PathBuf>,
    ) -> Self {
        Self {
            base: base.into(),
            telechargeur,
            dossier,
            fraicheur: FRAICHEUR_PAR_DEFAUT,
            index: tokio::sync::Mutex::new(None),
        }
    }

    /// Le catalogue de production : GitHub, copie dans `dossier`.
    pub fn production(dossier: Option<PathBuf>) -> Self {
        Self::new(BASE_AUTOEQ, Arc::new(TelechargeurHttp::new()), dossier)
    }

    pub fn avec_fraicheur(mut self, fraicheur: Duration) -> Self {
        self.fraicheur = fraicheur;
        self
    }

    fn url(&self, fichier: &str) -> String {
        format!("{}/{fichier}", self.base.trim_end_matches('/'))
    }

    fn frais(&self, releve: SystemTime) -> bool {
        SystemTime::now()
            .duration_since(releve)
            .is_ok_and(|age| age < self.fraicheur)
    }

    /// L'index : la mémoire s'il est frais, sinon le réseau, et à défaut de
    /// réseau la copie la plus récente, même périmée.
    pub async fn index(&self) -> Result<Arc<Vec<EntreeAutoEq>>, ErreurBaseAutoEq> {
        let mut garde = self.index.lock().await;
        if let Some(i) = garde.as_ref()
            && self.frais(i.releve)
        {
            return Ok(i.entrees.clone());
        }
        if garde.is_none()
            && let Some((texte, releve)) = self.lire_copie(FICHIER_INDEX)
        {
            let entrees = analyser_index(&texte);
            if !entrees.is_empty() {
                *garde = Some(IndexEnMemoire {
                    entrees: Arc::new(entrees),
                    releve,
                });
                if let Some(i) = garde.as_ref()
                    && self.frais(i.releve)
                {
                    return Ok(i.entrees.clone());
                }
            }
        }
        match self
            .telechargeur
            .telecharger(&self.url(FICHIER_INDEX), TAILLE_MAX_INDEX)
            .await
        {
            Ok(texte) => {
                let entrees = analyser_index(&texte);
                if entrees.is_empty() {
                    tracing::warn!(octets = texte.len(), "autoeq_index_illisible");
                    return garde
                        .as_ref()
                        .map(|i| i.entrees.clone())
                        .ok_or(ErreurBaseAutoEq::IndexIllisible);
                }
                tracing::info!(profils = entrees.len(), "autoeq_index_telecharge");
                self.ecrire_copie(FICHIER_INDEX, &texte);
                let entrees = Arc::new(entrees);
                *garde = Some(IndexEnMemoire {
                    entrees: entrees.clone(),
                    releve: SystemTime::now(),
                });
                Ok(entrees)
            }
            Err(erreur) => match garde.as_ref() {
                Some(i) => {
                    tracing::warn!(%erreur, "autoeq_hors_ligne_copie_servie");
                    Ok(i.entrees.clone())
                }
                None => {
                    tracing::warn!(%erreur, "autoeq_hors_ligne_sans_copie");
                    Err(ErreurBaseAutoEq::HorsLigne(erreur))
                }
            },
        }
    }

    /// Chercher un casque. Rend au plus `limite` profils et le total trouvé.
    pub async fn rechercher(
        &self,
        requete: &str,
        limite: usize,
    ) -> Result<(Vec<EntreeAutoEq>, usize), ErreurBaseAutoEq> {
        let index = self.index().await?;
        let (trouves, total) = rechercher(&index, requete, limite);
        Ok((trouves.into_iter().cloned().collect(), total))
    }

    /// Le texte ParametricEQ du profil `chemin` (un chemin de l'index) :
    /// la copie locale si elle est fraîche, sinon le réseau, et à défaut la
    /// copie même périmée.
    pub async fn profil(&self, chemin: &str) -> Result<(EntreeAutoEq, String), ErreurBaseAutoEq> {
        let index = self.index().await?;
        let entree = index
            .iter()
            .find(|e| e.chemin == chemin)
            .cloned()
            .ok_or_else(|| ErreurBaseAutoEq::ProfilInconnu(chemin.to_string()))?;
        let fichier = fichier_de_profil(chemin);
        let copie = self.lire_copie(&fichier);
        if let Some((texte, releve)) = &copie
            && self.frais(*releve)
        {
            return Ok((entree, texte.clone()));
        }
        match self
            .telechargeur
            .telecharger(&entree.url_du_profil(&self.base), TAILLE_MAX_PROFIL)
            .await
        {
            Ok(texte) => {
                self.ecrire_copie(&fichier, &texte);
                Ok((entree, texte))
            }
            Err(erreur) => match copie {
                Some((texte, _)) => {
                    tracing::warn!(%erreur, chemin, "autoeq_profil_hors_ligne_copie_servie");
                    Ok((entree, texte))
                }
                None => Err(ErreurBaseAutoEq::HorsLigne(erreur)),
            },
        }
    }

    fn lire_copie(&self, fichier: &str) -> Option<(String, SystemTime)> {
        let chemin = self.dossier.as_ref()?.join(fichier);
        let releve = std::fs::metadata(&chemin).and_then(|m| m.modified()).ok()?;
        let texte = std::fs::read_to_string(&chemin).ok()?;
        Some((texte, releve))
    }

    /// Écrit la copie (fichier temporaire puis renommage). Un échec n'est
    /// qu'une copie de moins : il est tracé, pas remonté.
    fn ecrire_copie(&self, fichier: &str, texte: &str) {
        let Some(dossier) = self.dossier.as_ref() else {
            return;
        };
        if let Err(err) = ecrire_atomiquement(&dossier.join(fichier), texte) {
            tracing::warn!(%err, fichier, "autoeq_copie_locale_impossible");
        }
    }
}

/// Le nom de la copie d'un profil : une empreinte du chemin, pour qu'aucun
/// nom de casque ne fabrique un chemin de fichier.
fn fichier_de_profil(chemin: &str) -> String {
    let empreinte = Sha256::digest(chemin.as_bytes());
    let hex: String = empreinte
        .iter()
        .take(16)
        .map(|o| format!("{o:02x}"))
        .collect();
    format!("profils/{hex}.txt")
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
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    const INDEX: &str = "# Index\n\
This is a list of all equalization profiles.\n\
\n\
- [Sennheiser HD 650](./oratory1990/over-ear/Sennheiser%20HD%20650) by oratory1990\n\
- [Sennheiser HD 650](./crinacle/GRAS%2043AG-7%20over-ear/Sennheiser%20HD%20650) by crinacle on GRAS 43AG-7\n\
- [Sennheiser HD 600](./oratory1990/over-ear/Sennheiser%20HD%20600) by oratory1990\n\
- [1MORE Aero (ANC Off)](./HypetheSonics/GRAS%20RA0045%20in-ear/1MORE%20Aero%20(ANC%20Off)) by HypetheSonics on GRAS RA0045\n\
- [Beyerdynamic DT 990 Édition](./Rtings/over-ear/Beyerdynamic%20DT%20990%20%C3%89dition) by Rtings\n\
- [Piège](./../../etc/passwd) by personne\n\
- ligne cassée\n";

    const HD650: &str = "Preamp: -6.1 dB\nFilter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n";

    /// Un faux réseau : il rend `INDEX` et `HD650`, compte ses appels, et
    /// peut être coupé.
    #[derive(Default)]
    struct FauxReseau {
        coupe: AtomicBool,
        appels: Mutex<Vec<String>>,
    }

    impl FauxReseau {
        fn appels(&self) -> Vec<String> {
            self.appels.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TelechargeurAutoEq for FauxReseau {
        async fn telecharger(&self, url: &str, _taille_max: usize) -> Result<String, String> {
            self.appels.lock().unwrap().push(url.to_string());
            if self.coupe.load(Ordering::SeqCst) {
                return Err("réseau injoignable".into());
            }
            if url.ends_with("/INDEX.md") {
                Ok(INDEX.into())
            } else if url.ends_with("ParametricEQ.txt") {
                Ok(HD650.into())
            } else {
                Err("404".into())
            }
        }
    }

    fn catalogue(reseau: &Arc<FauxReseau>, dossier: Option<PathBuf>) -> CatalogueAutoEq {
        CatalogueAutoEq::new("https://exemple.test/results/", reseau.clone(), dossier)
    }

    #[test]
    fn l_index_donne_nom_source_banc_forme_et_chemin() {
        let e = analyser_index(INDEX);
        assert_eq!(
            e.len(),
            5,
            "titre, ligne cassée et chemin piégé écartés : {e:?}"
        );
        assert_eq!(
            e[1],
            EntreeAutoEq {
                nom: "Sennheiser HD 650".into(),
                source: "crinacle".into(),
                banc: Some("GRAS 43AG-7".into()),
                forme: "GRAS 43AG-7 over-ear".into(),
                chemin: "crinacle/GRAS 43AG-7 over-ear/Sennheiser HD 650".into(),
            }
        );
        assert_eq!(e[0].banc, None);
        assert_eq!(e[3].nom, "1MORE Aero (ANC Off)");
        assert_eq!(e[4].chemin, "Rtings/over-ear/Beyerdynamic DT 990 Édition");
        assert!(e.iter().all(|x| !x.chemin.contains("..")));
    }

    #[test]
    fn l_url_du_profil_encode_chaque_segment() {
        let e = &analyser_index(INDEX)[3];
        assert_eq!(
            e.url_du_profil("https://exemple.test/results/"),
            "https://exemple.test/results/HypetheSonics/GRAS%20RA0045%20in-ear/\
             1MORE%20Aero%20%28ANC%20Off%29/1MORE%20Aero%20%28ANC%20Off%29%20ParametricEQ.txt"
        );
    }

    #[test]
    fn la_recherche_trouve_toutes_les_sources_d_un_casque() {
        let e = analyser_index(INDEX);
        for requete in ["hd650", "HD 650", "sennheiser 650", "Sennheiser HD-650"] {
            let (trouves, total) = rechercher(&e, requete, 10);
            assert_eq!(total, 2, "{requete}");
            let sources: Vec<&str> = trouves.iter().map(|t| t.source.as_str()).collect();
            assert_eq!(sources, ["crinacle", "oratory1990"], "{requete}");
        }
        // Sans accents, sans casse.
        assert_eq!(rechercher(&e, "dt 990 edition", 10).1, 1);
        // Le nom exact passe devant ce qui ne fait que le contenir.
        let (t, _) = rechercher(&e, "sennheiser hd 60", 10);
        assert_eq!(t[0].nom, "Sennheiser HD 600");
        // La limite borne la liste, pas le total.
        let (t, total) = rechercher(&e, "sennheiser", 1);
        assert_eq!((t.len(), total), (1, 3));
        assert_eq!(rechercher(&e, "  - ", 10).1, 0, "requête vide");
        assert_eq!(rechercher(&e, "zzz", 10).1, 0);
    }

    #[tokio::test]
    async fn l_index_est_telecharge_une_fois_puis_servi_de_memoire() {
        let reseau = Arc::new(FauxReseau::default());
        let c = catalogue(&reseau, None);
        assert_eq!(c.rechercher("hd 650", 10).await.unwrap().1, 2);
        assert_eq!(c.rechercher("hd 600", 10).await.unwrap().1, 1);
        assert_eq!(reseau.appels(), ["https://exemple.test/results/INDEX.md"]);
    }

    #[tokio::test]
    async fn sans_reseau_la_copie_locale_sert_index_et_profil() {
        let dossier = tempfile::tempdir().unwrap();
        let reseau = Arc::new(FauxReseau::default());
        let c = catalogue(&reseau, Some(dossier.path().to_path_buf()));
        let (entree, texte) = c
            .profil("oratory1990/over-ear/Sennheiser HD 650")
            .await
            .unwrap();
        assert_eq!(entree.source, "oratory1990");
        assert_eq!(texte, HD650);
        assert_eq!(
            reseau.appels()[1],
            "https://exemple.test/results/oratory1990/over-ear/Sennheiser%20HD%20650/\
             Sennheiser%20HD%20650%20ParametricEQ.txt"
        );

        // Le serveur redémarre sans réseau : la copie suffit.
        let hors_ligne = Arc::new(FauxReseau::default());
        hors_ligne.coupe.store(true, Ordering::SeqCst);
        let c = catalogue(&hors_ligne, Some(dossier.path().to_path_buf()));
        assert_eq!(c.rechercher("hd650", 10).await.unwrap().1, 2);
        let (_, texte) = c
            .profil("oratory1990/over-ear/Sennheiser HD 650")
            .await
            .unwrap();
        assert_eq!(texte, HD650);
        assert!(
            hors_ligne.appels().is_empty(),
            "copie fraîche : pas de réseau"
        );
    }

    #[tokio::test]
    async fn une_copie_perimee_est_servie_quand_le_reseau_manque() {
        let dossier = tempfile::tempdir().unwrap();
        let reseau = Arc::new(FauxReseau::default());
        catalogue(&reseau, Some(dossier.path().to_path_buf()))
            .index()
            .await
            .unwrap();
        reseau.coupe.store(true, Ordering::SeqCst);
        let c =
            catalogue(&reseau, Some(dossier.path().to_path_buf())).avec_fraicheur(Duration::ZERO);
        assert_eq!(c.rechercher("hd 650", 10).await.unwrap().1, 2);
        assert_eq!(reseau.appels().len(), 2, "le réseau a bien été tenté");
    }

    #[tokio::test]
    async fn sans_reseau_ni_copie_l_erreur_le_dit() {
        let reseau = Arc::new(FauxReseau::default());
        reseau.coupe.store(true, Ordering::SeqCst);
        let c = catalogue(&reseau, None);
        assert!(matches!(
            c.rechercher("hd 650", 10).await,
            Err(ErreurBaseAutoEq::HorsLigne(_))
        ));
    }

    #[tokio::test]
    async fn un_chemin_hors_de_l_index_n_est_jamais_telecharge() {
        let reseau = Arc::new(FauxReseau::default());
        let c = catalogue(&reseau, None);
        for chemin in ["../../etc/passwd", "oratory1990/over-ear/Inconnu", ""] {
            assert!(matches!(
                c.profil(chemin).await,
                Err(ErreurBaseAutoEq::ProfilInconnu(_))
            ));
        }
        assert_eq!(reseau.appels().len(), 1, "seul l'index a été lu");
    }
}
