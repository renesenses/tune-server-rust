//! Les radios HLS (#6357) : liste `.m3u8`, segments MPEG-TS, AAC/HE-AAC,
//! en Rust pur, sans ffmpeg.
//!
//! * [`liste`] lit les listes maîtresse et média et choisit la variante ;
//! * [`ts`] démultiplexe les segments (MPEG-TS, ou audio empaqueté) vers le
//!   flux élémentaire ADTS ou MP3, et lit les titres ID3 ;
//! * [`source`] relit la liste au rythme du direct, enchaîne les segments
//!   sans trou ni doublon et rend le tout comme un `Read` continu.
//!
//! Le lecteur radio (`orchestrator::radio`) pousse ce `Read` dans symphonia,
//! comme le corps d'une station Icecast, et décode l'AAC par `rusty_aac`
//! (HE-AAC et HE-AACv2 compris). Ce qui reste refusé, en le nommant
//! (`radio_hls_unsupported`) : fMP4/CMAF, plages d'octets, DRM
//! (`SAMPLE-AES`, FairPlay…), AC-3/E-AC-3, LATM.
//!
//! Voir l'étude `docs/etude-hls-6357.md` (PR #6363).

pub(crate) mod liste;
pub(crate) mod source;
pub(crate) mod ts;

#[cfg(test)]
pub(crate) mod fabrique_tests;
#[cfg(test)]
mod tests;

pub(crate) use source::{HlsSource, Reglages};
pub(crate) use ts::Charge;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Le titre en cours de chaque station HLS jouée, par adresse de station.
///
/// Il est posé par la source au moment où le segment qui le porte commence
/// à être LU par le décodeur, non quand il est téléchargé : le chargeur a
/// deux ou trois segments d'avance, et le titre arriverait sinon vingt
/// secondes avant la musique.
static TITRES: LazyLock<Mutex<HashMap<String, liste::TitreListe>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn publier_titre(station: &str, titre: liste::TitreListe) {
    let mut titres = TITRES.lock().unwrap_or_else(|e| e.into_inner());
    if titres.get(station) != Some(&titre) {
        tracing::info!(titre = %titre.titre, artiste = ?titre.artiste, "hls_titre_en_cours");
        titres.insert(station.to_string(), titre);
    }
}

pub(crate) fn oublier_titre(station: &str) {
    TITRES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(station);
}

/// Le titre que la liste (`EXTINF`) ou les segments (ID3) de la station
/// portent en ce moment, si elle est jouée et qu'ils en portent un.
pub(crate) fn titre_en_cours(station: &str) -> Option<liste::TitreListe> {
    TITRES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(station)
        .cloned()
}
