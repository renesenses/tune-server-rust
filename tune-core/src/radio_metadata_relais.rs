//! Titre en cours des radios suivies par radio-metadata.fr, lu sur le relais de
//! mozaiklabs.fr (#6516).
//!
//! # Ce qui a été décidé le 13/10/2026 avec Rémi (radio-metadata.fr)
//!
//! * Tune n'interroge JAMAIS radio-metadata.fr. Seul le relais de mozaiklabs.fr
//!   le fait, une requête toutes les 3 à 5 s pour toutes les stations, quel que
//!   soit le nombre d'auditeurs. La clé d'API de radio-metadata.fr ne vit que
//!   dans le relais : ce module n'en porte aucune.
//! * Tune interroge le relais toutes les `RADIO_METADATA_CLIENT_REFRESH_PERIOD`
//!   secondes : 5 par défaut, réglable de 5 à 10.
//! * radio-metadata.fr passe AVANT le code propre à chaque diffuseur (Radio
//!   France, Radio Paradise, BBC…), qui reste le repli, puis l'ICY.
//!
//! # Ce que Tune envoie
//!
//! La seule clé radio-metadata de la station écoutée (`fip`, `fipjazz`…), prise
//! dans l'annuaire public. Ni compte, ni profil, ni identifiant de serveur :
//! aucune donnée personnelle n'est ajoutée à la requête.
//!
//! # Les stations suivies
//!
//! Le registre ([`publier_les_cles`]) associe une adresse de flux normalisée,
//! ou un nom de station replié, à une clé radio-metadata. Il est rempli par le
//! serveur à chaque relevé de l'annuaire. Une station absente du registre ne
//! touche jamais le relais : elle suit le parcours d'avant, à l'identique.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::info;

use crate::radio_metadata::IcyMetadata;

/// La route du relais sur mozaiklabs.fr (site-mozaiklabs, `RadioMetadataRelayController`).
pub const RELAIS_URL: &str = "https://mozaiklabs.fr/api/v1/radio-metadata/now-playing";

/// Variable qui règle la cadence de lecture du relais (décision du 13/10).
pub const VARIABLE_PERIODE: &str = "RADIO_METADATA_CLIENT_REFRESH_PERIOD";

/// Bornes de la cadence, en secondes : 5 par défaut, jamais en dessous, au plus 10.
pub const PERIODE_MIN_S: u64 = 5;
pub const PERIODE_MAX_S: u64 = 10;

/// Délai d'une lecture du relais : plus court que la période, pour qu'une
/// lecture lente ne chevauche jamais la suivante.
const DELAI_LECTURE: Duration = Duration::from_secs(4);

/// Après un relais muet (plusieurs échecs de suite, ou relais inactif faute de
/// clé d'API), on ne le réinterroge pas avant ce délai. Sans cette pause, chaque station suivie
/// paierait un aller-retour perdu toutes les 5 s avant d'atteindre son repli.
const PAUSE_APRES_PANNE: Duration = Duration::from_secs(600);

/// La cadence demandée par `valeur` (le contenu de la variable), bornée.
///
/// Absente, vide ou illisible : 5 s. Un `3` devient 5 (le relais lui-même ne
/// relit radio-metadata.fr que toutes les 3 à 5 s), un `30` devient 10.
pub fn periode_depuis(valeur: Option<&str>) -> Duration {
    let s = valeur
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(PERIODE_MIN_S)
        .clamp(PERIODE_MIN_S, PERIODE_MAX_S);
    Duration::from_secs(s)
}

/// La cadence du serveur, lue UNE fois dans l'environnement (le sondeur la
/// demande à chaque tour).
pub fn periode_client() -> Duration {
    static PERIODE: LazyLock<Duration> =
        LazyLock::new(|| periode_depuis(std::env::var(VARIABLE_PERIODE).ok().as_deref()));
    *PERIODE
}

/// Deux écritures d'une même adresse de flux ramenées à une seule.
///
/// Insensible au schéma, à la casse et au `/` final — rien de plus. C'est LA
/// règle du serveur (`routes/radios.rs` l'appelle) : une seule copie, pour que
/// le rattrapage des logos, la porte anti-doublon et ce registre rapprochent
/// toujours les mêmes adresses.
pub fn flux_normalise(url: &str) -> String {
    let sans_bords = url.trim().trim_end_matches('/').to_ascii_lowercase();
    sans_bords
        .strip_prefix("https://")
        .or_else(|| sans_bords.strip_prefix("http://"))
        .unwrap_or(&sans_bords)
        .to_string()
}

/// Un nom de station replié : sans accents, sans casse, sans ponctuation.
///
/// « FIP Nouveautés » et « FIP: Nouveautés » donnent tous deux `fipnouveautes`.
pub fn nom_replie(nom: &str) -> String {
    crate::db::engine::fold_diacritics(nom)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Une station de l'annuaire suivie par radio-metadata.fr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationSuivie {
    /// Toutes les adresses de flux connues de la fiche.
    pub flux: Vec<String>,
    pub nom: String,
    pub cle: String,
    /// L'identifiant de la fiche dans l'annuaire (`S000973`), quand il est
    /// connu : c'est par lui que se retrouve d'abord une station de « Mes
    /// radios » liée à sa fiche (`radio_stations.external_id`, #6516).
    pub external_id: Option<String>,
}

/// Les stations suivies, rangées par identifiant de fiche, par adresse de
/// flux et par nom.
#[derive(Debug, Default, Clone)]
pub struct Registre {
    par_id: HashMap<String, String>,
    par_flux: HashMap<String, String>,
    par_nom: HashMap<String, String>,
}

impl Registre {
    /// Le registre d'un relevé de l'annuaire.
    ///
    /// Un nom porté par DEUX clés différentes n'est pas retenu : le
    /// rapprochement par nom ne devine pas. L'adresse de flux, elle, désigne
    /// une seule station.
    pub fn depuis(stations: &[StationSuivie]) -> Self {
        let mut reg = Self::default();
        reg.ajouter(stations);
        reg
    }

    fn ajouter(&mut self, stations: &[StationSuivie]) {
        let mut noms_ambigus: std::collections::HashSet<String> = Default::default();
        for s in stations {
            let cle = s.cle.trim();
            if cle.is_empty() {
                continue;
            }
            if let Some(id) = s
                .external_id
                .as_deref()
                .map(str::trim)
                .filter(|i| !i.is_empty())
            {
                self.par_id
                    .entry(id.to_string())
                    .or_insert_with(|| cle.to_string());
            }
            for f in &s.flux {
                let n = flux_normalise(f);
                if !n.is_empty() {
                    self.par_flux.entry(n).or_insert_with(|| cle.to_string());
                }
            }
            let nom = nom_replie(&s.nom);
            if nom.is_empty() || noms_ambigus.contains(&nom) {
                continue;
            }
            match self.par_nom.get(&nom) {
                Some(existante) if existante != cle => {
                    self.par_nom.remove(&nom);
                    noms_ambigus.insert(nom);
                }
                Some(_) => {}
                None => {
                    self.par_nom.insert(nom, cle.to_string());
                }
            }
        }
    }

    /// La clé radio-metadata d'une station, par son adresse de flux d'abord,
    /// par son nom ensuite — ou `None` : la station n'est pas suivie.
    pub fn cle_pour(&self, nom_station: &str, url_flux: &str) -> Option<String> {
        self.par_flux
            .get(&flux_normalise(url_flux))
            .or_else(|| self.par_nom.get(&nom_replie(nom_station)))
            .cloned()
    }

    /// La clé radio-metadata d'une station de « Mes radios », dans l'ordre :
    /// son lien `external_id` d'abord, son adresse de flux ensuite, son nom
    /// enfin.
    ///
    /// Une station LIÉE à une fiche que le registre ne suit pas (fiche sans
    /// clé, ou sortie de l'annuaire) ne se rapproche plus par son NOM : elle a
    /// déjà sa fiche, et le nom pourrait désigner une autre station. Seule
    /// l'adresse exacte reste admise.
    pub fn cle_pour_lien(
        &self,
        external_id: Option<&str>,
        nom_station: &str,
        url_flux: &str,
    ) -> Option<String> {
        match external_id.map(str::trim).filter(|i| !i.is_empty()) {
            Some(id) => self
                .par_id
                .get(id)
                .or_else(|| self.par_flux.get(&flux_normalise(url_flux)))
                .cloned(),
            None => self.cle_pour(nom_station, url_flux),
        }
    }

    pub fn nombre_de_flux(&self) -> usize {
        self.par_flux.len()
    }
}

static REGISTRE: LazyLock<RwLock<Registre>> = LazyLock::new(|| RwLock::new(Registre::default()));

/// Remplace les stations suivies par celles du dernier relevé de l'annuaire.
pub fn publier_les_cles(stations: &[StationSuivie]) {
    let reg = Registre::depuis(stations);
    info!(
        flux = reg.par_flux.len(),
        noms = reg.par_nom.len(),
        "radio_metadata_relais_stations_suivies"
    );
    if let Ok(mut g) = REGISTRE.write() {
        *g = reg;
    }
}

/// Ajoute des stations au registre du processus SANS effacer les autres :
/// réservé aux essais, qui tournent en parallèle dans le même binaire.
#[cfg(test)]
pub(crate) fn ajouter_les_cles_pour_un_essai(stations: &[StationSuivie]) {
    if let Ok(mut g) = REGISTRE.write() {
        g.ajouter(stations);
    }
}

/// La clé radio-metadata d'une station dans le registre du processus.
pub fn cle_pour(nom_station: &str, url_flux: &str) -> Option<String> {
    REGISTRE.read().ok()?.cle_pour(nom_station, url_flux)
}

/// [`Registre::cle_pour_lien`] dans le registre du processus.
pub fn cle_pour_lien(
    external_id: Option<&str>,
    nom_station: &str,
    url_flux: &str,
) -> Option<String> {
    REGISTRE
        .read()
        .ok()?
        .cle_pour_lien(external_id, nom_station, url_flux)
}

/// Ce que le relais dit d'une station, réduit à ce que Tune affiche.
///
/// Forme attendue (site-mozaiklabs) :
/// `{"active":true,"items":{"fip":{"mode":"song","title":…,"artist":…,"cover_url":…}}}`.
pub fn lire_l_entree(corps: &Value, cle: &str) -> Option<IcyMetadata> {
    if corps.get("active").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let item = corps.get("items")?.get(cle)?;
    let texte = |k: &str| {
        item.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mode = texte("mode").unwrap_or_default().to_ascii_lowercase();
    if mode != "song" && mode != "program" {
        return None;
    }
    let title = texte("title")?;
    // En mode « émission », radio-metadata.fr répète souvent le nom de
    // l'émission (ou de la station) dans `artist` : l'afficher deux fois
    // n'apprend rien.
    let artist = texte("artist").filter(|a| !(mode == "program" && *a == title));
    Some(IcyMetadata {
        title,
        artist,
        station: texte("station"),
        cover_url: texte("cover_url")
            .filter(|u| u.starts_with("https://") || u.starts_with("http://")),
    })
}

// ---------------------------------------------------------------------------
// État du relais et cadence par zone
// ---------------------------------------------------------------------------

/// Fin de la pause, par racine de relais. Par racine et non globale : deux
/// relais (le vrai, celui d'un essai) ne se mettent pas en pause l'un l'autre.
static PANNES: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Le relais `base` est-il en pause après une panne ?
pub fn relais_en_pause(base: &str) -> bool {
    PANNES
        .lock()
        .ok()
        .and_then(|g| g.get(base).copied())
        .is_some_and(|t| Instant::now() < t)
}

/// Échecs consécutifs par racine de relais, depuis la dernière lecture réussie.
static ECHECS: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Nombre d'échecs de suite (délai dépassé, connexion, statut non 2xx, corps
/// illisible) avant de mettre le relais en pause. Un échec isolé ne coupe plus
/// l'enrichissement pour dix minutes (#7010) : la lecture suivante est tentée.
const ECHECS_AVANT_PAUSE: u32 = 3;

/// Note le résultat d'une lecture. `panne` : la lecture a échoué ; `immediat` :
/// le relais a dit lui-même qu'il est inactif (`active: false`, faute de clé
/// d'API), la pause est alors posée tout de suite. Une lecture réussie remet
/// le compte à zéro. Rend `true` quand la pause vient d'être posée.
fn noter_panne(base: &str, panne: bool, immediat: bool) -> bool {
    let Ok(mut pannes) = PANNES.lock() else {
        return false;
    };
    let Ok(mut echecs) = ECHECS.lock() else {
        return false;
    };
    if !panne {
        echecs.remove(base);
        pannes.remove(base);
        return false;
    }
    let n = echecs.entry(base.to_string()).or_insert(0);
    *n += 1;
    if immediat || *n >= ECHECS_AVANT_PAUSE {
        echecs.remove(base);
        pannes.insert(base.to_string(), Instant::now() + PAUSE_APRES_PANNE);
        true
    } else {
        false
    }
}

/// Le titre d'une station suivie, si le relais le donne ; `None` sinon, et la
/// pause est posée quand le relais lui-même est en défaut (relais inactif, ou
/// `ECHECS_AVANT_PAUSE` échecs de suite).
///
/// Une station suivie absente de la réponse n'est pas une panne : le relais a
/// répondu, il ne sait rien d'elle à cet instant.
pub async fn titre_par_le_relais(base: &str, cle: &str) -> Option<IcyMetadata> {
    if relais_en_pause(base) {
        return None;
    }
    let reponse = crate::http::client::shared()
        .get(base)
        .query(&[("keys", cle)])
        .timeout(DELAI_LECTURE)
        .send()
        .await;
    let reponse = match reponse {
        Ok(r) => r,
        Err(e) => {
            let en_pause = noter_panne(base, true, false);
            info!(
                cle,
                erreur = %e,
                delai_depasse = e.is_timeout(),
                connexion = e.is_connect(),
                en_pause,
                "radio_metadata_relais_lecture_echouee"
            );
            if en_pause {
                info!(cle, "radio_metadata_relais_injoignable_pause");
            }
            return None;
        }
    };
    let statut = reponse.status();
    let corps = reponse.json::<Value>().await.ok();
    let inactif = corps
        .as_ref()
        .and_then(|c| c.get("active"))
        .and_then(Value::as_bool)
        == Some(false);
    if inactif {
        noter_panne(base, true, true);
        info!(status = %statut, cle, "radio_metadata_relais_inactif_pause");
        return None;
    }
    let Some(corps) = corps.filter(|_| statut.is_success()) else {
        let en_pause = noter_panne(base, true, false);
        info!(
            status = %statut,
            cle,
            en_pause,
            "radio_metadata_relais_lecture_echouee"
        );
        return None;
    };
    noter_panne(base, false, false);
    lire_l_entree(&corps, cle)
}

static ZONES_SUIVIES: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Note le dernier rafraîchissement d'une zone : `par_le_relais` dit si son
/// titre vient du relais. Seules ces zones-là passent à la cadence du relais ;
/// les autres gardent celle du sondeur.
pub fn noter_zone(zone_id: i64, par_le_relais: bool) {
    if let Ok(mut g) = ZONES_SUIVIES.lock() {
        if par_le_relais {
            g.insert(zone_id, Instant::now());
        } else {
            g.remove(&zone_id);
        }
    }
}

/// La zone suivie par le relais doit-elle être relue maintenant ?
pub fn zone_a_relire(zone_id: i64, periode: Duration) -> bool {
    ZONES_SUIVIES
        .lock()
        .ok()
        .and_then(|g| g.get(&zone_id).copied())
        .is_some_and(|dernier| dernier.elapsed() >= periode)
}

#[cfg(test)]
#[path = "radio_metadata_relais_tests.rs"]
mod tests;
