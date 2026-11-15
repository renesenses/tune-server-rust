//! #8668 — le pays d'ÉDITION d'un album, lu sur sa release MusicBrainz.
//!
//! La carte « Pays de l'édition » ne lisait que la balise RELEASECOUNTRY :
//! au .18, ~50 albums sur 9 490 la portent, alors que 858 albums locaux ont
//! un `musicbrainz_release_id` (balise du fichier ou identification). Le
//! pays de la release n'était lu nulle part : `musicbrainz_release_cache`
//! n'est rempli que par le calcul de complétude (2 lignes au .18).
//!
//! Une passe de fond ([`rattraper_les_editions`]) lit `GET /release/{mbid}`
//! (au créneau du limiteur MusicBrainz partagé, ou dans
//! `musicbrainz_release_cache` quand la release y est déjà) pour chaque
//! album identifié SANS balise RELEASECOUNTRY, et range :
//!
//! - `albums.release_country` : le code ISO 3166-1 alpha-2 ([`pays_de_la_release`]),
//!   `XE`/`XW` (Europe, monde entier) quand c'est tout ce que MusicBrainz
//!   donne, ou `''` quand il ne donne rien — l'album n'est alors plus
//!   interrogé tant que son identifiant de release ne change pas ;
//! - `albums.release_country_mbid` : l'identifiant dont il a été lu.
//!
//! Lecture seule sur les balises : aucun fichier n'est touché, et la balise
//! d'une piste prime toujours sur ce pays (facettes et filtre, voir
//! [`crate::db::facet_filter::SANS_PAYS_DE_BALISE`]). Pas d'identifiant, pas
//! de pays : rien n'est deviné.

use std::sync::Arc;

use serde_json::Value;
use tracing::debug;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::metadata::musicbrainz_release::RefusMusicBrainz;
use crate::metadata::pays_des_artistes::code_de_pays;

/// Le bilan d'un tour de la passe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BilanEditions {
    /// Releases lues.
    pub traitees: usize,
    /// Pays rangés (code ISO ou `XE`/`XW`).
    pub poses: usize,
    /// Releases sans pays : `''` rangé.
    pub sans_pays: usize,
    /// Refus de MusicBrainz (réseau, statut, corps illisible).
    pub pannes: usize,
    /// Le dernier album traité : le curseur du tour suivant.
    pub dernier_id: i64,
    /// Le tour a atteint la fin de la liste.
    pub fin_de_liste: bool,
}

/// `XE` (Europe) ou `XW` (monde entier) : pas un pays de la carte, mais une
/// réponse de MusicBrainz, comptée « hors carte » comme la balise. `XU`
/// (inconnu) n'en est pas une.
fn pseudo_region(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim().to_ascii_uppercase();
    matches!(s.as_str(), "XE" | "XW").then_some(s)
}

/// Le pays d'une release MusicBrainz : son champ `country` s'il désigne un
/// pays, sinon le premier code ISO des zones de ses `release-events`, sinon
/// `XE`/`XW` quand c'est tout ce qu'elle donne.
pub fn pays_de_la_release(release: &Value) -> Option<String> {
    code_de_pays(release.get("country"))
        .or_else(|| {
            release
                .get("release-events")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|e| e.get("area")?.get("iso-3166-1-codes")?.as_array())
                .flatten()
                .find_map(|c| code_de_pays(Some(c)))
        })
        .or_else(|| pseudo_region(release.get("country")))
}

/// Les albums à lire après `apres_id` : `(id, mbid de release)` en
/// minuscules. Un identifiant de release, aucune balise RELEASECOUNTRY, et un
/// pays jamais lu ou lu d'un autre identifiant.
pub fn candidats(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
) -> Result<Vec<(i64, String)>, String> {
    let limite = limite.max(1) as i64;
    let sans_balise = crate::db::facet_filter::SANS_PAYS_DE_BALISE;
    let lignes = backend.query_many(
        &format!(
            "SELECT pe_al.id, pe_al.musicbrainz_release_id FROM albums pe_al \
             WHERE pe_al.id > ? AND pe_al.musicbrainz_release_id IS NOT NULL \
               AND TRIM(pe_al.musicbrainz_release_id) <> '' \
               AND (pe_al.release_country_mbid IS NULL \
                    OR pe_al.release_country_mbid <> LOWER(TRIM(pe_al.musicbrainz_release_id))) \
               AND {sans_balise} \
             ORDER BY pe_al.id LIMIT ?"
        ),
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

/// Range le pays `pays` (`''` : aucun) lu sur la release `mbid` de l'album
/// `id` — seulement si l'album porte TOUJOURS cet identifiant.
pub fn ranger_le_pays(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    mbid: &str,
    pays: &str,
) -> Result<bool, String> {
    let n = backend.execute(
        "UPDATE albums SET release_country = ?, release_country_mbid = ? \
         WHERE id = ? AND LOWER(TRIM(musicbrainz_release_id)) = ?",
        &[
            &pays as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &id as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
        ],
    )?;
    Ok(n > 0)
}

/// Un tour de la passe : au plus `limite` albums après `apres_id`.
/// `interroger` lit une release MusicBrainz par son identifiant (cache en
/// base puis limiteur partagé en production, une réponse factice au banc).
///
/// Une panne (réseau, 5xx, corps illisible) n'écrit rien et arrête le tour
/// SUR l'album en panne : le tour suivant le reprend. Une release introuvable
/// (400, 404) est rangée sans pays.
pub async fn rattraper_les_editions<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    apres_id: i64,
    limite: usize,
    interroger: F,
) -> Result<BilanEditions, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let albums = candidats(backend, apres_id, limite)?;
    let mut bilan = BilanEditions {
        dernier_id: apres_id,
        fin_de_liste: albums.len() < limite.max(1),
        ..Default::default()
    };
    for (id, mbid) in albums {
        match interroger(mbid.clone()).await {
            Ok(release) => {
                let pays = pays_de_la_release(&release);
                ranger_le_pays(backend, id, &mbid, pays.as_deref().unwrap_or(""))?;
                bilan.traitees += 1;
                bilan.dernier_id = id;
                if pays.is_some() {
                    bilan.poses += 1;
                } else {
                    bilan.sans_pays += 1;
                }
            }
            Err(RefusMusicBrainz::Statut(400 | 404)) => {
                ranger_le_pays(backend, id, &mbid, "")?;
                bilan.traitees += 1;
                bilan.sans_pays += 1;
                bilan.dernier_id = id;
            }
            Err(e) => {
                debug!(album_id = id, erreur = ?e, "pays_des_editions_panne");
                bilan.pannes += 1;
                bilan.fin_de_liste = false;
                break;
            }
        }
    }
    Ok(bilan)
}

/// Le transport de production : la release gardée en base
/// (`musicbrainz_release_cache`, n'importe quels `inc` : `country` et
/// `release-events` sont toujours là) sans requête ; sinon UNE lecture
/// `/release/{mbid}` sans `inc`, au créneau du limiteur MusicBrainz partagé.
pub async fn interroger_la_release(
    backend: Arc<dyn DbBackend>,
    mbid: String,
) -> Result<Value, RefusMusicBrainz> {
    use crate::metadata::musicbrainz_release as mb;
    if let Some(v) =
        crate::metadata::musicbrainz_release_cache::lire(&backend, &mbid, "", chrono::Utc::now())
    {
        return Ok(v);
    }
    mb::rate_limit_delay().await;
    mb::parcourir_sur_musicbrainz(&format!("release/{mbid}"), &[]).await
}

#[cfg(test)]
#[path = "pays_des_editions_tests_8668.rs"]
mod tests;
