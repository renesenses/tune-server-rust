//! Vignette de podcast mise en cache à l'abonnement (#5214).
//!
//! ## Le constat
//!
//! Un abonnement enregistrait l'`image_url` du flux telle quelle, et la lecture
//! d'un épisode posait cette URL distante comme pochette « en cours ». Le client
//! la redemandait au relais `/library/artwork/proxy`, qui n'admet sans
//! signature que les hôtes de sa liste fermée : une vignette hébergée chez un
//! hébergeur de flux RSS absent de la liste (Simplecast, Acast, Ausha…) restait
//! une note de musique dans Historique et dans Lecture en cours.
//!
//! ## La décision (Bertrand, 28/09)
//!
//! Tune télécharge l'image une fois, à l'abonnement — et de nouveau quand le
//! flux la change —, la range dans le cache de pochettes existant et la sert
//! lui-même, par la route `/library/artwork/{condensat}` que toutes les
//! pochettes locales empruntent déjà. Le relais n'est pas touché.
//!
//! ## L'adresse
//!
//! L'image est rangée sous `artwork_hash("podcast-vignette|{url}")` : une
//! adresse dérivée de l'URL SOURCE, calculable sans base de données. C'est ce
//! qui permet de reconnaître, à la lecture d'un épisode ou dans la liste des
//! abonnements, qu'une URL distante a déjà sa copie locale — et de retélécharger
//! quand l'URL du flux change, puisque l'adresse change avec elle.
//!
//! Ce n'est pas l'adressage par le contenu de
//! [`cache_fetched_image`](crate::library::artwork::cache_fetched_image), et
//! c'est voulu : on ne réécrit JAMAIS une adresse déjà servie (une adresse
//! présente dans le cache n'est pas retéléchargée), donc la réserve de #1444 —
//! une même adresse pour deux versions successives d'une image — ne s'applique
//! pas. Une image changée derrière la même URL garde l'ancienne copie ; les
//! hébergeurs changent l'URL quand ils changent l'image.

use std::path::Path;

use crate::library::artwork::{artwork_hash, find_cached, save_to_cache, sniff_image_ext};
use crate::library::artwork_proxy::{EchecTelechargement, Relais};

/// Taille maximale d'une vignette téléchargée : 10 Mio. Les flux publient des
/// carrés de 1400 à 3000 px, de quelques centaines de Kio à quelques Mio.
pub const TAILLE_MAX: usize = 10 * 1024 * 1024;

/// L'adresse de cache de la vignette dont l'URL source est `url`.
pub fn adresse(url: &str) -> String {
    artwork_hash(&format!("podcast-vignette|{}", url.trim()))
}

/// Vrai pour une URL `http(s)` — la seule forme qu'on sache télécharger.
fn est_distante(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://")
}

/// L'adresse locale de la vignette de `url`, si elle est DÉJÀ en cache.
///
/// `None` pour une URL absente du cache, ou qui n'est pas distante.
pub fn en_cache(cache_dir: &Path, url: &str) -> Option<String> {
    if !est_distante(url) {
        return None;
    }
    let a = adresse(url);
    find_cached(cache_dir, &a).map(|_| a)
}

/// Pourquoi une vignette n'a pas été mise en cache.
#[derive(Debug)]
pub enum EchecVignette {
    /// L'URL n'est pas `http(s)`.
    PasDistante,
    /// Le téléchargement a échoué ou a été refusé.
    Telechargement(EchecTelechargement),
    /// Les octets ne sont pas une image que le cache sait resservir.
    FormatInconnu,
    /// L'écriture dans le cache a échoué.
    Ecriture,
}

impl std::fmt::Display for EchecVignette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EchecVignette::PasDistante => write!(f, "URL non http(s)"),
            EchecVignette::Telechargement(e) => write!(f, "{e}"),
            EchecVignette::FormatInconnu => write!(f, "format d'image inconnu"),
            EchecVignette::Ecriture => write!(f, "écriture dans le cache impossible"),
        }
    }
}

/// Met en cache la vignette de `url` et rend son adresse locale.
///
/// Déjà en cache : rend l'adresse sans rien télécharger. Sinon, télécharge par
/// [`Relais::telecharger_image`] (garde d'adresse, redirections jugées, borne
/// de taille et de type), vérifie dans les OCTETS que c'est une image servable
/// ([`sniff_image_ext`]) et l'écrit par [`save_to_cache`], le mécanisme de
/// toutes les pochettes. En cas d'échec, rien n'est écrit : l'appelant garde
/// l'URL distante, comme avant.
pub async fn mettre_en_cache(
    relais: &Relais,
    cache_dir: &Path,
    url: &str,
    taille_max: usize,
) -> Result<String, EchecVignette> {
    if !est_distante(url) {
        return Err(EchecVignette::PasDistante);
    }
    if let Some(a) = en_cache(cache_dir, url) {
        return Ok(a);
    }
    let image = relais
        .telecharger_image(url.trim(), taille_max)
        .await
        .map_err(EchecVignette::Telechargement)?;
    let ext = sniff_image_ext(&image.octets).ok_or(EchecVignette::FormatInconnu)?;
    let a = adresse(url);
    save_to_cache(&image.octets, cache_dir, &a, ext).ok_or(EchecVignette::Ecriture)?;
    Ok(a)
}

#[cfg(test)]
#[path = "vignette_podcast_tests_5214.rs"]
mod tests;
