//! Réglage « Balises du classique » (#8325, demande ESL63P) : QUELLES balises
//! des fichiers le scan lit pour le chef, l'ensemble, l'œuvre et le
//! mouvement, et ce qu'il fait de BAND.
//!
//! Jusqu'ici, la correspondance était en dur dans
//! [`super::credits_balises`] (ENSEMBLE / ORCHESTRA / CHOIR, BAND) et dans
//! [`super::read_extended_metadata`] (CONDUCTOR, WORK, MOVEMENTNAME par lofty).
//! Un testeur qui range son chœur sous `TXXX:CHORUS` n'avait aucun recours.
//!
//! # Les valeurs par défaut sont le comportement d'avant
//!
//! Qui ne touche à rien lit exactement les mêmes balises qu'avant le
//! réglage : [`BalisesClassique::default`] reprend les listes en dur, et les
//! témoins `balises_classique_tests` le vérifient fichier par fichier.
//!
//! # Noms standard et noms libres
//!
//! Trois noms sont STANDARD : `CONDUCTOR`, `WORK` et `MOVEMENTNAME`. Ils
//! désignent la lecture par lofty, qui couvre toutes les graphies d'un même
//! champ selon le conteneur (CONDUCTOR → aussi ID3 TPE3 et l'atome MP4 ;
//! WORK → aussi `©wrk`, APE WORKTITLE et TIT1 quand GRP1 est là ;
//! MOVEMENTNAME → aussi MVNM et `©mvn`). Les retirer de la liste coupe cette
//! lecture.
//!
//! Tout autre nom est une clé BRUTE, comparée sans préfixe de conteneur et
//! sans casse ([`super::balises_brutes::nom_de_cle`]) : `CHORUS` lit le
//! Vorbis `CHORUS`, l'ID3 `TXXX:CHORUS`, l'atome `----:com.apple.iTunes:CHORUS`
//! et l'APE `Chorus`. `TXXX:CHORUS` saisi tel quel vaut `CHORUS`.
//!
//! # Où le réglage vit
//!
//! En base, sous la clé [`CLE_REGLAGE`] de `settings` (JSON). En mémoire,
//! un miroir ([`courante`]) que le scan lit à chaque fichier sans requête :
//! [`charger`] le remplit au démarrage, [`ecrire`] à chaque modification.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Serialize};

use super::balises_brutes::nom_de_cle;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé de `settings` qui porte le réglage (JSON).
pub const CLE_REGLAGE: &str = "classical_tags";

/// Le nom standard du chef : la lecture lofty `Conductor`.
pub const STANDARD_CHEF: &str = "CONDUCTOR";
/// Le nom standard de l'œuvre : la lecture lofty `Work` (et TIT1 + GRP1).
pub const STANDARD_OEUVRE: &str = "WORK";
/// Le nom standard du mouvement : la lecture lofty `Movement`.
pub const STANDARD_MOUVEMENT: &str = "MOVEMENTNAME";

/// Au plus tant de balises par rôle.
pub const MAX_BALISES_PAR_ROLE: usize = 16;
/// Au plus tant de caractères par nom de balise.
pub const MAX_LONGUEUR_NOM: usize = 64;

/// Ce que le scan fait de la clé brute BAND (voir
/// [`super::credits_balises::CLE_BAND`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeBand {
    /// Le comportement d'avant (défaut) : BAND est l'ensemble quand il
    /// diffère de l'artiste d'album et de celui de la piste, et TOUJOURS sur
    /// une piste classique (compositeur ET chef ou œuvre).
    #[default]
    Classical,
    /// BAND n'est l'ensemble que s'il diffère des artistes, même sur une
    /// piste classique.
    IfDifferent,
    /// BAND n'est jamais lu.
    Ignored,
}

/// Le réglage entier. Les noms sont rangés en capitales, sans doublon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalisesClassique {
    /// Chef d'orchestre → clé `conductor`.
    pub conductor: Vec<String>,
    /// Ensemble, orchestre, chœur → clé `ensemble`.
    pub ensemble: Vec<String>,
    /// Œuvre → clé `work` (la première trouvée).
    pub work: Vec<String>,
    /// Mouvement → clé `movement` (le premier trouvé).
    pub movement: Vec<String>,
    /// Le sort de BAND.
    pub band: ModeBand,
}

impl Default for BalisesClassique {
    /// Exactement les balises lues avant le réglage.
    fn default() -> Self {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        Self {
            conductor: v(&[STANDARD_CHEF]),
            ensemble: v(&super::credits_balises::CLES_ENSEMBLE),
            work: v(&[STANDARD_OEUVRE]),
            movement: v(&[STANDARD_MOUVEMENT]),
            band: ModeBand::Classical,
        }
    }
}

/// Le corps reçu : un rôle absent garde sa valeur par défaut.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Saisie {
    pub conductor: Option<Vec<String>>,
    pub ensemble: Option<Vec<String>>,
    pub work: Option<Vec<String>>,
    pub movement: Option<Vec<String>>,
    pub band: Option<ModeBand>,
}

/// Un nom saisi, mis en capitales et vérifié.
fn nom_valide(role: &str, brut: &str) -> Result<String, String> {
    let nom = brut.trim().to_uppercase();
    if nom.is_empty() {
        return Err(format!("{role} : nom de balise vide"));
    }
    if nom.chars().count() > MAX_LONGUEUR_NOM {
        return Err(format!(
            "{role} : « {nom} » dépasse {MAX_LONGUEUR_NOM} caractères"
        ));
    }
    if !nom
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || " _-:.".contains(c))
    {
        return Err(format!(
            "{role} : « {nom} » ne doit contenir que des lettres A-Z, des chiffres, espace, _ - : ."
        ));
    }
    if nom_de_cle(&nom).is_empty() {
        return Err(format!("{role} : « {nom} » n'a pas de nom de balise"));
    }
    Ok(nom)
}

fn liste_valide(role: &str, brute: Vec<String>) -> Result<Vec<String>, String> {
    let mut sortie: Vec<String> = Vec::new();
    for b in brute {
        let nom = nom_valide(role, &b)?;
        if nom_de_cle(&nom) == super::credits_balises::CLE_BAND {
            return Err(format!(
                "{role} : BAND se règle par son propre choix, pas dans la liste"
            ));
        }
        if !sortie.iter().any(|n| nom_de_cle(n) == nom_de_cle(&nom)) {
            sortie.push(nom);
        }
    }
    if sortie.len() > MAX_BALISES_PAR_ROLE {
        return Err(format!("{role} : au plus {MAX_BALISES_PAR_ROLE} balises"));
    }
    Ok(sortie)
}

impl Saisie {
    /// Le réglage complet, validé ; un rôle absent prend sa valeur par défaut.
    pub fn valider(self) -> Result<BalisesClassique, String> {
        let d = BalisesClassique::default();
        let prendre = |role: &str, v: Option<Vec<String>>, defaut: Vec<String>| match v {
            Some(v) => liste_valide(role, v),
            None => Ok(defaut),
        };
        Ok(BalisesClassique {
            conductor: prendre("conductor", self.conductor, d.conductor)?,
            ensemble: prendre("ensemble", self.ensemble, d.ensemble)?,
            work: prendre("work", self.work, d.work)?,
            movement: prendre("movement", self.movement, d.movement)?,
            band: self.band.unwrap_or(d.band),
        })
    }
}

impl BalisesClassique {
    /// Le nom standard `standard` est-il dans `liste` ?
    pub fn lit_standard(liste: &[String], standard: &str) -> bool {
        liste.iter().any(|n| nom_de_cle(n) == standard)
    }

    /// Les noms BRUTS de `liste` (hors nom standard `standard`), sans préfixe.
    pub fn noms_bruts<'a>(
        liste: &'a [String],
        standard: &'a str,
    ) -> impl Iterator<Item = String> + 'a {
        liste
            .iter()
            .map(|n| nom_de_cle(n))
            .filter(move |n| n != standard)
    }

    /// Toutes les clés brutes à chercher dans la tête Vorbis : les noms
    /// libres des quatre rôles, sans préfixe et sans doublon.
    pub fn cles_brutes(&self) -> Vec<String> {
        let mut cles: Vec<String> = Vec::new();
        let toutes = Self::noms_bruts(&self.conductor, STANDARD_CHEF)
            .chain(Self::noms_bruts(&self.ensemble, ""))
            .chain(Self::noms_bruts(&self.work, STANDARD_OEUVRE))
            .chain(Self::noms_bruts(&self.movement, STANDARD_MOUVEMENT));
        for c in toutes {
            if !cles.contains(&c) {
                cles.push(c);
            }
        }
        cles
    }

    /// Complète `meta` par les clés brutes LIBRES du chef, de l'œuvre et du
    /// mouvement. Le chef cumule ses valeurs (« ; ») ; l'œuvre et le
    /// mouvement ne prennent la première trouvée que si la lecture standard
    /// n'a rien donné. L'ensemble est l'affaire de
    /// [`super::credits_balises::completer_par_les_cles_brutes`].
    pub fn completer_par_les_noms_libres(
        &self,
        meta: &mut HashMap<String, String>,
        brutes: &[(String, String)],
    ) {
        for nom in Self::noms_bruts(&self.conductor, STANDARD_CHEF) {
            for (cle, valeur) in brutes {
                if nom_de_cle(cle) == nom {
                    for v in valeur.split(';') {
                        super::credits_balises::ajouter(meta, "conductor", v);
                    }
                }
            }
        }
        for (role, liste, standard) in [
            ("work", &self.work, STANDARD_OEUVRE),
            ("movement", &self.movement, STANDARD_MOUVEMENT),
        ] {
            if meta.get(role).is_some_and(|v| !v.trim().is_empty()) {
                continue;
            }
            let trouvee = Self::noms_bruts(liste, standard).find_map(|nom| {
                brutes
                    .iter()
                    .filter(|(cle, _)| nom_de_cle(cle) == nom)
                    .map(|(_, v)| v.trim())
                    .find(|v| !v.is_empty())
                    .map(str::to_owned)
            });
            if let Some(v) = trouvee {
                meta.insert(role.to_owned(), v);
            }
        }
    }

    /// Toutes les clés (sans préfixe) qu'une lecture selon CE réglage peut
    /// prendre, standard compris (avec leurs graphies de conteneur) et BAND.
    /// La relecture après modification n'efface une valeur que si le
    /// fichier la porte sous l'une d'elles (voir `relecture_classique`).
    pub fn cles_candidates(&self) -> Vec<String> {
        let mut cles = self.cles_brutes();
        let mut pousser = |c: &str| {
            if !cles.iter().any(|x| x == c) {
                cles.push(c.to_owned());
            }
        };
        pousser(super::credits_balises::CLE_BAND);
        if Self::lit_standard(&self.conductor, STANDARD_CHEF) {
            for c in ["CONDUCTOR", "TPE3"] {
                pousser(c);
            }
        }
        if Self::lit_standard(&self.work, STANDARD_OEUVRE) {
            for c in ["WORK", "WORKTITLE", "©WRK", "TIT1"] {
                pousser(c);
            }
        }
        if Self::lit_standard(&self.movement, STANDARD_MOUVEMENT) {
            for c in ["MOVEMENTNAME", "MVNM", "©MVN"] {
                pousser(c);
            }
        }
        cles
    }
}

static COURANTE: LazyLock<RwLock<Arc<BalisesClassique>>> =
    LazyLock::new(|| RwLock::new(Arc::new(BalisesClassique::default())));

/// Le réglage en vigueur (miroir en mémoire, sans requête).
pub fn courante() -> Arc<BalisesClassique> {
    COURANTE
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

fn installer(r: BalisesClassique) {
    match COURANTE.write() {
        Ok(mut g) => *g = Arc::new(r),
        Err(e) => *e.into_inner() = Arc::new(r),
    }
}

/// Le réglage tel qu'en base ; les valeurs par défaut s'il n'y est pas, ou
/// s'il est illisible (journalisé).
pub fn lire(backend: &Arc<dyn DbBackend>) -> BalisesClassique {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten();
    let Some(brut) = brut else {
        return BalisesClassique::default();
    };
    match serde_json::from_str::<Saisie>(&brut)
        .map_err(|e| e.to_string())
        .and_then(Saisie::valider)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "balises_classique_reglage_illisible");
            BalisesClassique::default()
        }
    }
}

/// Au démarrage, AVANT le premier scan : le miroir prend le réglage en base.
pub fn charger(backend: &Arc<dyn DbBackend>) {
    installer(lire(backend));
}

/// Valide, enregistre et installe `saisie`. Rend le réglage d'avant et
/// celui d'après.
pub fn ecrire(
    backend: &Arc<dyn DbBackend>,
    saisie: Saisie,
) -> Result<(BalisesClassique, BalisesClassique), String> {
    let nouveau = saisie.valider()?;
    let ancien = lire(backend);
    let settings = SettingsRepo::with_backend(backend.clone());
    if nouveau == BalisesClassique::default() {
        // Revenir aux défauts efface la clé : une future évolution des
        // défauts atteindra aussi qui n'a jamais rien choisi d'autre.
        settings.delete(CLE_REGLAGE)?;
    } else {
        let json = serde_json::to_string(&nouveau).map_err(|e| e.to_string())?;
        settings.set(CLE_REGLAGE, &json)?;
    }
    installer(nouveau.clone());
    Ok((ancien, nouveau))
}

#[cfg(test)]
#[path = "balises_classique_tests.rs"]
mod tests;
