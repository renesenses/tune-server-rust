//! #8135 (fil 2703) — une RADIO comme élément d'une playlist.
//!
//! Une playlist de titres locaux sert de réveil ; à sa fin, une radio prend
//! le relais pour la matinée. Décision de Bertrand (08/11) : une radio peut
//! être un élément d'une playlist, à n'importe quelle position.
//!
//! La ligne se range dans `playlist_tracks` sous la forme d'un titre de
//! service (#4889) : `source = "radio"`, `source_id` = l'identifiant de la
//! station dans « Mes radios » (`radio_stations.id`), le nom de la station en
//! titre, son logo en pochette. Aucune colonne ni migration nouvelle : le
//! CHECK de la table admet déjà cette forme, sous SQLite comme sous
//! PostgreSQL.
//!
//! La ligne garde l'IDENTIFIANT de la station, jamais l'adresse de son flux :
//! le flux se choisit à la lecture, comme pour une radio jouée seule
//! (`POST /radios/{id}/play/{zone}` — fiche de l'annuaire, pays de
//! l'auditeur). Une station retirée de « Mes radios » reste visible dans la
//! playlist, marquée indisponible ; la lecture la saute, comme une piste
//! introuvable (#6179), sans rien casser.
//!
//! Une ligne `radio` plus ancienne dont le `source_id` est déjà l'adresse
//! d'un flux (`http(s)://…`, posée par un client par `streaming_tracks`) se
//! joue telle quelle : c'est ce qu'elle faisait avant.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::backend::DbBackend;
use crate::db::playlist_repo::{EntryContent, MissingEntry, PlaylistEntry, ServiceEntry};
use crate::db::radio_repo::{RadioRepo, RadioStation};

/// La `source` d'une ligne radio, dans une playlist comme dans la file.
pub const SOURCE_RADIO: &str = "radio";

/// L'artiste qu'une radio porte dans la file : celui de `jouer_un_flux`.
const ARTISTE_D_UNE_RADIO: &str = "Live Radio";

/// Le choix du flux d'une station enregistrée. Le serveur installe celui de
/// la lecture d'une station seule (fiche de l'annuaire, pays) ; sans lui,
/// l'adresse enregistrée de la station. `None` : la station ne se joue pas
/// d'ici (pays exclu, adresse vide) — la ligne est alors indisponible.
pub type ChoixDuFlux = Arc<dyn Fn(&RadioStation) -> Option<String> + Send + Sync>;

/// Le choix par défaut : l'adresse enregistrée de la station.
pub fn flux_enregistre() -> ChoixDuFlux {
    Arc::new(|radio: &RadioStation| Some(radio.url.trim().to_string()).filter(|u| !u.is_empty()))
}

/// Une adresse de flux jouable telle quelle (`http://` ou `https://`).
fn est_une_adresse(s: &str) -> bool {
    let s = s.trim();
    s.get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || s.get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

/// Vrai pour une ligne radio.
pub fn est_une_radio(s: &ServiceEntry) -> bool {
    s.source == SOURCE_RADIO
}

/// L'identifiant de station d'une ligne radio (`radio_stations.id`).
pub fn station_de_la_ligne(s: &ServiceEntry) -> Option<i64> {
    if !est_une_radio(s) {
        return None;
    }
    s.source_id.trim().parse::<i64>().ok().filter(|id| *id > 0)
}

/// La ligne de playlist d'une station de « Mes radios ». `None` pour une
/// station qui n'est pas en base (sans identifiant).
pub fn ligne_de_radio(radio: &RadioStation) -> Option<ServiceEntry> {
    let id = radio.id.filter(|id| *id > 0)?;
    Some(ServiceEntry {
        source: SOURCE_RADIO.into(),
        source_id: id.to_string(),
        title: radio.name.clone(),
        artist: None,
        album: None,
        album_source_id: None,
        duration_ms: None,
        cover_url: radio.logo_url.clone(),
    })
}

/// Ce qu'une ligne radio JOUE : `Some((station, flux))`, ou `None` si elle est
/// indisponible (station retirée, flux refusé). La station n'est connue que
/// pour une ligne qui la désigne par son identifiant.
fn flux_de_la_ligne(
    db: &Arc<dyn DbBackend>,
    s: &ServiceEntry,
    choix: &ChoixDuFlux,
) -> Option<(Option<RadioStation>, String)> {
    if est_une_adresse(&s.source_id) {
        return Some((None, s.source_id.trim().to_string()));
    }
    let id = station_de_la_ligne(s)?;
    let radio = RadioRepo::with_backend(db.clone()).get(id).ok().flatten()?;
    let flux = choix(&radio)?;
    Some((Some(radio), flux))
}

/// Les lignes d'une playlist prêtes pour la file : chaque ligne radio devient
/// la ligne de son FLUX (`source_id` = adresse, comme `play_radio` la passe
/// à l'orchestrateur), ou une ligne introuvable quand la station ne se joue
/// plus — la lecture la saute et `start_index` reste juste
/// (`sans_les_introuvables`). Les autres lignes ne bougent pas.
pub fn lignes_a_jouer(
    db: &Arc<dyn DbBackend>,
    entries: Vec<PlaylistEntry>,
    choix: &ChoixDuFlux,
) -> Vec<PlaylistEntry> {
    entries
        .into_iter()
        .map(|mut e| {
            let EntryContent::Service(s) = &e.content else {
                return e;
            };
            if !est_une_radio(s) {
                return e;
            }
            e.content = match flux_de_la_ligne(db, s, choix) {
                Some((radio, flux)) => {
                    let nom = radio.as_ref().map_or(s.title.clone(), |r| r.name.clone());
                    EntryContent::Service(ServiceEntry {
                        source: SOURCE_RADIO.into(),
                        source_id: flux,
                        title: nom.clone(),
                        artist: Some(ARTISTE_D_UNE_RADIO.into()),
                        album: Some(nom),
                        album_source_id: None,
                        duration_ms: None,
                        cover_url: radio
                            .and_then(|r| r.logo_url)
                            .or_else(|| s.cover_url.clone()),
                    })
                }
                None => {
                    tracing::info!(
                        entry_id = e.id,
                        station = %s.source_id,
                        "playlist_radio_indisponible_sautee"
                    );
                    EntryContent::Missing(MissingEntry {
                        title: Some(s.title.clone()),
                        ..Default::default()
                    })
                }
            };
            e
        })
        .collect()
}

/// La ligne radio telle que `GET /playlists/{id}/tracks` la montre : la forme
/// d'une ligne de service, plus `radio` (vrai), `radio_id` (la station, ou
/// nul pour une ligne qui porte une adresse) et `unavailable` (vrai quand la
/// station a quitté « Mes radios »). Nom et logo suivent la station quand
/// elle existe encore.
pub fn decrire(db: &Arc<dyn DbBackend>, s: &ServiceEntry) -> Value {
    let mut v = s.to_json();
    let station = station_de_la_ligne(s);
    let radio = station.and_then(|id| RadioRepo::with_backend(db.clone()).get(id).ok().flatten());
    let disponible = radio.is_some() || est_une_adresse(&s.source_id);
    if let Some(o) = v.as_object_mut() {
        if let Some(r) = &radio {
            o.insert("title".into(), json!(r.name));
            if r.logo_url.is_some() {
                o.insert("cover_path".into(), json!(r.logo_url));
            }
        }
        o.insert("radio".into(), json!(true));
        o.insert("radio_id".into(), json!(station));
        o.insert("unavailable".into(), json!(!disponible));
    }
    v
}

/// L'adresse qu'un export écrit pour une ligne radio : l'adresse
/// ENREGISTRÉE de la station (celle que l'import retrouve dans « Mes radios
/// »), ou l'adresse que la ligne porte déjà. `None` pour une autre ligne et
/// pour une station retirée.
pub fn adresse_a_exporter(db: &Arc<dyn DbBackend>, s: &ServiceEntry) -> Option<String> {
    if !est_une_radio(s) {
        return None;
    }
    flux_de_la_ligne(db, s, &flux_enregistre()).map(|(_, flux)| flux)
}

/// La ligne radio d'une adresse de flux lue à l'import (M3U) : la station de
/// « Mes radios » qui porte exactement cette adresse. `None` sinon — la ligne
/// suit alors le chemin d'une ligne ordinaire.
pub fn station_d_une_adresse(db: &Arc<dyn DbBackend>, ligne: &str) -> Option<ServiceEntry> {
    let ligne = ligne.trim();
    if !est_une_adresse(ligne) {
        return None;
    }
    let radio = RadioRepo::with_backend(db.clone())
        .get_by_url(ligne)
        .ok()
        .flatten()?;
    ligne_de_radio(&radio)
}

#[cfg(test)]
#[path = "playlist_radio_8135_tests.rs"]
mod tests;
