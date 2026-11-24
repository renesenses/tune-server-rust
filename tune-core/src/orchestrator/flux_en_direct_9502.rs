//! #9502 (fils 3049 et 3066) — un flux EN DIRECT poussé au MediaRenderer.
//!
//! Un point de contrôle UPnP (JPlay) pousse à la façade MediaRenderer de Tune
//! l'adresse d'une radio (`http://stream.radioparadise.com/flac`). La façade
//! la jouait comme une piste de serveur multimédia (`source = "upnp"`) : la
//! sortie locale recevait l'adresse brute, la lisait comme un FICHIER
//! compressé, et le flux sans fin restait figé au démarrage. La relance
//! automatique (#5522) le relançait, et le cycle recommençait.
//!
//! La même station lancée depuis Tune passe par le chemin radio (décodage au
//! fil de l'eau vers une session WAV, `radio_decode_to_wav_for_local_output`)
//! et joue. Ce module dit, pour une piste `upnp`, si c'est un direct ; la
//! résolution (`resolve_direct`) la fait alors passer par ce chemin-là, sans
//! changer sa source : la façade reconnaît sa lecture à `source = "upnp"`.
//!
//! Les indices, du plus sûr au plus faible :
//! - le DIDL du point de contrôle classe l'item en `audioBroadcast`
//!   (retenu ici par la façade, [`annoncer_une_diffusion`]) ;
//! - le serveur répond avec des en-têtes `icy-*` (Icecast, SHOUTcast) ;
//! - le serveur ne dit aucune longueur (pas de `Content-Length`, ou
//!   `Transfer-Encoding: chunked`) ET la durée est introuvable, ni dans le
//!   DIDL ni dans les en-têtes du flux.
//!
//! Une piste dont la durée est connue, ou une piste indexée de la
//! bibliothèque, reste sur le chemin fichier sans aucune requête de plus.
//!
//! `PlayRequest` n'a pas gagné de champ, pour la même raison que
//! `radio_replis_8110` : trop de constructions sans `..Default::default()`.
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
/// Adresses retenues au plus : la plus ancienne est oubliée.
const DIFFUSIONS_RETENUES_AU_PLUS: usize = 32;
/// Borne de la lecture des en-têtes : au-delà, on ne conclut rien.
const BUDGET_ENTETES: std::time::Duration = std::time::Duration::from_secs(3);
static DIFFUSIONS: LazyLock<Mutex<VecDeque<String>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));
/// Ce qui a fait reconnaître un direct (écrit au journal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndiceDuDirect {
    /// `upnp:class` en `object.item.audioItem.audioBroadcast`.
    ClasseDidlDiffusion,
    /// Le serveur répond avec des en-têtes `icy-*`.
    EnTetesIcy,
    /// Ni longueur annoncée, ni durée lisible.
    SansLongueurNiDuree,
}
impl IndiceDuDirect {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ClasseDidlDiffusion => "didl_audio_broadcast",
            Self::EnTetesIcy => "entetes_icy",
            Self::SansLongueurNiDuree => "sans_longueur_ni_duree",
        }
    }
}
/// Vrai quand une classe `upnp:class` désigne une diffusion
/// (`object.item.audioItem.audioBroadcast`).
pub fn classe_de_diffusion(classe: &str) -> bool {
    classe.to_ascii_lowercase().contains("audiobroadcast")
}
/// La façade MediaRenderer retient ici l'adresse qu'un point de contrôle a
/// classée `audioBroadcast` dans son DIDL, juste avant de la jouer.
pub fn annoncer_une_diffusion(uri: &str) {
    let uri = uri.trim();
    if uri.is_empty() {
        return;
    }
    let Ok(mut retenues) = DIFFUSIONS.lock() else {
        return;
    };
    retenues.retain(|u| u != uri);
    retenues.push_back(uri.to_string());
    while retenues.len() > DIFFUSIONS_RETENUES_AU_PLUS {
        retenues.pop_front();
    }
}
fn diffusion_annoncee(uri: &str) -> bool {
    DIFFUSIONS
        .lock()
        .is_ok_and(|retenues| retenues.iter().any(|u| u == uri.trim()))
}
/// Ce que disent les en-têtes de la réponse, sans lire le corps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entetes {
    Icy,
    SansLongueur,
    AvecLongueur,
}
async fn lire_les_entetes(url: &str) -> Option<Entetes> {
    let lecture = async {
        let rep = crate::http::client::shared()
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .ok()?;
        if !rep.status().is_success() {
            return None;
        }
        let entetes = rep.headers();
        let icy = entetes.keys().any(|nom| {
            let nom = nom.as_str();
            nom.starts_with("icy-") || nom.starts_with("ice-")
        });
        let chunked = entetes
            .get(reqwest::header::TRANSFER_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        // La réponse est abandonnée ici : seuls les en-têtes ont été lus.
        Some(if icy {
            Entetes::Icy
        } else if chunked || rep.content_length().is_none() {
            Entetes::SansLongueur
        } else {
            Entetes::AvecLongueur
        })
    };
    tokio::time::timeout(BUDGET_ENTETES, lecture)
        .await
        .ok()
        .flatten()
}
/// Le direct reconnu pour une piste `upnp` d'adresse `url`, ou `None` quand
/// c'est un fichier (ou qu'on ne peut pas le dire : le chemin fichier reste
/// celui d'avant).
pub(crate) async fn indice_du_direct(
    req: &super::PlayRequest,
    url: &str,
) -> Option<IndiceDuDirect> {
    let annoncee =
        req.source_id.as_deref().is_some_and(diffusion_annoncee) || diffusion_annoncee(url);
    if annoncee {
        return Some(IndiceDuDirect::ClasseDidlDiffusion);
    }
    // Une piste de la bibliothèque, ou une durée connue : un fichier.
    if req.track_id.is_some() || req.duration_ms.is_some_and(|d| d > 0) {
        return None;
    }
    match lire_les_entetes(url).await? {
        Entetes::Icy => Some(IndiceDuDirect::EnTetesIcy),
        Entetes::AvecLongueur => None,
        Entetes::SansLongueur => {
            let duree = super::duree_du_flux_2062::deduire_la_duree_du_flux(url).await;
            (!duree.is_some_and(|d| d > 0)).then_some(IndiceDuDirect::SansLongueurNiDuree)
        }
    }
}
