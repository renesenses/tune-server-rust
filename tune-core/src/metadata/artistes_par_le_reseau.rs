//! Le MBID des artistes que le pressage n'a pas donné, par une recherche
//! MusicBrainz (#4805, étape C).
//!
//! Idée : MetaRust (la règle de confiance « nettement devant » de
//! `pick_confident_search_hit`), réécrite ici pour les artistes. Les seuils sont
//! ceux de #5866 ([`score_nettement_devant`]).
//!
//! L'étape B ([`super::artistes_du_pressage`]) pose le MBID d'un artiste à
//! partir des crédits du pressage identifié, sans requête. Il lui échappe deux
//! cas : un nom local écrit autrement que tous les noms crédités (alias,
//! translittération, `Fela Anikulapo Kuti`, `坂本龍一` / `Ryuichi Sakamoto`), et
//! un artiste dont aucun album n'est identifié. Cette passe-ci les prend, un
//! artiste à la fois, par le réseau.
//!
//! # La règle
//!
//! 1. **Recherche** `artist` sur MusicBrainz : la phrase du nom local, sur le
//!    nom, l'alias et le nom de tri (`artist:"…" OR alias:"…" OR
//!    sortname:"…"`). UNE requête.
//! 2. **Ne restent que les candidats qui NOMMENT l'artiste local** : même clé
//!    [`cle_artiste`] (accents, casse, « The », ponctuation) que le nom, le nom
//!    de tri (aussi lu « Prénom Nom »), ou l'un des alias. Un pseudo-artiste
//!    MusicBrainz (`Various Artists`…) n'est jamais candidat.
//! 3. **Sans ambiguïté** : le premier est seul, ou nettement devant le second
//!    ([`score_nettement_devant`]). Sinon, comme dans #5866, un départage :
//!    les candidats du peloton (à moins de dix points du premier, cinq au
//!    plus) sont confrontés à la bibliothèque, et un seul doit passer.
//! 4. **Confirmé par la bibliothèque** : un album local de l'artiste porte le
//!    titre d'un groupe de sortie de ce MBID (`release-group`, `arid:`), ou à
//!    défaut une piste locale celui d'un enregistrement (`recording`, `arid:`).
//!    Une ou deux requêtes par candidat confronté. Le score seul ne suffit
//!    jamais.
//! 5. Sinon **rien n'est écrit**, et le cas est compté ([`BilanReseau`]).
//!
//! L'écriture passe par [`super::artistes_du_pressage::poser_les_mbid`] : la
//! colonne doit être vide (aucun écrasement, même d'une valeur posée entre la
//! sélection et l'écriture) et aucune autre fiche ne doit déjà porter ce MBID.
//!
//! Tout se fait en base : seule la colonne `artists.musicbrainz_id` bouge, et
//! seulement là où elle était vide. Aucune écriture dans les fichiers audio.
//!
//! # Débit
//!
//! Chaque requête attend son créneau du limiteur MusicBrainz partagé
//! ([`musicbrainz_release::rate_limit_delay`], 1 requête/s), avec le
//! User-Agent de Tune. La boucle, la pause, la reprise et le disjoncteur sont
//! dans le pilote de lot (`POST /library/identify-all?mode=artistes`).

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;
use tracing::debug;

use crate::db::artist_repo::cle_artiste;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::fold_diacritics;
use crate::metadata::artistes_du_pressage::{MBID_PSEUDO_ARTISTES, poser_les_mbid};
use crate::metadata::musicbrainz_release::{
    self, MB_UA, RefusMusicBrainz, artiste_de_requete, est_un_artiste_fictif, titre_de_requete,
    titre_de_requete_pour,
};

/// Le score à partir duquel un candidat est sûr (#5866).
const SCORE_SUR: i32 = 90;
/// L'avance qui sépare un candidat sûr du suivant (#5866).
const AVANCE_MIN: i32 = 10;
/// Le score très sûr, qui l'emporte sur un second sous [`SCORE_SUR`] (#5866).
const SCORE_TRES_SUR: i32 = 95;

/// Combien de candidats la recherche d'artiste demande.
pub const CANDIDATS_PAR_RECHERCHE: usize = 10;
/// Combien de résultats une requête de confirmation demande.
pub const RESULTATS_PAR_CONFIRMATION: usize = 25;
/// Au-delà de ce nombre de candidats à égalité, l'artiste est ambigu sans
/// même être confronté : cinq homonymes à départager coûtent déjà jusqu'à dix
/// requêtes.
pub const PELOTON_MAX: usize = 5;
/// Combien de titres d'album la requête de confirmation porte au plus.
pub const TITRES_D_ALBUM_MAX: usize = 5;
/// Combien de titres de piste la requête de confirmation porte au plus.
pub const TITRES_DE_PISTE_MAX: usize = 8;

const MB_API: &str = "https://musicbrainz.org/ws/2";

/// La règle de confiance de #5866 sur deux scores : le premier est-il
/// **nettement devant** le second ? Mêmes seuils : ≥ 90 avec 10 points
/// d'avance, ou ≥ 95 face à un second sous 90.
pub fn score_nettement_devant(premier: i32, second: i32) -> bool {
    (premier >= SCORE_SUR && premier - second >= AVANCE_MIN)
        || (premier >= SCORE_TRES_SUR && second < SCORE_SUR)
}

/// Ce que la passe demande à MusicBrainz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entite {
    /// `GET /artist?query=` : la recherche du nom.
    Artiste,
    /// `GET /release-group?query=arid:…` : la confirmation par un album.
    GroupeDeSortie,
    /// `GET /recording?query=arid:…` : la confirmation par une piste.
    Enregistrement,
}

impl Entite {
    /// Le chemin de la ressource, qui est aussi le préfixe de la clé des
    /// réponses enregistrées du banc.
    pub fn chemin(self) -> &'static str {
        match self {
            Entite::Artiste => "artist",
            Entite::GroupeDeSortie => "release-group",
            Entite::Enregistrement => "recording",
        }
    }

    fn cle_des_resultats(self) -> &'static str {
        match self {
            Entite::Artiste => "artists",
            Entite::GroupeDeSortie => "release-groups",
            Entite::Enregistrement => "recordings",
        }
    }

    /// Le nombre de résultats demandés.
    pub fn limite(self) -> usize {
        match self {
            Entite::Artiste => CANDIDATS_PAR_RECHERCHE,
            _ => RESULTATS_PAR_CONFIRMATION,
        }
    }
}

/// Un candidat de la recherche d'artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatArtiste {
    pub mbid: String,
    pub nom: String,
    pub score: i32,
    /// Le nom, le nom de tri, et chaque alias (nom et nom de tri) : tout ce
    /// sous quoi MusicBrainz connaît cet artiste.
    pub noms: Vec<String>,
}

/// Une phrase Lucene : guillemets, et le guillemet et la barre oblique
/// inverse échappés (comme #5866).
fn phrase(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Le nom tel qu'il part dans la requête : espaces en tête et en queue
/// retirés, espaces multiples réduits.
fn nom_de_requete(nom: &str) -> String {
    nom.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// La requête de la recherche d'artiste : la phrase du nom, sur le nom,
/// l'alias et le nom de tri.
pub fn requete_artiste(nom: &str) -> String {
    let p = phrase(&nom_de_requete(nom));
    format!("artist:{p} OR alias:{p} OR sortname:{p}")
}

/// La requête de confirmation : les titres locaux, sous ce MBID d'artiste.
pub fn requete_de_confirmation(entite: Entite, mbid: &str, titres: &[String]) -> String {
    let champ = match entite {
        Entite::GroupeDeSortie => "releasegroup",
        _ => "recording",
    };
    let ou = titres
        .iter()
        .map(|t| format!("{champ}:{}", phrase(t)))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("arid:{} AND ({ou})", mbid.trim())
}

/// Les candidats d'une réponse `/artist?query=`, dans l'ordre de MusicBrainz.
pub fn candidats_d_artiste(data: &Value) -> Vec<CandidatArtiste> {
    let Some(liste) = data.get("artists").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let texte = |v: &Value, k: &str| -> Option<String> {
        v.get(k)
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    liste
        .iter()
        .filter_map(|a| {
            let mbid = texte(a, "id")?.to_lowercase();
            let nom = texte(a, "name").unwrap_or_default();
            let mut noms: Vec<String> = [texte(a, "name"), texte(a, "sort-name")]
                .into_iter()
                .flatten()
                .collect();
            if let Some(alias) = a.get("aliases").and_then(|x| x.as_array()) {
                for al in alias {
                    noms.extend(
                        [texte(al, "name"), texte(al, "sort-name")]
                            .into_iter()
                            .flatten(),
                    );
                }
            }
            let score = a.get("score").and_then(|s| s.as_i64()).unwrap_or(0) as i32;
            Some(CandidatArtiste {
                mbid,
                nom,
                score,
                noms,
            })
        })
        .collect()
}

/// `Sakamoto, Ryuichi` → `Ryuichi Sakamoto` : le nom de tri lu à l'endroit.
fn a_l_endroit(nom: &str) -> Option<String> {
    let (nom_de_famille, prenom) = nom.split_once(", ")?;
    if nom_de_famille.trim().is_empty() || prenom.trim().is_empty() || prenom.contains(',') {
        return None;
    }
    Some(format!("{} {}", prenom.trim(), nom_de_famille.trim()))
}

/// Le candidat nomme-t-il l'artiste local dont la clé est `cle` ?
pub fn nomme(candidat: &CandidatArtiste, cle: &str) -> bool {
    candidat
        .noms
        .iter()
        .any(|n| cle_artiste(n) == cle || a_l_endroit(n).is_some_and(|e| cle_artiste(&e) == cle))
}

/// La clé d'un titre, pour comparer un titre local à un titre MusicBrainz :
/// accents pliés, minuscules, lettres et chiffres seulement.
pub fn cle_de_titre(titre: &str) -> String {
    fold_diacritics(titre)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Ce que la bibliothèque sait d'un artiste : de quoi le confirmer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matiere {
    pub nom: String,
    /// Les titres de ses albums locaux (`albums.artist_id`).
    pub titres_d_album: Vec<String>,
    /// Les titres de ses pistes locales (`tracks.artist_id`).
    pub titres_de_piste: Vec<String>,
}

impl Matiere {
    /// Les titres d'album à demander : tels quels, et tels que la recherche
    /// de pressage les nettoie (suffixes d'édition, préfixe d'artiste ou de
    /// compositeur). Dédoublonnés par [`cle_de_titre`].
    fn albums_a_demander(&self) -> Vec<String> {
        let mut vus = BTreeSet::new();
        let mut out = Vec::new();
        for t in &self.titres_d_album {
            let formes = [
                Some(t.trim().to_string()),
                titre_de_requete(t),
                titre_de_requete_pour(t, &self.nom),
            ];
            for f in formes.into_iter().flatten() {
                let cle = cle_de_titre(&f);
                if !cle.is_empty() && vus.insert(cle) {
                    out.push(f);
                }
            }
        }
        out.truncate(TITRES_D_ALBUM_MAX);
        out
    }

    fn pistes_a_demander(&self) -> Vec<String> {
        let mut vus = BTreeSet::new();
        let mut out = Vec::new();
        for t in &self.titres_de_piste {
            let cle = cle_de_titre(t);
            if !cle.is_empty() && vus.insert(cle) {
                out.push(t.trim().to_string());
            }
        }
        out.truncate(TITRES_DE_PISTE_MAX);
        out
    }
}

/// Une réponse de confirmation porte-t-elle, SOUS CE MBID, un titre local ?
/// L'artiste est relu dans les crédits du résultat : la requête filtre déjà
/// par `arid:`, la réponse le prouve.
pub fn confirme(data: &Value, entite: Entite, mbid: &str, titres: &[String]) -> bool {
    let cles: BTreeSet<String> = titres.iter().map(|t| cle_de_titre(t)).collect();
    let Some(liste) = data
        .get(entite.cle_des_resultats())
        .and_then(|r| r.as_array())
    else {
        return false;
    };
    liste.iter().any(|r| {
        let titre = r.get("title").and_then(|t| t.as_str()).unwrap_or_default();
        let credite = r
            .get("artist-credit")
            .and_then(|c| c.as_array())
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.pointer("/artist/id")
                        .and_then(|i| i.as_str())
                        .is_some_and(|i| i.trim().eq_ignore_ascii_case(mbid))
                })
            });
        credite && cles.contains(&cle_de_titre(titre))
    })
}

/// Le verdict sur un artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Un MBID sûr et confirmé : à poser.
    Pose { mbid: String, departage: bool },
    /// Artiste fictif ou alias de compilation : aucune requête.
    Ecarte,
    /// Aucun candidat ne nomme l'artiste local.
    SansCorrespondance,
    /// Plusieurs candidats se valent, et la bibliothèque n'en désigne pas un
    /// seul.
    Ambigu,
    /// Un candidat sûr, mais rien dans la bibliothèque ne le confirme.
    NonConfirme,
    /// MusicBrainz n'a pas répondu : on ne sait rien de cet artiste.
    Refus(RefusMusicBrainz),
}

/// Ce qu'a coûté et donné l'examen d'un artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Examen {
    pub verdict: Verdict,
    pub requetes: usize,
}

/// L'artiste local est-il à écarter d'office (artiste fictif, alias de
/// compilation, nom vide) ?
pub fn a_ecarter(nom: &str) -> bool {
    est_un_artiste_fictif(nom)
        || cle_artiste(nom).is_empty()
        || artiste_de_requete(Some(nom), None) == "Various Artists"
}

/// Examine UN artiste : recherche, filtre, règle, confirmation. Ne touche pas
/// à la base. `interroger` porte chaque requête (le réseau en service, des
/// réponses enregistrées au banc).
pub async fn examiner<F, Fut>(matiere: &Matiere, mut interroger: F) -> Examen
where
    F: FnMut(Entite, String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let mut requetes = 0;
    let fin = |verdict, requetes| Examen { verdict, requetes };
    if a_ecarter(&matiere.nom) {
        return fin(Verdict::Ecarte, 0);
    }
    let cle = cle_artiste(&matiere.nom);

    requetes += 1;
    let data = match interroger(Entite::Artiste, requete_artiste(&matiere.nom)).await {
        Ok(d) => d,
        Err(r) => return fin(Verdict::Refus(r), requetes),
    };
    let mut candidats: Vec<CandidatArtiste> = candidats_d_artiste(&data)
        .into_iter()
        .filter(|c| !MBID_PSEUDO_ARTISTES.contains(&c.mbid.as_str()) && nomme(c, &cle))
        .collect();
    // Un même MBID rendu deux fois ne fait pas deux candidats.
    let mut vus = BTreeSet::new();
    candidats.retain(|c| vus.insert(c.mbid.clone()));
    candidats.sort_by(|a, b| b.score.cmp(&a.score));
    if candidats.is_empty() {
        return fin(Verdict::SansCorrespondance, requetes);
    }

    let seul_devant =
        candidats.len() == 1 || score_nettement_devant(candidats[0].score, candidats[1].score);
    let a_confronter: Vec<&CandidatArtiste> = if seul_devant {
        vec![&candidats[0]]
    } else {
        let tete = candidats[0].score;
        let peloton: Vec<&CandidatArtiste> = candidats
            .iter()
            .filter(|c| c.score > tete - AVANCE_MIN)
            .collect();
        if peloton.len() > PELOTON_MAX {
            return fin(Verdict::Ambigu, requetes);
        }
        peloton
    };

    let albums = matiere.albums_a_demander();
    let pistes = matiere.pistes_a_demander();
    let mut confirmes = Vec::new();
    for c in &a_confronter {
        let mut ok = false;
        for (entite, titres) in [
            (Entite::GroupeDeSortie, &albums),
            (Entite::Enregistrement, &pistes),
        ] {
            if ok || titres.is_empty() {
                continue;
            }
            requetes += 1;
            match interroger(entite, requete_de_confirmation(entite, &c.mbid, titres)).await {
                Ok(d) => ok = confirme(&d, entite, &c.mbid, titres),
                Err(r) => return fin(Verdict::Refus(r), requetes),
            }
        }
        if ok {
            confirmes.push(c.mbid.clone());
        }
    }

    let verdict = match (confirmes.len(), seul_devant) {
        (1, _) => Verdict::Pose {
            mbid: confirmes.remove(0),
            departage: !seul_devant,
        },
        (0, true) => Verdict::NonConfirme,
        _ => Verdict::Ambigu,
    };
    fin(verdict, requetes)
}

/// La requête réelle : un créneau du limiteur MusicBrainz partagé, puis la
/// recherche, avec le User-Agent de Tune.
pub async fn interroger_musicbrainz(
    entite: Entite,
    requete: String,
) -> Result<Value, RefusMusicBrainz> {
    musicbrainz_release::rate_limit_delay().await;
    let resp = crate::http::client::shared()
        .get(format!("{MB_API}/{}", entite.chemin()))
        .query(&[
            ("query", requete.as_str()),
            ("limit", &entite.limite().to_string()),
            ("fmt", "json"),
        ])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            debug!(error = %e, "artistes_reseau_transport");
            RefusMusicBrainz::Transport
        })?;
    if !resp.status().is_success() {
        return Err(RefusMusicBrainz::Statut(resp.status().as_u16()));
    }
    resp.json()
        .await
        .map_err(|_| RefusMusicBrainz::CorpsIllisible)
}

/// La sélection : les fiches SANS MBID, porteuses d'au moins un album ou une
/// piste locale, après le curseur (`?`, 0 pour un tour neuf), par
/// identifiant, au plus `?` fiches.
pub fn sql_candidats() -> &'static str {
    "SELECT ar.id, ar.name FROM artists ar \
     WHERE TRIM(COALESCE(ar.musicbrainz_id, '')) = '' \
       AND ar.id > ? \
       AND (EXISTS (SELECT 1 FROM albums al WHERE al.artist_id = ar.id \
                      AND COALESCE(al.source, 'local') = 'local') \
         OR EXISTS (SELECT 1 FROM tracks t WHERE t.artist_id = ar.id \
                      AND COALESCE(t.source, 'local') = 'local')) \
     ORDER BY ar.id \
     LIMIT ?"
}

/// Les fiches d'un tour : `(id, nom)`.
pub fn candidats(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
    limite: usize,
) -> Result<Vec<(i64, String)>, String> {
    let limite = limite as i64;
    let lignes = backend.query_many(
        sql_candidats(),
        &[&apres as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| Some((l.first()?.as_i64()?, l.get(1)?.as_string()?)))
        .collect())
}

/// Lit ce que la bibliothèque sait d'un artiste.
pub fn matiere(backend: &Arc<dyn DbBackend>, id: i64, nom: &str) -> Result<Matiere, String> {
    let titres = |sql: &str| -> Result<Vec<String>, String> {
        Ok(backend
            .query_many(sql, &[&id as &dyn ToSqlValue])?
            .iter()
            .filter_map(|l| l.first().and_then(|v| v.as_string()))
            .filter(|t| !t.trim().is_empty())
            .collect())
    };
    Ok(Matiere {
        nom: nom.to_string(),
        titres_d_album: titres(
            "SELECT title FROM albums WHERE artist_id = ? \
             AND COALESCE(source, 'local') = 'local' ORDER BY id LIMIT 20",
        )?,
        titres_de_piste: titres(
            "SELECT title FROM tracks WHERE artist_id = ? \
             AND COALESCE(source, 'local') = 'local' ORDER BY id LIMIT 40",
        )?,
    })
}

/// Le bilan d'un tour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BilanReseau {
    pub traites: usize,
    /// MBID effectivement écrits en base.
    pub poses: usize,
    /// … dont par départage entre candidats à égalité.
    pub departages: usize,
    pub ecartes: usize,
    pub sans_correspondance: usize,
    pub ambigus: usize,
    pub non_confirmes: usize,
    /// MBID décidés que la base a refusés : fiche remplie entre-temps, ou
    /// MBID déjà porté par une autre fiche (un doublon à fusionner).
    pub refuses_par_la_base: usize,
    /// Refus de MusicBrainz (`503`, coupure, délai).
    pub pannes: usize,
    pub requetes: usize,
}

/// Examine et, s'il y a lieu, écrit UN artiste. Rend le refus de
/// MusicBrainz éventuel, pour le disjoncteur du pilote.
pub async fn traiter_un_artiste<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    id: i64,
    nom: &str,
    interroger: F,
    bilan: &mut BilanReseau,
) -> Result<Option<RefusMusicBrainz>, String>
where
    F: FnMut(Entite, String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let m = if a_ecarter(nom) {
        Matiere {
            nom: nom.to_string(),
            ..Default::default()
        }
    } else {
        matiere(backend, id, nom)?
    };
    let examen = examiner(&m, interroger).await;
    bilan.traites += 1;
    bilan.requetes += examen.requetes;
    match examen.verdict {
        Verdict::Pose { mbid, departage } => {
            if poser_les_mbid(backend, &[(id, mbid)])? == 1 {
                bilan.poses += 1;
                if departage {
                    bilan.departages += 1;
                }
            } else {
                bilan.refuses_par_la_base += 1;
            }
        }
        Verdict::Ecarte => bilan.ecartes += 1,
        Verdict::SansCorrespondance => bilan.sans_correspondance += 1,
        Verdict::Ambigu => bilan.ambigus += 1,
        Verdict::NonConfirme => bilan.non_confirmes += 1,
        Verdict::Refus(r) => {
            bilan.pannes += 1;
            return Ok(Some(r));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "artistes_par_le_reseau_tests.rs"]
mod tests;
