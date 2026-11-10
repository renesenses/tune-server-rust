//! #8090 — le pays de l'ARTISTE, lu sur sa fiche MusicBrainz.
//!
//! La carte et le filtre « Pays » de la Bibliothèque (#2645) ne lisaient que
//! le pays de PARUTION de l'édition (balise RELEASECOUNTRY), présent sur une
//! poignée de titres : « France » ne rendait qu'un album au .18. Bertrand
//! attend le pays de l'artiste.
//!
//! Une passe de fond ([`rattraper_les_pays`]) lit la fiche MusicBrainz
//! (`GET /artist/{mbid}`, au créneau du limiteur partagé) de chaque artiste
//! dont le MBID est déjà résolu, et range :
//!
//! - `artists.country` : le code ISO 3166-1 alpha-2 de la fiche
//!   ([`pays_de_la_fiche`]), ou `''` quand MusicBrainz n'en donne aucun —
//!   l'artiste n'est alors plus interrogé tant que son MBID ne change pas ;
//! - `artists.country_mbid` : le MBID dont il a été lu.
//!
//! Un MBID changé ou retiré rend le pays CADUC sans autre écriture : les
//! facettes ne lisent un pays que si `country_mbid` est le MBID courant
//! ([`crate::db::facet_filter::PAYS_ARTISTE_VALIDE`]), et la passe relit la
//! fiche. Rien n'est inventé : pas de MBID, pas de pays.

use std::sync::Arc;

use serde_json::Value;
use tracing::debug;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::metadata::artistes_du_pressage::MBID_PSEUDO_ARTISTES;
use crate::metadata::musicbrainz_release::RefusMusicBrainz;

/// Le bilan d'un tour de la passe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BilanPays {
    /// Fiches lues (ou écartées sans requête : pseudo-artistes).
    pub traitees: usize,
    /// Pays rangés.
    pub poses: usize,
    /// Fiches MusicBrainz sans pays : `''` rangé.
    pub sans_pays: usize,
    /// Refus de MusicBrainz (réseau, statut, corps illisible).
    pub pannes: usize,
    /// La dernière fiche traitée : le curseur du tour suivant.
    pub dernier_id: i64,
    /// Le tour a atteint la fin de la liste.
    pub fin_de_liste: bool,
}

/// Les pseudo-codes de MusicBrainz qui ne sont pas des pays : `XW` (monde
/// entier), `XE` (Europe), `XU` (inconnu).
const PSEUDO_PAYS: &[&str] = &["XW", "XE", "XU"];

fn code_de_pays(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim().to_ascii_uppercase();
    (s.len() == 2
        && s.bytes().all(|b| b.is_ascii_uppercase())
        && !PSEUDO_PAYS.contains(&s.as_str()))
    .then_some(s)
}

/// Le pays d'une fiche MusicBrainz d'artiste : son champ `country`, sinon le
/// premier code ISO de sa zone (`area.iso-3166-1-codes`). Une zone qui n'est
/// pas un pays (une ville) n'a pas de code : rien n'est déduit.
pub fn pays_de_la_fiche(fiche: &Value) -> Option<String> {
    code_de_pays(fiche.get("country")).or_else(|| {
        fiche
            .get("area")
            .and_then(|a| a.get("iso-3166-1-codes"))
            .and_then(Value::as_array)
            .and_then(|codes| codes.iter().find_map(|c| code_de_pays(Some(c))))
    })
}

/// Les fiches à lire après `apres_id` : `(id, mbid)` en minuscules. Un MBID,
/// et un pays jamais lu ou lu d'un autre MBID.
pub fn candidats(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
) -> Result<Vec<(i64, String)>, String> {
    let limite = limite.max(1) as i64;
    let lignes = backend.query_many(
        "SELECT id, musicbrainz_id FROM artists \
         WHERE id > ? AND musicbrainz_id IS NOT NULL AND TRIM(musicbrainz_id) <> '' \
           AND (country_mbid IS NULL OR country_mbid <> LOWER(TRIM(musicbrainz_id))) \
         ORDER BY id LIMIT ?",
        &[&apres_id as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .into_iter()
        .filter_map(|l| {
            let id = l.first()?.as_i64()?;
            let mbid = l.get(1)?.as_string()?.trim().to_lowercase();
            Some((id, mbid))
        })
        .collect())
}

/// Range le pays `pays` (`''` : aucun) lu sur la fiche `mbid` de l'artiste
/// `id` — seulement si l'artiste porte TOUJOURS ce MBID.
pub fn ranger_le_pays(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    mbid: &str,
    pays: &str,
) -> Result<bool, String> {
    let n = backend.execute(
        "UPDATE artists SET country = ?, country_mbid = ? \
         WHERE id = ? AND LOWER(TRIM(musicbrainz_id)) = ?",
        &[
            &pays as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &id as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
        ],
    )?;
    Ok(n > 0)
}

/// Un tour de la passe : au plus `limite` fiches après `apres_id`.
/// `interroger` lit une fiche MusicBrainz par son MBID (le limiteur partagé
/// en production, une réponse factice au banc).
///
/// Une panne n'écrit rien et arrête le tour SUR la fiche en panne : le tour
/// suivant la reprend.
pub async fn rattraper_les_pays<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
    interroger: F,
) -> Result<BilanPays, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let fiches = candidats(backend, apres_id, limite)?;
    let mut bilan = BilanPays {
        dernier_id: apres_id,
        fin_de_liste: fiches.len() < limite.max(1),
        ..Default::default()
    };
    for (id, mbid) in fiches {
        if MBID_PSEUDO_ARTISTES.contains(&mbid.as_str()) {
            // « Various Artists » et consorts : pas de pays, pas de requête.
            ranger_le_pays(backend, id, &mbid, "")?;
            bilan.traitees += 1;
            bilan.sans_pays += 1;
            bilan.dernier_id = id;
            continue;
        }
        match interroger(mbid.clone()).await {
            Ok(fiche) => {
                bilan.traitees += 1;
                bilan.dernier_id = id;
                let pays = pays_de_la_fiche(&fiche);
                ranger_le_pays(backend, id, &mbid, pays.as_deref().unwrap_or(""))?;
                if pays.is_some() {
                    bilan.poses += 1;
                } else {
                    bilan.sans_pays += 1;
                }
            }
            Err(e) => {
                debug!(artiste_id = id, erreur = ?e, "pays_des_artistes_panne");
                bilan.pannes += 1;
                bilan.fin_de_liste = false;
                break;
            }
        }
    }
    Ok(bilan)
}

#[cfg(test)]
#[path = "pays_des_artistes_tests_8090.rs"]
mod tests;
