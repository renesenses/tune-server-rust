//! Cover+ (#8452) — la pochette du titre en cours d'une webradio qui n'en
//! publie pas, retrouvée d'après sa métadonnée ICY et l'API iTunes Search.
//!
//! Proposition de radio-metadata.fr du 11/11/2026, retenue par Bertrand pour
//! la rc13. Spécification et code de référence :
//! `cover-plus-specification.md`, `pretraitement.py`, `itunes_choix.py`,
//! `photo_artiste.py`. Ce module en est le portage en Rust ; aucun Python
//! n'est livré.
//!
//! ## Le parcours
//!
//! 1. Le sondeur radio lit le titre en cours (ICY du flux joué, ou API du
//!    diffuseur) — la lecture existante, sans seconde connexion.
//! 2. **La station donne déjà une pochette** : rien n'est cherché.
//! 3. Cover+ éteint (globalement ou pour la station) : rien n'est cherché.
//! 4. Sinon, la chaîne est pré-traitée ([`pretraitement::preparer`]) ; une
//!    publicité, un slogan ou un jingle ne déclenchent aucun appel.
//! 5. La recherche part EN TÂCHE DE FOND : le sondeur ne l'attend jamais.
//!    Tant qu'elle n'a pas abouti, l'écran garde le logo de la station — la
//!    pochette du titre précédent ne reste jamais sur le nouveau (§ 4).
//! 6. Le résultat, trouvé ou non, est mis en cache : une chaîne déjà vue
//!    n'est jamais recherchée deux fois (§ 3.2).
//!
//! ## Le débit
//!
//! iTunes tolère environ 20 appels par minute et par adresse IP. Le serveur
//! Tune tourne chez l'auditeur : ses appels partent donc de SON adresse,
//! jamais d'un service central (§ 3.1). Ils passent par le limiteur iTunes
//! PARTAGÉ du serveur ([`crate::library::images_rapides::Hote::Itunes`], un
//! appel toutes les 3 s, soit 20 par minute), le même que la passe des
//! pochettes de la bibliothèque : les deux ensemble ne dépassent pas le
//! plafond. Le client HTTP est le client partagé, jamais un client propre.
//!
//! ## Le repli « photo de l'artiste »
//!
//! Sans pochette concordante, Cover+ cherche une photo de l'artiste (§ 3.6,
//! [`photo_artiste`]). Elle se lit dans la balise `og:image` d'une page web de
//! music.apple.com, PAS dans une API : si Apple change sa page, ce repli cesse
//! de fonctionner sans prévenir. Il a donc son propre interrupteur
//! ([`photo_artiste_active`]), qu'on coupe sans toucher au reste.
//!
//! ## Livré éteint
//!
//! Cover+ part éteint ([`actif_globalement`]) : réglage `cover_plus.actif`
//! (route `PUT /radios/cover-plus`), ou variable d'environnement
//! `TUNE_COVER_PLUS=1`. Une fois allumé, chaque station de « Mes radios » l'a
//! par défaut, et peut le couper (`PUT /radios/{id}/cover-plus`).

pub mod choix;
pub mod photo_artiste;
pub mod pretraitement;

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::Value;
use tracing::debug;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

use choix::{Disques, classer, concorde, discographie, norme};
use pretraitement::{Preparation, preparer};

/// La racine de l'API iTunes Search.
pub const ITUNES_BASE: &str = "https://itunes.apple.com";

/// Taille des caches : chaînes ICY vues, discographies, photos d'artistes,
/// ordres appris par station.
const CACHE_TITRES_MAX: usize = 4096;
const CACHE_DISQUES_MAX: usize = 512;
const CACHE_PHOTOS_MAX: usize = 1024;
const CACHE_ORDRES_MAX: usize = 1024;

// ---------------------------------------------------------------------------
// Réglages
// ---------------------------------------------------------------------------

/// Le réglage qui ALLUME Cover+ sur ce serveur. Absent : éteint.
pub const CLE_ACTIF: &str = "cover_plus.actif";
/// Le réglage du repli « photo de l'artiste ». Absent : actif (quand Cover+
/// l'est).
pub const CLE_PHOTO_ARTISTE: &str = "cover_plus.photo_artiste";
/// Variable d'environnement qui prime sur [`CLE_ACTIF`] : `1` ou `0`.
pub const ENV_ACTIF: &str = "TUNE_COVER_PLUS";
/// Variable d'environnement qui prime sur [`CLE_PHOTO_ARTISTE`] : `0` coupe
/// le repli sans redéployer si Apple change sa page.
pub const ENV_PHOTO_ARTISTE: &str = "TUNE_COVER_PLUS_PHOTO_ARTISTE";

fn lire_booleen(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "oui" | "yes" => Some(true),
        "0" | "false" | "off" | "non" | "no" => Some(false),
        _ => None,
    }
}

fn interrupteur(db: &Arc<dyn DbBackend>, cle: &str, env: &str, defaut: bool) -> bool {
    if let Some(v) = std::env::var(env).ok().as_deref().and_then(lire_booleen) {
        return v;
    }
    SettingsRepo::with_backend(db.clone())
        .get(cle)
        .ok()
        .flatten()
        .as_deref()
        .and_then(lire_booleen)
        .unwrap_or(defaut)
}

fn poser(db: &Arc<dyn DbBackend>, cle: &str, valeur: bool) -> Result<(), String> {
    SettingsRepo::with_backend(db.clone()).set(cle, if valeur { "1" } else { "0" })
}

/// Cover+ est-il allumé sur ce serveur ? Éteint par défaut (livré éteint).
pub fn actif_globalement(db: &Arc<dyn DbBackend>) -> bool {
    interrupteur(db, CLE_ACTIF, ENV_ACTIF, false)
}

/// Allumer ou éteindre Cover+ sur ce serveur.
pub fn regler_globalement(db: &Arc<dyn DbBackend>, actif: bool) -> Result<(), String> {
    poser(db, CLE_ACTIF, actif)
}

/// Le repli « photo de l'artiste » (§ 3.6) est-il permis ? Oui par défaut.
pub fn photo_artiste_active(db: &Arc<dyn DbBackend>) -> bool {
    interrupteur(db, CLE_PHOTO_ARTISTE, ENV_PHOTO_ARTISTE, true)
}

/// Permettre ou couper le repli « photo de l'artiste ».
pub fn regler_photo_artiste(db: &Arc<dyn DbBackend>, actif: bool) -> Result<(), String> {
    poser(db, CLE_PHOTO_ARTISTE, actif)
}

/// La clé de réglage qui COUPE Cover+ pour une station de « Mes radios ».
/// Absente : Cover+ est actif pour elle.
pub fn cle_reglage_station(station_id: i64) -> String {
    format!("cover_plus.station_off.{station_id}")
}

/// Cover+ est-il actif pour cette station (réglage de la station seul) ? Une
/// station hors de « Mes radios » (écoutée depuis l'annuaire) n'a pas de
/// réglage propre : actif.
pub fn actif_pour_la_station(db: &Arc<dyn DbBackend>, station_id: Option<i64>) -> bool {
    let Some(id) = station_id else {
        return true;
    };
    !matches!(
        SettingsRepo::with_backend(db.clone())
            .get(&cle_reglage_station(id))
            .ok()
            .flatten()
            .as_deref()
            .and_then(lire_booleen),
        Some(true)
    )
}

/// Couper (`false`) ou rétablir (`true`) Cover+ pour une station.
pub fn regler_la_station(
    db: &Arc<dyn DbBackend>,
    station_id: i64,
    actif: bool,
) -> Result<(), String> {
    let repo = SettingsRepo::with_backend(db.clone());
    if actif {
        repo.delete(&cle_reglage_station(station_id))
    } else {
        repo.set(&cle_reglage_station(station_id), "1")
    }
}

/// La boutique iTunes de la station : son pays (un code à deux lettres, ou
/// un nom en clair comme le catalogue les stocke), les États-Unis sinon.
pub fn boutique_de_la_station(pays: Option<&str>) -> String {
    let p = pays.map(str::trim).unwrap_or_default();
    if p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()) {
        p.to_ascii_uppercase()
    } else {
        crate::pays::code_pays(p).unwrap_or("US").to_string()
    }
}

/// La chaîne cherchée, telle que la station l'a publiée : « Artiste - Titre »
/// quand le lecteur ICY l'a déjà découpée sur le premier « - », le titre
/// seul sinon. Le lecteur ICY coupe sur le premier « - » et rien d'autre :
/// la recoller rend la chaîne d'origine.
pub fn chaine_du_titre(artiste: Option<&str>, titre: &str) -> String {
    match artiste.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{a} - {}", titre.trim()),
        None => titre.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Ce que Cover+ trouve
// ---------------------------------------------------------------------------

/// D'où vient l'image affichée avec le label « Cover+ » (§ 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origine {
    /// La pochette du morceau, chez iTunes (§ 3.3 à 3.5).
    Pochette,
    /// Une photo de l'artiste, repli (§ 3.6).
    PhotoArtiste,
    /// La pochette que le flux iHeart fournit lui-même (§ 3.2, étape 6).
    PochetteDuFlux,
}

impl Origine {
    /// Le code publié aux clients (`cover_plus_source`).
    pub fn code(self) -> &'static str {
        match self {
            Origine::Pochette => "itunes",
            Origine::PhotoArtiste => "artist_photo",
            Origine::PochetteDuFlux => "stream",
        }
    }
}

/// Une image retrouvée par Cover+.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trouvaille {
    pub url: String,
    pub origine: Origine,
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Une table bornée : au-delà de `max` entrées, la plus ancienne sort.
struct Borne<K, V> {
    carte: HashMap<K, V>,
    ordre: VecDeque<K>,
    max: usize,
}

impl<K: Eq + Hash + Clone, V> Borne<K, V> {
    fn new(max: usize) -> Self {
        Self {
            carte: HashMap::new(),
            ordre: VecDeque::new(),
            max,
        }
    }

    fn get(&self, k: &K) -> Option<&V> {
        self.carte.get(k)
    }

    fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.carte.get_mut(k)
    }

    fn insert(&mut self, k: K, v: V) {
        if self.carte.insert(k.clone(), v).is_none() {
            self.ordre.push_back(k);
            while self.ordre.len() > self.max {
                if let Some(vieille) = self.ordre.pop_front() {
                    self.carte.remove(&vieille);
                }
            }
        }
    }
}

struct Cache {
    /// Chaque chaîne ICY vue : sa trouvaille, ou `None` (échec retenu).
    titres: Borne<String, Option<Trouvaille>>,
    en_cours: HashSet<String>,
    /// Discographie d'un artiste, par boutique (§ 3.2, étape 4).
    disques: Borne<(i64, String), Disques>,
    /// Photo d'un artiste, par nom normalisé et boutique (§ 3.6).
    photos: Borne<(String, String), Option<String>>,
    /// Par station : combien de fois « artiste - titre » et « titre -
    /// artiste » ont concordé (§ 3.6, point 1).
    ordres: Borne<String, [u32; 2]>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    Mutex::new(Cache {
        titres: Borne::new(CACHE_TITRES_MAX),
        en_cours: HashSet::new(),
        disques: Borne::new(CACHE_DISQUES_MAX),
        photos: Borne::new(CACHE_PHOTOS_MAX),
        ordres: Borne::new(CACHE_ORDRES_MAX),
    })
});

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

fn cle_de_cache(boutique: &str, station: &str, chaine: &str) -> String {
    format!("{boutique}\u{1f}{}\u{1f}{chaine}", norme(station))
}

fn retenir_titre(cle: String, trouvaille: Option<Trouvaille>) {
    let mut c = cache();
    c.en_cours.remove(&cle);
    c.titres.insert(cle, trouvaille);
}

/// Noter qu'une hypothèse a concordé sur cette station : 0 pour « artiste -
/// titre » (1re partie, 2e partie), 1 pour l'inverse.
fn noter_ordre(station: &str, hypothese: usize) {
    if hypothese > 1 {
        return;
    }
    let cle = norme(station);
    let mut c = cache();
    if let Some(n) = c.ordres.get_mut(&cle) {
        n[hypothese] = n[hypothese].saturating_add(1);
        return;
    }
    let mut n = [0, 0];
    n[hypothese] = 1;
    c.ordres.insert(cle, n);
}

/// L'artiste pour le repli « photo » (§ 3.6, point 1) : la partie que
/// désigne l'ordre qui a le plus souvent réussi sur cette station, et à
/// défaut la 1re partie.
pub fn artiste_pour_la_photo(station: &str, prep: &Preparation) -> Option<String> {
    let (premiere, seconde) = prep.hypotheses.first()?;
    let n = cache().ordres.get(&norme(station)).copied().unwrap_or([0, 0]);
    Some(if n[1] > n[0] {
        seconde.clone()
    } else {
        premiere.clone()
    })
}

// ---------------------------------------------------------------------------
// Appels
// ---------------------------------------------------------------------------

/// Où et comment parler à iTunes. La racine est un paramètre pour que les
/// épreuves dressent un faux service : **aucun test n'appelle iTunes**.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub base: &'a str,
    /// Passer par le limiteur iTunes partagé. Toujours vrai hors épreuves.
    pub limiter: bool,
}

impl Source<'static> {
    pub const ITUNES: Self = Self {
        base: ITUNES_BASE,
        limiter: true,
    };
}

/// Une requête `GET`, par le limiteur iTunes partagé quand la source le
/// demande. La réponse si elle est un succès, `None` sinon.
async fn obtenir(
    source: Source<'_>,
    url: reqwest::Url,
    agent: Option<&str>,
) -> Option<reqwest::Response> {
    let (limiteur, cle) = crate::library::images_rapides::Hote::Itunes.limiteur();
    if source.limiter {
        limiteur.acquire(cle).await;
    }
    let mut requete = crate::http::client::shared().get(url);
    if let Some(a) = agent {
        requete = requete.header(reqwest::header::USER_AGENT, a);
    }
    let reponse = match requete.send().await {
        Ok(r) => r,
        Err(e) => {
            debug!(erreur = %e, "cover_plus_injoignable");
            return None;
        }
    };
    if source.limiter {
        limiteur.constater_reponse(cle, &reponse);
    }
    if !reponse.status().is_success() {
        debug!(statut = reponse.status().as_u16(), "cover_plus_refus");
        return None;
    }
    Some(reponse)
}

/// Un appel `GET {base}{chemin}?{params}` ; la liste `results`, ou `None`.
pub(crate) async fn appeler(
    source: Source<'_>,
    chemin: &str,
    params: &[(&str, &str)],
) -> Option<Vec<Value>> {
    let url = reqwest::Url::parse_with_params(&format!("{}{chemin}", source.base), params).ok()?;
    let corps: Value = obtenir(source, url, None).await?.json().await.ok()?;
    corps.get("results")?.as_array().cloned()
}

async fn chercher_morceaux(source: Source<'_>, requete: &str, boutique: &str) -> Vec<Value> {
    appeler(
        source,
        "/search",
        &[
            ("term", requete),
            ("entity", "song"),
            ("country", boutique),
            // 50, pas 25 : avec des mots courants, la bonne réponse est
            // parfois au-delà du 25e résultat (« Tool - The Pot »).
            ("limit", "50"),
        ],
    )
    .await
    .unwrap_or_default()
}

/// La discographie de l'artiste, une fois par artiste et par boutique.
async fn disques_de(source: Source<'_>, artiste_id: i64, boutique: &str) -> Option<Disques> {
    let cle = (artiste_id, boutique.to_string());
    if let Some(d) = cache().disques.get(&cle).cloned() {
        return Some(d);
    }
    let id = artiste_id.to_string();
    let albums = appeler(
        source,
        "/lookup",
        &[
            ("id", id.as_str()),
            ("entity", "album"),
            ("country", boutique),
            ("limit", "200"),
        ],
    )
    .await?;
    let d = discographie(&albums);
    cache().disques.insert(cle, d.clone());
    Some(d)
}

/// Les pochettes classées pour l'hypothèse (artiste, titre), la
/// discographie de l'artiste servant à dater les disques (§ 3.4).
async fn classer_avec_discographie(
    source: Source<'_>,
    resultats: &[Value],
    artiste: &str,
    titre: &str,
    boutique: &str,
) -> Vec<choix::Pochette> {
    let v = classer(resultats, artiste, titre, None);
    if v.is_empty() {
        return v;
    }
    let artiste_id = resultats
        .iter()
        .find(|x| {
            concorde(
                artiste,
                x.get("artistName").and_then(Value::as_str).unwrap_or(""),
            ) && concorde(
                titre,
                x.get("trackName").and_then(Value::as_str).unwrap_or(""),
            )
        })
        .and_then(|x| x.get("artistId").and_then(Value::as_i64));
    let Some(aid) = artiste_id else {
        return v;
    };
    match disques_de(source, aid, boutique).await {
        Some(d) => {
            let avec = classer(resultats, artiste, titre, Some(&d));
            if avec.is_empty() { v } else { avec }
        }
        None => v,
    }
}

/// Sans séparateur (§ 2.2, étape 7) : la chaîne doit être EXACTEMENT
/// « artiste titre » ou « titre artiste » une fois normalisée. Le titre de
/// référence est ce qui reste de la chaîne diffusée, pas le titre iTunes,
/// sinon « [Remix] » passerait pour voulu.
async fn sans_separateur(
    source: Source<'_>,
    texte: &str,
    resultats: &[Value],
    boutique: &str,
) -> Option<String> {
    let n = norme(texte);
    for x in resultats {
        let a = x.get("artistName").and_then(Value::as_str).unwrap_or("");
        let t = x.get("trackName").and_then(Value::as_str).unwrap_or("");
        if n != norme(&format!("{a} {t}")) && n != norme(&format!("{t} {a}")) {
            continue;
        }
        let reste = format!(" {n} ").replacen(&format!(" {} ", norme(a)), " ", 1);
        let reste = reste.split_whitespace().collect::<Vec<_>>().join(" ");
        let v = classer_avec_discographie(source, resultats, a, &reste, boutique).await;
        if let Some(p) = v.into_iter().next() {
            return Some(p.url);
        }
    }
    None
}

/// La pochette du morceau : la cascade d'appels de la spécification (§ 3.2,
/// étapes 1 à 4). La requête avec le remixeur ne part que dans la boutique
/// de la station ; les requêtes simples y partent d'abord, puis aux
/// États-Unis si la station n'y est pas et que rien n'a concordé.
async fn chercher_la_pochette(
    source: Source<'_>,
    prep: &Preparation,
    boutique: &str,
    station: &str,
) -> Option<String> {
    let mut boutiques = vec![boutique.to_string()];
    if boutique != "US" {
        boutiques.push("US".into());
    }
    for (k, b) in boutiques.iter().enumerate() {
        for (i, requete) in prep.requetes.iter().enumerate() {
            if requete.trim().is_empty() || (k > 0 && i == 0 && prep.avec_remixeur) {
                continue;
            }
            let resultats = chercher_morceaux(source, requete, b).await;
            if resultats.is_empty() {
                continue;
            }
            if prep.hypotheses.is_empty() {
                if let Some(url) = sans_separateur(source, &prep.texte, &resultats, b).await {
                    return Some(url);
                }
                continue;
            }
            for (h, (a, t)) in prep.hypotheses.iter().enumerate() {
                let v = classer_avec_discographie(source, &resultats, a, t, b).await;
                if let Some(p) = v.into_iter().next() {
                    noter_ordre(station, h);
                    return Some(p.url);
                }
            }
        }
    }
    None
}

/// La photo de l'artiste, une fois par artiste et par boutique.
async fn photo_en_cache(source: Source<'_>, nom: &str, boutique: &str) -> Option<String> {
    let cle = (norme(nom), boutique.to_string());
    if let Some(p) = cache().photos.get(&cle).cloned() {
        return p;
    }
    let p = photo_artiste::photo_artiste(source, nom, boutique).await;
    debug!(trouvee = p.trouvee(), raison = p.raison, "cover_plus_photo_artiste");
    let url = p.url;
    cache().photos.insert(cle, url.clone());
    url
}

/// La cascade complète pour une chaîne déjà préparée (§ 3.2) : la pochette
/// du morceau ; à défaut, la photo de l'artiste quand le repli est permis
/// et que la chaîne avait un séparateur ; à défaut, la pochette fournie par
/// le flux iHeart.
pub async fn chercher(
    source: Source<'_>,
    prep: &Preparation,
    boutique: &str,
    station: &str,
    photo_permise: bool,
) -> Option<Trouvaille> {
    if prep.ignorer.is_some() {
        return None;
    }
    if let Some(url) = chercher_la_pochette(source, prep, boutique, station).await {
        return Some(Trouvaille {
            url,
            origine: Origine::Pochette,
        });
    }
    // Pas de photo pour une chaîne sans séparateur : on ne sait pas qui est
    // l'artiste (§ 2.2, étape 7 ; § 3.6, point 5).
    if photo_permise
        && let Some(nom) = artiste_pour_la_photo(station, prep)
        && let Some(url) = photo_en_cache(source, &nom, boutique).await
    {
        return Some(Trouvaille {
            url,
            origine: Origine::PhotoArtiste,
        });
    }
    prep.pochette_fournie.clone().map(|url| Trouvaille {
        url,
        origine: Origine::PochetteDuFlux,
    })
}

// ---------------------------------------------------------------------------
// Le point d'entrée du sondeur
// ---------------------------------------------------------------------------

/// Ce que le sondeur sait de la station et du titre en cours.
#[derive(Debug, Clone)]
pub struct TitreEnCours<'a> {
    pub station_id: Option<i64>,
    pub station_nom: &'a str,
    pub station_pays: Option<&'a str>,
    pub artiste: Option<&'a str>,
    pub titre: &'a str,
    /// La pochette que la station publie elle-même pour ce titre.
    pub pochette_station: Option<&'a str>,
}

/// Ce que Cover+ décide pour le titre en cours, sans jamais attendre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Rien à faire : la station donne sa pochette, Cover+ est éteint, ou la
    /// chaîne n'est pas un morceau.
    Rien,
    /// L'image est connue.
    Pochette(Trouvaille),
    /// Déjà cherchée, sans succès : le logo de la station.
    Introuvable,
    /// Recherche lancée (ou déjà en cours) : le logo en attendant.
    EnCours,
}

/// Ce qu'il faut pour lancer une recherche.
struct ARechercher {
    cle: String,
    prep: Preparation,
    boutique: String,
    photo_permise: bool,
}

/// La décision, sans réseau : `Ok` si elle est déjà prise, `Err` s'il faut
/// chercher.
fn decider(db: &Arc<dyn DbBackend>, t: &TitreEnCours<'_>) -> Result<Decision, Box<ARechercher>> {
    if t.pochette_station.is_some_and(|p| !p.trim().is_empty()) {
        return Ok(Decision::Rien);
    }
    if !actif_globalement(db) || !actif_pour_la_station(db, t.station_id) {
        return Ok(Decision::Rien);
    }
    let chaine = chaine_du_titre(t.artiste, t.titre);
    let prep = preparer(&chaine, t.station_nom);
    if prep.ignorer.is_some() {
        return Ok(Decision::Rien);
    }
    let boutique = boutique_de_la_station(t.station_pays);
    let cle = cle_de_cache(&boutique, t.station_nom, &chaine);
    {
        let mut c = cache();
        let connue = c.titres.get(&cle).cloned();
        match connue {
            Some(Some(trouvaille)) => return Ok(Decision::Pochette(trouvaille)),
            Some(None) => return Ok(Decision::Introuvable),
            None if c.en_cours.contains(&cle) => return Ok(Decision::EnCours),
            None => {
                c.en_cours.insert(cle.clone());
            }
        }
    }
    Err(Box::new(ARechercher {
        cle,
        prep,
        boutique,
        photo_permise: photo_artiste_active(db),
    }))
}

/// Le point d'entrée du sondeur radio : l'image Cover+ du titre en cours si
/// elle est déjà connue, sinon `EnCours` et une recherche en tâche de fond.
pub fn pochette_du_titre(db: &Arc<dyn DbBackend>, t: &TitreEnCours<'_>) -> Decision {
    // Les épreuves de `tune-core` passent par ici sans le savoir (le sondeur
    // radio) : elles parlent à une adresse morte, jamais à iTunes.
    #[cfg(test)]
    let source = Source {
        base: "http://127.0.0.1:9",
        limiter: false,
    };
    #[cfg(not(test))]
    let source = Source::ITUNES;
    pochette_du_titre_par(db, t, source)
}

/// Pose une trouvaille dans le cache, comme une recherche aboutie : les
/// épreuves du sondeur ne cherchent pas, elles constatent ce qu'il affiche.
#[cfg(test)]
pub(crate) fn semer(
    pays: Option<&str>,
    station: &str,
    artiste: &str,
    titre: &str,
    trouvaille: Option<Trouvaille>,
) {
    let cle = cle_de_cache(
        &boutique_de_la_station(pays),
        station,
        &chaine_du_titre(Some(artiste), titre),
    );
    retenir_titre(cle, trouvaille);
}

fn pochette_du_titre_par(
    db: &Arc<dyn DbBackend>,
    t: &TitreEnCours<'_>,
    source: Source<'static>,
) -> Decision {
    match decider(db, t) {
        Ok(d) => d,
        Err(a) => {
            let ARechercher {
                cle,
                prep,
                boutique,
                photo_permise,
            } = *a;
            let station = t.station_nom.to_string();
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(async move {
                        let trouvee =
                            chercher(source, &prep, &boutique, &station, photo_permise).await;
                        debug!(
                            station = %station,
                            origine = trouvee.as_ref().map(|t| t.origine.code()),
                            "cover_plus_recherche_terminee"
                        );
                        retenir_titre(cle, trouvee);
                    });
                }
                Err(_) => {
                    cache().en_cours.remove(&cle);
                }
            }
            Decision::EnCours
        }
    }
}

#[cfg(test)]
#[path = "choix_tests.rs"]
mod choix_tests;
#[cfg(test)]
#[path = "photo_artiste_tests.rs"]
mod photo_artiste_tests;
#[cfg(test)]
#[path = "pretraitement_tests.rs"]
mod pretraitement_tests;
#[cfg(test)]
#[path = "pretraitement_parite_tests.rs"]
mod pretraitement_parite_tests;
#[cfg(test)]
#[path = "recherche_tests.rs"]
mod recherche_tests;
