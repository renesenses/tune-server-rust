//! Discussion #8273, point 3 — ajouter une station en collant son adresse.
//!
//! `POST /radios/probe {"url": "…"}` sonde ce qu'on a collé — un flux direct,
//! une liste `.pls` / `.m3u` ou un direct HLS `.m3u8` — et rend de quoi
//! PRÉ-REMPLIR le formulaire : l'adresse du flux à enregistrer (celle de la
//! liste déréférencée), le codec et le débit, et ce que le serveur de la
//! station annonce de lui-même dans ses en-têtes ICY (`icy-name`,
//! `icy-genre`, `icy-url`, `icy-logo` quand il en publie un). Rien n'est
//! enregistré : c'est l'utilisateur qui valide, après avoir écouté s'il veut.
//!
//! **Ce qui est refusé** est exactement ce que `POST /radios` refuserait : la
//! même validation de forme ([`valider_url_flux`]) et le même juge du contenu
//! ([`tune_core::orchestrator::non_audio_content_type`]). Tout le reste —
//! serveur muet, délai dépassé, code d'erreur, réponse `ICY 200` que le
//! client HTTP ne sait pas lire — rend `reachable: false` et un formulaire
//! non pré-rempli, jamais une erreur : une station lente n'est pas une
//! station fausse (même règle que [`sonder_le_flux`]).
//!
//! Le réseau passe par le client HTTP partagé de l'état (garde
//! `http_client_seam`) ; seuls les en-têtes d'un flux sont lus, jamais son
//! corps, et le corps d'une liste est borné.
//!
//! [`valider_url_flux`]: super::radios::valider_url_flux
//! [`sonder_le_flux`]: super::radios

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::radios::{
    DELAI_SONDE_FLUX, ProblemeUrlFlux, flux_normalise, refus_url, suggestions_du_catalogue,
    valider_url_flux,
};
use crate::state::AppState;
use tune_core::db::radio_repo::RadioRepo;

/// Une liste `.pls` / `.m3u` ou un manifeste HLS ne pèse que quelques Kio ;
/// au-delà, ce n'en est pas un, et on ne lit pas plus loin.
const PLAFOND_LISTE: usize = 64 * 1024;

#[derive(Deserialize)]
pub(crate) struct DemandeSonde {
    #[serde(alias = "stream_url")]
    url: String,
}

/// Ce que la sonde a appris. Tout est facultatif sauf l'adresse à enregistrer.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub(crate) struct Sonde {
    /// L'adresse à enregistrer : le flux lui-même, ou celui qu'une liste
    /// `.pls` / `.m3u` désignait.
    pub stream_url: String,
    /// L'adresse collée quand c'était une liste déréférencée.
    pub playlist_url: Option<String>,
    /// Le serveur a répondu (en-têtes lus).
    pub reachable: bool,
    pub hls: bool,
    pub content_type: Option<String>,
    pub codec: Option<String>,
    /// kbit/s.
    pub bitrate: Option<i32>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub name: Option<String>,
    pub genre: Option<String>,
    pub description: Option<String>,
    pub homepage_url: Option<String>,
    pub logo_url: Option<String>,
    pub country: Option<String>,
    /// Une station de « Mes radios » a déjà ce flux.
    pub existing_id: Option<i64>,
    pub existing_name: Option<String>,
}

/// Le codec d'un `Content-Type` audio, dans l'écriture de l'annuaire.
pub(crate) fn codec_du_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => "MP3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/mp4" | "audio/x-m4a" => "AAC",
        "audio/flac" | "audio/x-flac" => "FLAC",
        "audio/ogg" | "application/ogg" | "audio/vorbis" => "OGG",
        "audio/opus" => "OPUS",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "WAV",
        _ => return None,
    })
}

/// Le codec d'une entrée `CODECS` de HLS (RFC 6381).
fn codec_hls(codecs: &str) -> Option<&'static str> {
    let codecs = codecs.to_ascii_lowercase();
    codecs.split(',').map(str::trim).find_map(|c| {
        Some(match c {
            "mp4a.40.34" | "mp4a.69" | "mp4a.6b" => "MP3",
            c if c.starts_with("mp4a") => "AAC",
            "flac" => "FLAC",
            "opus" => "OPUS",
            "ac-3" => "AC3",
            "ec-3" => "EAC3",
            _ => return None,
        })
    })
}

/// Liste `.pls` / `.m3u` (pas HLS) d'après le chemin ou le type annoncé.
fn est_une_liste(url: &str, content_type: Option<&str>) -> bool {
    let chemin = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if chemin.ends_with(".pls") || chemin.ends_with(".m3u") {
        return true;
    }
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    ct.starts_with("audio/x-scpls")
        || ct.starts_with("application/pls")
        || ct.starts_with("audio/x-mpegurl")
        || ct.starts_with("audio/mpegurl")
}

fn est_hls(url: &str, content_type: Option<&str>) -> bool {
    let chemin = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    chemin.ends_with(".m3u8")
        || ct.starts_with("application/vnd.apple.mpegurl")
        || ct.starts_with("application/x-mpegurl")
}

/// Valeur d'en-tête en texte. `icy-name` est souvent en Latin-1, que
/// `to_str` refuse : chaque octet y est un point de code.
fn texte_d_entete(h: &reqwest::header::HeaderMap, nom: &str) -> Option<String> {
    let v = h.get(nom)?.as_bytes();
    let s = match std::str::from_utf8(v) {
        Ok(s) => s.to_string(),
        Err(_) => v.iter().map(|&b| b as char).collect(),
    };
    let s = s.trim().to_string();
    (!s.is_empty() && s != "-" && !s.eq_ignore_ascii_case("unspecified")).then_some(s)
}

fn premier_entier(s: &str) -> Option<i32> {
    s.split([',', ';', ' '])
        .next()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|n| *n > 0)
}

/// `ice-audio-info: ice-samplerate=44100;ice-bitrate=128;ice-channels=2`.
fn info_ice(valeur: &str, cle: &str) -> Option<i32> {
    valeur.split(';').find_map(|paire| {
        let (k, v) = paire.split_once('=')?;
        let k = k.trim().to_ascii_lowercase();
        (k == cle || k == format!("ice-{cle}"))
            .then(|| v.trim().parse::<i32>().ok())
            .flatten()
            .filter(|n| *n > 0)
    })
}

fn adresse_http(s: &str) -> Option<String> {
    let s = s.trim();
    (s.starts_with("http://") || s.starts_with("https://")).then(|| s.to_string())
}

/// Remplit la sonde à partir des en-têtes d'un flux.
fn lire_les_entetes(s: &mut Sonde, h: &reqwest::header::HeaderMap) {
    s.content_type = texte_d_entete(h, "content-type");
    s.codec = s
        .content_type
        .as_deref()
        .and_then(codec_du_type)
        .map(str::to_string);
    let ice = texte_d_entete(h, "ice-audio-info");
    s.bitrate = texte_d_entete(h, "icy-br")
        .as_deref()
        .and_then(premier_entier)
        .or_else(|| ice.as_deref().and_then(|i| info_ice(i, "bitrate")));
    s.sample_rate = texte_d_entete(h, "icy-sr")
        .as_deref()
        .and_then(premier_entier)
        .or_else(|| ice.as_deref().and_then(|i| info_ice(i, "samplerate")));
    s.channels = ice.as_deref().and_then(|i| info_ice(i, "channels"));
    s.name = texte_d_entete(h, "icy-name");
    s.genre = texte_d_entete(h, "icy-genre");
    s.description = texte_d_entete(h, "icy-description");
    s.homepage_url = texte_d_entete(h, "icy-url").and_then(|u| adresse_http(&u));
    s.logo_url = texte_d_entete(h, "icy-logo").and_then(|u| adresse_http(&u));
}

/// Lit au plus [`PLAFOND_LISTE`] octets du corps.
async fn corps_borne(mut r: reqwest::Response) -> Option<Vec<u8>> {
    let mut corps = Vec::new();
    while let Some(morceau) = r.chunk().await.ok()? {
        corps.extend_from_slice(&morceau);
        if corps.len() >= PLAFOND_LISTE {
            corps.truncate(PLAFOND_LISTE);
            break;
        }
    }
    Some(corps)
}

/// Le meilleur variant d'un manifeste HLS maître : son débit et son codec.
pub(crate) fn variant_hls(manifeste: &str) -> (Option<i32>, Option<&'static str>) {
    let mut meilleur: Option<(i64, Option<&'static str>)> = None;
    for ligne in manifeste.lines() {
        let Some(attributs) = ligne.trim().strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let mut debit: Option<i64> = None;
        let mut moyen: Option<i64> = None;
        let mut codec = None;
        // Les attributs sont séparés par des virgules, mais `CODECS="a,b"` en
        // contient aussi : on découpe hors des guillemets.
        let mut entre_guillemets = false;
        let mut debut = 0;
        let mut morceaux = Vec::new();
        for (i, c) in attributs.char_indices() {
            match c {
                '"' => entre_guillemets = !entre_guillemets,
                ',' if !entre_guillemets => {
                    morceaux.push(&attributs[debut..i]);
                    debut = i + 1;
                }
                _ => {}
            }
        }
        morceaux.push(&attributs[debut..]);
        for m in morceaux {
            let Some((k, v)) = m.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim() {
                "BANDWIDTH" => debit = v.parse().ok(),
                "AVERAGE-BANDWIDTH" => moyen = v.parse().ok(),
                "CODECS" => codec = codec_hls(v),
                _ => {}
            }
        }
        if let Some(b) = moyen.or(debit)
            && meilleur.as_ref().is_none_or(|(m, _)| b > *m)
        {
            meilleur = Some((b, codec));
        }
    }
    match meilleur {
        Some((b, c)) => (i32::try_from((b + 500) / 1000).ok(), c),
        None => (None, None),
    }
}

async fn get(client: &reqwest::Client, url: &str) -> Option<reqwest::Response> {
    client
        .get(url)
        .header("Icy-MetaData", "1")
        .timeout(DELAI_SONDE_FLUX)
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())
}

/// Sonde une adresse collée. `Err` seulement pour ce que `POST /radios`
/// refuserait aussi.
pub(crate) async fn sonder(
    client: &reqwest::Client,
    saisie: &str,
) -> Result<Sonde, ProblemeUrlFlux> {
    let url = valider_url_flux(saisie)?;
    let mut s = Sonde {
        stream_url: url.clone(),
        ..Default::default()
    };
    let Some(mut reponse) = get(client, &url).await else {
        return Ok(s);
    };
    let mut type_annonce = texte_d_entete(reponse.headers(), "content-type");

    // Liste `.pls` / `.m3u` : on la lit, et l'on sonde le flux qu'elle désigne.
    if est_une_liste(&url, type_annonce.as_deref()) && !est_hls(&url, type_annonce.as_deref()) {
        let Some(corps) = corps_borne(reponse).await else {
            return Ok(s);
        };
        let entrees = tune_core::library::m3u_parser::parse_m3u_content(&corps, true);
        let Some(entree) = entrees
            .into_iter()
            .find(|e| adresse_http(&e.path).is_some())
        else {
            return Ok(s);
        };
        let flux = entree.path.trim().to_string();
        let flux = valider_url_flux(&flux)?;
        s.playlist_url = Some(url.clone());
        s.stream_url = flux.clone();
        let titre = entree.title.filter(|t| !t.trim().is_empty());
        let Some(r) = get(client, &flux).await else {
            s.name = titre;
            return Ok(s);
        };
        reponse = r;
        type_annonce = texte_d_entete(reponse.headers(), "content-type");
        if let Some(t) = titre {
            s.name = Some(t);
        }
    }

    s.reachable = true;
    if let Some(type_mime) = type_annonce
        .as_deref()
        .and_then(tune_core::orchestrator::non_audio_content_type)
    {
        return Err(ProblemeUrlFlux::PasUnFlux { type_mime });
    }
    let nom_de_la_liste = s.name.take();
    lire_les_entetes(&mut s, reponse.headers());
    s.name = s.name.or(nom_de_la_liste);

    if est_hls(&s.stream_url, type_annonce.as_deref()) {
        s.hls = true;
        if let Some(corps) = corps_borne(reponse).await {
            let (debit, codec) = variant_hls(&String::from_utf8_lossy(&corps));
            s.bitrate = s.bitrate.or(debit);
            s.codec = codec.map(str::to_string);
        }
    }
    // Pour un flux, la réponse est abandonnée ici : seuls ses en-têtes ont été
    // lus, la connexion se ferme avec elle.
    Ok(s)
}

/// Complète la sonde avec ce que Tune sait déjà : une station de « Mes
/// radios » au même flux, puis la fiche de l'annuaire au même flux (nom,
/// genre, pays, site, logo) pour ce que le serveur de la station n'a pas dit.
fn completer(state: &AppState, s: &mut Sonde) {
    let cible = flux_normalise(&s.stream_url);
    if let Some(deja) = RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default()
        .into_iter()
        .find(|r| flux_normalise(&r.url) == cible)
    {
        s.existing_id = deja.id;
        s.existing_name = Some(deja.name);
    }
    let Ok(annuaire) = state.annuaire_radios.read() else {
        return;
    };
    if let Some(fiche) = annuaire
        .iter()
        .find(|f| super::radios_annuaire::flux_de_la_fiche(f, &s.stream_url))
    {
        let vide = |o: &Option<String>| o.as_deref().is_none_or(|v| v.trim().is_empty());
        if vide(&s.name) {
            s.name = Some(fiche.name.clone());
        }
        if vide(&s.genre) {
            s.genre = fiche.genre.clone();
        }
        if vide(&s.homepage_url) {
            s.homepage_url = fiche.website_url.clone();
        }
        if vide(&s.logo_url) {
            s.logo_url = fiche.logo_url.clone();
        }
        if vide(&s.codec) {
            s.codec = fiche.codec.clone();
        }
        s.bitrate = s.bitrate.or(fiche.bitrate);
        s.country = fiche.country.clone();
    }
}

/// `POST /radios/probe`
pub(crate) async fn sonder_une_station(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(demande): Json<DemandeSonde>,
) -> Response {
    match sonder(&state.http_client, &demande.url).await {
        Ok(mut s) => {
            completer(&state, &mut s);
            tracing::info!(
                url = %s.stream_url,
                liste = s.playlist_url.is_some(),
                joignable = s.reachable,
                codec = ?s.codec,
                debit = ?s.bitrate,
                "radio_sondee"
            );
            Json(s).into_response()
        }
        Err(probleme) => {
            let repo = RadioRepo::with_backend(state.backend.clone());
            let suggestions = suggestions_du_catalogue(&repo, None, &demande.url);
            refus_url(
                &probleme,
                &crate::i18n::lang_from_header(&headers),
                &suggestions,
            )
        }
    }
}

#[cfg(test)]
#[path = "radios_sonde_8273_tests.rs"]
mod tests;
