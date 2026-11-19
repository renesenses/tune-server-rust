//! #9158 (Rob, réunion du 18/11, go de Bertrand le 19/11) — réglage
//! « Balises personnelles → tags Tune » : QUELLES balises libres des fichiers
//! deviennent des tags Tune au scan, et comment.
//!
//! Jusqu'ici, le scan écartait tout ce que lofty ne range sous aucun
//! `ItemKey` : `TXXX` de description libre, clés Vorbis non standard, atomes
//! MP4 libres (`----:com.apple.iTunes:…`). Le tiroir des balises les
//! montrait, piste par piste, sans rien en garder.
//!
//! # Une correspondance
//!
//! - **la clé** lue dans le fichier, sans casse et sans préfixe de conteneur
//!   ([`super::balises_brutes::nom_de_cle`]) : `MOOD` lit le Vorbis `MOOD`,
//!   l'ID3 `TXXX:MOOD`, l'atome `----:com.apple.iTunes:MOOD` et l'APE `Mood` ;
//!   `TXXX:MOOD` saisi tel quel vaut `MOOD` ;
//! - **le mode** : [`Mode::Values`], chaque valeur devient un tag
//!   (`MOOD=Calme;Nuit` → « Calme » et « Nuit », chaque trame d'une clé
//!   répétée aussi), avec un préfixe facultatif (« Humeur : Calme ») ; ou
//!   [`Mode::Fixed`], un tag FIXE posé dès que la balise porte une valeur.
//!
//! # Aucun coût sans réglage
//!
//! Le réglage par défaut est VIDE : le scan ne lit rien de plus, n'écrit
//! rien, ne touche à aucun tag. Le comportement d'avant #9158 est intact.
//!
//! # Le passage au scan
//!
//! [`super::read_extended_metadata`] range les noms de tags trouvés sous la
//! clé RÉSERVÉE [`CLE_LUE`] de sa table de métadonnées étendues — jamais
//! écrite dans `track_metadata` (`TrackMetadataRepo` écarte les clés
//! réservées) — et `db::tags_perso_repo::ecrire_lot` les pose en tags Tune
//! dans la même boucle que les crédits des balises. La clé est présente
//! (éventuellement vide) dès qu'une correspondance existe : c'est ce qui
//! permet de RETIRER un tag dont la balise a quitté le fichier.
//!
//! # Où le réglage vit
//!
//! En base, sous la clé [`CLE_REGLAGE`] de `settings` (JSON). En mémoire, un
//! miroir ([`courante`]) que le scan lit à chaque fichier sans requête :
//! [`charger`] le remplit au démarrage, [`ecrire`] à chaque modification.
//! Même régime que « Balises du classique » (#8325).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Serialize};

use super::balises_brutes::nom_de_cle;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé de `settings` qui porte le réglage (JSON).
pub const CLE_REGLAGE: &str = "personal_tags";

/// La clé RÉSERVÉE des métadonnées étendues qui porte les tags lus, joints
/// par [`SEPARATEUR_LU`]. Le `@` initial la marque réservée : aucune balise
/// ne produit une telle clé, et `TrackMetadataRepo` ne l'écrit jamais.
pub const CLE_LUE: &str = "@tags_perso";
/// Le séparateur des noms de tags sous [`CLE_LUE`] (US, jamais dans un nom :
/// [`nettoyer_nom_de_tag`] retire les caractères de contrôle).
pub const SEPARATEUR_LU: char = '\u{1f}';

/// Au plus tant de correspondances.
pub const MAX_CORRESPONDANCES: usize = 32;
/// Au plus tant de caractères par nom de balise.
pub const MAX_LONGUEUR_CLE: usize = 64;
/// Au plus tant de caractères par nom de tag (préfixe compris).
pub const MAX_LONGUEUR_TAG: usize = 80;
/// Au plus tant de caractères de préfixe.
pub const MAX_LONGUEUR_PREFIXE: usize = 32;
/// Au plus tant de tags tirés d'un fichier : une balise de 2 000 valeurs ne
/// crée pas 2 000 tags.
pub const MAX_TAGS_PAR_PISTE: usize = 64;

/// Ce qu'une correspondance fait de la balise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Chaque valeur (découpée sur `;`, chaque trame) devient un tag.
    #[default]
    Values,
    /// Un tag fixe, posé dès que la balise porte une valeur non vide.
    Fixed,
}

/// Une correspondance « clé de fichier → tag(s) Tune ».
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correspondance {
    /// La clé, en capitales (`MOOD`, `TXXX:MOOD`, `----:COM.APPLE.ITUNES:MOOD`).
    pub key: String,
    #[serde(default)]
    pub mode: Mode,
    /// Le tag fixe ([`Mode::Fixed`] seulement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Le préfixe des tags ([`Mode::Values`] seulement) : « Humeur : ».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Le séparateur des valeurs d'UNE balise ([`Mode::Values`]) : absent =
    /// `;` (défaut), `""` = aucun, sinon `|`, `/` ou une chaîne choisie (au
    /// plus [`MAX_LONGUEUR_SEPARATEUR`] caractères). S'applique EN PLUS des
    /// champs répétés (deux trames `GROUPING`) et des valeurs multiples ID3v2.4
    /// (séparées par NUL), toujours découpés.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
}

/// Le séparateur par défaut des valeurs d'une balise.
pub const SEPARATEUR_DEFAUT: &str = ";";
/// Au plus tant de caractères de séparateur.
pub const MAX_LONGUEUR_SEPARATEUR: usize = 8;

/// Les balises proposées d'office (demande de Rob et usages courants :
/// Roon, foobar2000, Mp3tag).
pub const SUGGESTIONS: [&str; 5] = ["GROUPING", "MOOD", "STYLE", "ROONTRACKTAG", "ROONALBUMTAG"];

/// Les trames et atomes STANDARD qui portent le même champ qu'un nom libre :
/// le regroupement est `TIT1` / `GRP1` en ID3 et `©grp` en MP4, l'humeur
/// `TMOO` en ID3v2.4. Une correspondance `GROUPING` les lit aussi.
/// `TIT1` n'est le regroupement que sans `GRP1` : quand iTunes (12.5 et
/// suivants) écrit `GRP1`, `TIT1` est l'ŒUVRE (même règle que la lecture
/// étendue, `ItemKey::Work`).
fn alias_standard(nom: &str) -> &'static [&'static str] {
    match nom {
        "GROUPING" => &["GRP1", "\u{a9}GRP", "TIT1"],
        "MOOD" => &["TMOO"],
        _ => &[],
    }
}

/// Les valeurs de la balise `nom` parmi `paires`, toutes trames comprises,
/// graphies standard comprises ([`alias_standard`]).
fn valeurs_de<'a>(nom: &str, paires: &'a [(String, String)]) -> Vec<&'a str> {
    let alias = alias_standard(nom);
    let a_grp1 = paires.iter().any(|(k, _)| k == "GRP1");
    paires
        .iter()
        .filter(|(k, _)| {
            let n = nom_de_cle(k);
            // Un alias est une trame ou un atome NU (`TIT1`, `©grp`), jamais
            // `TXXX:TIT1`.
            n == nom
                || (!k.contains(':') && alias.contains(&n.as_str()) && !(n == "TIT1" && a_grp1))
        })
        .map(|(_, v)| v.as_str())
        .collect()
}

/// Le réglage entier.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TagsPerso {
    #[serde(default)]
    pub mappings: Vec<Correspondance>,
}

/// Un nom de tag propre : une ligne, sans caractère de contrôle, espaces
/// resserrées, borné à [`MAX_LONGUEUR_TAG`] caractères. `None` s'il ne reste
/// rien.
pub fn nettoyer_nom_de_tag(brut: &str) -> Option<String> {
    let une_ligne: String = brut
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let resserre = une_ligne.split_whitespace().collect::<Vec<_>>().join(" ");
    let borne: String = resserre.chars().take(MAX_LONGUEUR_TAG).collect();
    let borne = borne.trim().to_string();
    (!borne.is_empty()).then_some(borne)
}

fn cle_valide(brut: &str) -> Result<String, String> {
    let cle = brut.trim().to_uppercase();
    if cle.is_empty() {
        return Err("nom de balise vide".into());
    }
    if cle.chars().count() > MAX_LONGUEUR_CLE {
        return Err(format!("« {cle} » dépasse {MAX_LONGUEUR_CLE} caractères"));
    }
    if !cle
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || " _-:.".contains(c))
    {
        return Err(format!(
            "« {cle} » ne doit contenir que des lettres A-Z, des chiffres, espace, _ - : ."
        ));
    }
    if nom_de_cle(&cle).is_empty() {
        return Err(format!("« {cle} » n'a pas de nom de balise"));
    }
    Ok(cle)
}

impl Correspondance {
    fn valider(self) -> Result<Correspondance, String> {
        let key = cle_valide(&self.key)?;
        match self.mode {
            Mode::Fixed => {
                let tag = self
                    .tag
                    .as_deref()
                    .and_then(nettoyer_nom_de_tag)
                    .ok_or_else(|| format!("{key} : un tag fixe doit avoir un nom"))?;
                Ok(Correspondance {
                    key,
                    mode: Mode::Fixed,
                    tag: Some(tag),
                    prefix: None,
                    separator: None,
                })
            }
            Mode::Values => {
                let brut = self.prefix.unwrap_or_default();
                let nu = brut.trim();
                if nu.chars().count() > MAX_LONGUEUR_PREFIXE {
                    return Err(format!(
                        "{key} : le préfixe dépasse {MAX_LONGUEUR_PREFIXE} caractères"
                    ));
                }
                if nu.chars().any(char::is_control) {
                    return Err(format!(
                        "{key} : le préfixe contient un caractère de contrôle"
                    ));
                }
                // L'espace finale compte : « Humeur : » + « Calme ».
                let prefix = (!nu.is_empty()).then(|| brut.trim_start().to_string());
                let separator = match self.separator {
                    None => None,
                    Some(sep) if sep == SEPARATEUR_DEFAUT => None,
                    Some(sep) if sep.chars().count() > MAX_LONGUEUR_SEPARATEUR => {
                        return Err(format!(
                            "{key} : le séparateur dépasse {MAX_LONGUEUR_SEPARATEUR} caractères"
                        ));
                    }
                    Some(sep) if sep.chars().any(char::is_control) => {
                        return Err(format!(
                            "{key} : le séparateur contient un caractère de contrôle"
                        ));
                    }
                    // `""` = aucun séparateur (seuls les champs répétés et NUL).
                    Some(sep) => Some(sep),
                };
                Ok(Correspondance {
                    key,
                    mode: Mode::Values,
                    tag: None,
                    prefix,
                    separator,
                })
            }
        }
    }

    /// Le nom de balise comparé (sans préfixe de conteneur, en capitales).
    pub fn nom(&self) -> String {
        nom_de_cle(&self.key)
    }
}

impl TagsPerso {
    /// Le réglage validé : clés en capitales, doublons exacts écartés.
    pub fn valider(self) -> Result<TagsPerso, String> {
        let mut sortie: Vec<Correspondance> = Vec::new();
        for c in self.mappings {
            let c = c.valider()?;
            let deja = sortie.iter().any(|d| {
                d.nom() == c.nom()
                    && d.mode == c.mode
                    && d.tag == c.tag
                    && d.prefix == c.prefix
                    && d.separator == c.separator
            });
            if !deja {
                sortie.push(c);
            }
        }
        if sortie.len() > MAX_CORRESPONDANCES {
            return Err(format!("au plus {MAX_CORRESPONDANCES} correspondances"));
        }
        Ok(TagsPerso { mappings: sortie })
    }

    /// Aucune correspondance : le scan ne fait rien de plus qu'avant.
    pub fn est_vide(&self) -> bool {
        self.mappings.is_empty()
    }

    /// Les noms de balises à chercher, sans préfixe et sans doublon.
    pub fn cles(&self) -> Vec<String> {
        let mut cles: Vec<String> = Vec::new();
        for c in &self.mappings {
            let n = c.nom();
            if !cles.contains(&n) {
                cles.push(n);
            }
        }
        cles
    }

    /// Les tags tirés de paires `(clé, valeur)` d'un fichier : dans l'ordre
    /// des correspondances puis du fichier, sans doublon (casse ignorée),
    /// au plus [`MAX_TAGS_PAR_PISTE`].
    pub fn tags_des_paires(&self, paires: &[(String, String)]) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        let mut vus: HashSet<String> = HashSet::new();
        let mut pousser = |t: String, tags: &mut Vec<String>| {
            if tags.len() < MAX_TAGS_PAR_PISTE && vus.insert(t.to_lowercase()) {
                tags.push(t);
            }
        };
        for c in &self.mappings {
            let nom = c.nom();
            let valeurs = valeurs_de(&nom, paires);
            match c.mode {
                Mode::Fixed => {
                    let presente = valeurs.iter().any(|v| {
                        !v.trim_matches(|ch: char| ch.is_whitespace() || ch == ';' || ch == '\0')
                            .is_empty()
                    });
                    if presente && let Some(t) = c.tag.as_deref().and_then(nettoyer_nom_de_tag) {
                        pousser(t, &mut tags);
                    }
                }
                Mode::Values => {
                    let sep = c.separator.as_deref().unwrap_or(SEPARATEUR_DEFAUT);
                    for v in valeurs {
                        // Chaque trame compte (champs répétés) ; dans une
                        // trame, le séparateur réglé, et toujours NUL (valeurs
                        // multiples ID3v2.4).
                        let morceaux: Vec<&str> = v
                            .split('\0')
                            .flat_map(|m| {
                                if sep.is_empty() {
                                    vec![m]
                                } else {
                                    m.split(sep).collect()
                                }
                            })
                            .collect();
                        for morceau in morceaux {
                            let Some(valeur) = nettoyer_nom_de_tag(morceau) else {
                                continue;
                            };
                            let brut = match &c.prefix {
                                Some(p) => format!("{p}{valeur}"),
                                None => valeur,
                            };
                            if let Some(t) = nettoyer_nom_de_tag(&brut) {
                                pousser(t, &mut tags);
                            }
                        }
                    }
                }
            }
        }
        tags
    }
}

/// Les noms de tags sous [`CLE_LUE`], tels que le scan les a rangés.
pub fn encoder(tags: &[String]) -> String {
    let mut s = String::new();
    for (i, t) in tags.iter().enumerate() {
        if i > 0 {
            s.push(SEPARATEUR_LU);
        }
        s.push_str(t);
    }
    s
}

/// L'inverse de [`encoder`].
pub fn decoder(brut: &str) -> Vec<String> {
    brut.split(SEPARATEUR_LU)
        .filter_map(nettoyer_nom_de_tag)
        .collect()
}

/// Une clé réservée des métadonnées étendues : jamais rangée en base.
pub fn est_cle_reservee(cle: &str) -> bool {
    cle.starts_with('@')
}

// ---------------------------------------------------------------------------
// Les clés « connues » : celles que Tune lit déjà, ou que les logiciels
// d'étiquetage écrivent d'office. L'aperçu les distingue des clés libres.
// ---------------------------------------------------------------------------

/// Noms de balises (sans préfixe) que Tune lit déjà ou qui sont standard.
const CLES_CONNUES: &[&str] = &[
    "ACOUSTID_FINGERPRINT",
    "ACOUSTID_ID",
    "ACOUSTID FINGERPRINT",
    "ACOUSTID ID",
    "ALBUM",
    "ALBUM ARTIST",
    "ALBUM DYNAMIC RANGE",
    "ALBUMARTIST",
    "ALBUMARTISTS",
    "ALBUMARTISTSORT",
    "ALBUMSORT",
    "ARRANGER",
    "ARTIST",
    "ARTISTS",
    "ARTISTSORT",
    "ASIN",
    "BAND",
    "BARCODE",
    "BPM",
    "CATALOGNUMBER",
    "CHOIR",
    "COMMENT",
    "COMPILATION",
    "COMPOSER",
    "COMPOSERSORT",
    "CONDUCTOR",
    "COPYRIGHT",
    "COVERART",
    "CUESHEET",
    "DATE",
    "DESCRIPTION",
    "DISCNUMBER",
    "DISCSUBTITLE",
    "DISCTOTAL",
    "DJMIXER",
    "DYNAMIC RANGE",
    "EAN",
    "ENCODEDBY",
    "ENCODER",
    "ENCODER_OPTIONS",
    "ENCODERSETTINGS",
    "ENGINEER",
    "ENSEMBLE",
    "EXPLICIT",
    "GENRE",
    "ISRC",
    "ITUNESADVISORY",
    "LABEL",
    "LABELNO",
    "LANGUAGE",
    "LICENSE",
    "LOG",
    "LYRICIST",
    "LYRICS",
    "MEDIA",
    "METADATA_BLOCK_PICTURE",
    "MIXER",
    "MOVEMENT",
    "MOVEMENTNAME",
    "MOVEMENTTOTAL",
    "ORCHESTRA",
    "ORGANIZATION",
    "ORIGINALDATE",
    "ORIGINALRELEASEDATE",
    "ORIGINALYEAR",
    "PERFORMER",
    "PRODUCER",
    "PUBLISHER",
    "RATING",
    "RELEASECOUNTRY",
    "RELEASESTATUS",
    "RELEASETYPE",
    "REMIXER",
    "SCRIPT",
    "SHOWMOVEMENT",
    "SOURCE",
    "SUBTITLE",
    "TITLE",
    "TITLESORT",
    "TOTALDISCS",
    "TOTALTRACKS",
    "TRACKNUMBER",
    "TRACKTOTAL",
    "UNSYNCEDLYRICS",
    "UPC",
    "VENDOR",
    "VERSION",
    "WORK",
    "WRITER",
    "YEAR",
];

/// Préfixes de noms connus (familles entières).
const PREFIXES_CONNUS: &[&str] = &[
    "MUSICBRAINZ",
    "REPLAYGAIN",
    "ITUN",
    "DR_",
    "R128_",
    "DISCOGS_",
];

/// Le nom `nom` (sans préfixe, en capitales) est-il connu de Tune ou
/// standard ?
pub fn est_cle_connue(nom: &str) -> bool {
    CLES_CONNUES.contains(&nom) || PREFIXES_CONNUS.iter().any(|p| nom.starts_with(p))
}

/// Une balise du tiroir (`bloc`, `cle` d'origine) peut-elle servir de
/// correspondance ? Rend son nom comparé, `None` pour une trame ID3 standard
/// (`TIT2`…), un atome MP4 à quatre lettres (`©nam`…), un bloc ID3v1 ou RIFF.
pub fn nom_utilisable(bloc: &str, cle: &str) -> Option<String> {
    let nom = match bloc {
        "id3v2" if cle.starts_with("TXXX:") => Some(nom_de_cle(cle)),
        // Les graphies standard du regroupement et de l'humeur (voir
        // `alias_standard`) : comptées sous le nom à saisir.
        "id3v2" if cle == "GRP1" || cle == "TIT1" => Some("GROUPING".to_string()),
        "id3v2" if cle == "TMOO" => Some("MOOD".to_string()),
        "mp4" if cle == "\u{a9}grp" => Some("GROUPING".to_string()),
        "mp4" if cle.starts_with("----:") => Some(nom_de_cle(cle)),
        "vorbis" | "ape" => Some(nom_de_cle(cle)),
        _ => None,
    };
    nom.filter(|n| !n.is_empty())
}

// ---------------------------------------------------------------------------
// Le miroir et la base.
// ---------------------------------------------------------------------------

static COURANTE: LazyLock<RwLock<Arc<TagsPerso>>> =
    LazyLock::new(|| RwLock::new(Arc::new(TagsPerso::default())));

/// Le réglage en vigueur (miroir en mémoire, sans requête).
pub fn courante() -> Arc<TagsPerso> {
    COURANTE
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

fn installer(r: TagsPerso) {
    match COURANTE.write() {
        Ok(mut g) => *g = Arc::new(r),
        Err(e) => *e.into_inner() = Arc::new(r),
    }
}

/// Le réglage tel qu'en base ; vide s'il n'y est pas, ou s'il est illisible
/// (journalisé).
pub fn lire(backend: &Arc<dyn DbBackend>) -> TagsPerso {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten();
    let Some(brut) = brut else {
        return TagsPerso::default();
    };
    match serde_json::from_str::<TagsPerso>(&brut)
        .map_err(|e| e.to_string())
        .and_then(TagsPerso::valider)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "tags_perso_reglage_illisible");
            TagsPerso::default()
        }
    }
}

/// Au démarrage, AVANT le premier scan : le miroir prend le réglage en base.
pub fn charger(backend: &Arc<dyn DbBackend>) {
    installer(lire(backend));
}

/// Valide, enregistre et installe `saisie`. Rend le réglage d'après.
pub fn ecrire(backend: &Arc<dyn DbBackend>, saisie: TagsPerso) -> Result<TagsPerso, String> {
    let nouveau = saisie.valider()?;
    let settings = SettingsRepo::with_backend(backend.clone());
    if nouveau.est_vide() {
        settings.delete(CLE_REGLAGE)?;
    } else {
        let json = serde_json::to_string(&nouveau).map_err(|e| e.to_string())?;
        settings.set(CLE_REGLAGE, &json)?;
    }
    installer(nouveau.clone());
    Ok(nouveau)
}

#[cfg(test)]
#[path = "tags_perso_tests_9158.rs"]
mod tests;
