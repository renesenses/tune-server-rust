//! La reprise des MBID d'artiste faux, par vérification MusicBrainz (#6463,
//! décision de Bertrand du 23/10, B).
//!
//! « Christophe » chez FabienM (fil 2302), « Ange » chez Didier (fil 2394) :
//! avant #6614 (rc7), la recherche par le nom prenait le premier résultat de
//! MusicBrainz, et une bibliothèque déjà touchée garde son MBID faux — avec le
//! portrait et la biographie de quelqu'un d'autre. #6614 a corrigé l'avenir ;
//! cette passe reprend le passé.
//!
//! # La règle
//!
//! Sont candidats les artistes qui portent un MBID :
//!
//! - de provenance **inconnue** (NULL : tout MBID antérieur à la migration 133)
//!   ou **`nom`** (trouvé par une recherche du nom) — jamais `balise`, jamais
//!   `pressage`, et **jamais `manuel`** ;
//! - qu'**aucun crédit de pressage ne confirme** : pas de
//!   `track_credits.artist_mbid` égal à ce MBID pour cet artiste (même fiche,
//!   ou même nom crédité). Un tel crédit vaut preuve, sans requête.
//!
//! Pour chacun, UNE requête : la fiche MusicBrainz du MBID
//! (`GET /artist/{mbid}?inc=aliases`). Le MBID est **retiré seulement si ni
//! son nom ni aucun de ses alias** ne sont le nom de l'artiste local, à la
//! casse et aux espaces de bord près — la règle de `mb_choisir_artiste`
//! (#6614), [`super::noms_d_artiste_identiques`]. Un MBID que MusicBrainz ne
//! connaît pas (`404`) est **gardé** et compté : on ne retire que ce qu'on a
//! lu. Le retrait est gardé dans la requête elle-même
//! ([`ArtistRepo::retirer_le_mbid_verifie`]) : le MBID doit être toujours le
//! même, et sa provenance toujours inconnue ou `nom`.
//!
//! Le retrait emporte le portrait et la bio rapportés par ce MBID
//! ([`ArtistRepo::oublier_ce_que_le_mbid_a_apporte`]), et il est **journalisé**
//! : une ligne `mbid_artiste_retire_par_verification` au journal du serveur,
//! et une entrée dans [`CLE_JOURNAL`] (les [`JOURNAL_MAX`] derniers).
//!
//! ⚠ Un homonyme EXACT (deux artistes MusicBrainz qui s'appellent tous deux
//! « Ange ») passe la règle : la fiche lue porte bien le nom de l'artiste
//! local. Rien ici ne peut les départager ; c'est le geste « Détacher
//! l'identité » qui le fait.
//!
//! # Les gardes (les mêmes que les passes MusicBrainz existantes)
//!
//! - **Débit** : chaque requête attend son créneau du limiteur MusicBrainz
//!   PARTAGÉ ([`musicbrainz_release::rate_limit_delay`], 1 requête/s au plus,
//!   toutes passes confondues), avec le User-Agent de Tune ;
//! - **Pause** : la frontière de [`Tache::Enrichissement`] entre deux artistes
//!   ([`crate::taches_de_fond::attendre_son_tour`]) — pause de l'utilisateur,
//!   et place laissée à la lecture ;
//! - **Droit** : le pilote (`tune-server`, `background.rs`) ne lance la passe
//!   qu'avec le droit d'enrichissement automatique (`Feature::AutoEnrichment`) ;
//! - **Disjoncteur** : [`REFUS_D_AFFILEE_MAX`] refus de MusicBrainz d'affilée
//!   arrêtent le tour, curseur gardé avant la série refusée ;
//! - **Reprise après arrêt, et pas de requête refaite** : l'état
//!   ([`CLE_ETAT`]) garde la dernière fiche traitée, écrit après CHAQUE
//!   artiste. Un redémarrage repart après elle ; un tour qui atteint la fin
//!   de la liste marque la passe finie (`fin: true`), et elle ne repart plus.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::metadata::musicbrainz_release::{self, RefusMusicBrainz};
use crate::taches_de_fond::Tache;

/// L'état de la passe, dans `settings`.
pub const CLE_ETAT: &str = "verification_mbid_artistes";
/// Le journal des retraits, dans `settings` (tableau JSON, le plus récent en
/// dernier).
pub const CLE_JOURNAL: &str = "verification_mbid_artistes_journal";
/// Combien de retraits le journal garde.
pub const JOURNAL_MAX: usize = 500;
/// Combien de fiches un tour lit à la fois.
pub const FICHES_PAR_PAGE: usize = 200;
/// Au-delà de ce nombre de refus de MusicBrainz d'affilée, le tour s'arrête.
pub const REFUS_D_AFFILEE_MAX: usize = 12;

/// La sélection : les fiches à MBID de provenance inconnue ou `nom`, sans
/// crédit de pressage qui le confirme, après le curseur (`?`), par
/// identifiant, au plus `?` fiches.
pub fn sql_candidats() -> &'static str {
    "SELECT ar.id, ar.name, ar.musicbrainz_id FROM artists ar \
     WHERE TRIM(COALESCE(ar.musicbrainz_id, '')) <> '' \
       AND (ar.musicbrainz_id_source IS NULL OR ar.musicbrainz_id_source = 'nom') \
       AND ar.id > ? \
       AND NOT EXISTS (SELECT 1 FROM track_credits tc \
                       WHERE LOWER(TRIM(COALESCE(tc.artist_mbid, ''))) = LOWER(TRIM(ar.musicbrainz_id)) \
                         AND (tc.artist_id = ar.id \
                              OR LOWER(TRIM(tc.artist_name)) = LOWER(TRIM(ar.name)))) \
     ORDER BY ar.id \
     LIMIT ?"
}

/// Une fiche à vérifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidat {
    pub id: i64,
    pub nom: String,
    pub mbid: String,
}

/// Les candidats d'une page, après `apres`.
pub fn candidats(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
    limite: usize,
) -> Result<Vec<Candidat>, String> {
    let limite = limite as i64;
    let lignes = backend.query_many(
        sql_candidats(),
        &[&apres as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| {
            Some(Candidat {
                id: l.first()?.as_i64()?,
                nom: l.get(1)?.as_string()?,
                mbid: l.get(2)?.as_string()?.trim().to_string(),
            })
        })
        .collect())
}

/// La fiche MusicBrainz `fiche` nomme-t-elle l'artiste local `nom` ? Son nom,
/// ou l'un de ses alias, à la casse et aux espaces de bord près — la règle de
/// `mb_choisir_artiste` (#6614).
pub fn la_fiche_nomme_l_artiste(fiche: &Value, nom: &str) -> bool {
    let par_le_nom = fiche
        .get("name")
        .and_then(|v| v.as_str())
        .is_some_and(|n| super::noms_d_artiste_identiques(n, nom));
    par_le_nom
        || fiche
            .get("aliases")
            .and_then(|v| v.as_array())
            .is_some_and(|aliases| {
                aliases.iter().any(|al| {
                    al.get("name")
                        .and_then(|v| v.as_str())
                        .is_some_and(|n| super::noms_d_artiste_identiques(n, nom))
                })
            })
}

/// Ce que la vérification a décidé pour une fiche.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Le nom ou un alias de la fiche MusicBrainz est celui de l'artiste.
    Confirme,
    /// Ni le nom ni un alias : le MBID est retiré.
    Retire { nom_musicbrainz: String },
    /// Le MBID a été jugé faux, mais la base a refusé le retrait (le MBID a
    /// changé entre-temps, ou sa provenance n'est plus inconnue ni `nom`).
    RefuseParLaBase,
    /// MusicBrainz ne connaît pas ce MBID (`404`) : gardé, compté.
    Inconnu,
    /// MusicBrainz n'a pas répondu : on ne sait rien de cette fiche.
    Panne(RefusMusicBrainz),
}

/// Le bilan, cumulé tour après tour, rangé dans [`CLE_ETAT`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtatVerification {
    /// `running`, `paused`, `stopped`, `done`.
    #[serde(default)]
    pub status: String,
    /// La dernière fiche traitée : un tour repart après elle.
    #[serde(default)]
    pub dernier_artiste_id: i64,
    /// La liste est parcourue jusqu'au bout : la passe ne repart plus.
    #[serde(default)]
    pub fin: bool,
    #[serde(default)]
    pub verifies: usize,
    #[serde(default)]
    pub confirmes: usize,
    #[serde(default)]
    pub retires: usize,
    #[serde(default)]
    pub refuses_par_la_base: usize,
    #[serde(default)]
    pub inconnus: usize,
    #[serde(default)]
    pub pannes: usize,
    #[serde(default)]
    pub requetes: usize,
    #[serde(default)]
    pub raison: Option<String>,
}

/// Lit l'état rangé ; un état absent ou illisible est un départ à zéro.
pub fn lire_l_etat(backend: &Arc<dyn DbBackend>) -> EtatVerification {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn ecrire_l_etat(backend: &Arc<dyn DbBackend>, etat: &EtatVerification) {
    if let Err(e) = SettingsRepo::with_backend(backend.clone())
        .set(CLE_ETAT, &serde_json::to_string(etat).unwrap_or_default())
    {
        warn!(error = %e, "verification_mbid_etat_non_ecrit");
    }
}

/// Une entrée du journal des retraits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retrait {
    pub artist_id: i64,
    pub nom: String,
    pub mbid: String,
    pub nom_musicbrainz: String,
    pub le: String,
}

/// Le journal des retraits, le plus récent en dernier.
pub fn lire_le_journal(backend: &Arc<dyn DbBackend>) -> Vec<Retrait> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_JOURNAL)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn journaliser(backend: &Arc<dyn DbBackend>, retrait: Retrait) {
    info!(
        artist_id = retrait.artist_id,
        artist = %retrait.nom,
        mbid = %retrait.mbid,
        nom_musicbrainz = %retrait.nom_musicbrainz,
        "mbid_artiste_retire_par_verification"
    );
    let mut journal = lire_le_journal(backend);
    journal.push(retrait);
    if journal.len() > JOURNAL_MAX {
        let surplus = journal.len() - JOURNAL_MAX;
        journal.drain(..surplus);
    }
    if let Err(e) = SettingsRepo::with_backend(backend.clone()).set(
        CLE_JOURNAL,
        &serde_json::to_string(&journal).unwrap_or_default(),
    ) {
        warn!(error = %e, "verification_mbid_journal_non_ecrit");
    }
}

/// Vérifie UNE fiche : une requête, une décision, au plus un retrait.
/// `interroger` lit la fiche MusicBrainz d'un MBID : `Ok(None)` pour un MBID
/// inconnu (`404`), `Err` pour une panne.
pub async fn verifier_un_artiste<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    candidat: &Candidat,
    interroger: &mut F,
) -> Result<Verdict, String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
{
    let fiche = match interroger(candidat.mbid.clone()).await {
        Ok(Some(f)) => f,
        Ok(None) => return Ok(Verdict::Inconnu),
        Err(r) => return Ok(Verdict::Panne(r)),
    };
    if la_fiche_nomme_l_artiste(&fiche, &candidat.nom) {
        return Ok(Verdict::Confirme);
    }
    let nom_musicbrainz = fiche
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let repo = ArtistRepo::with_backend(backend.clone());
    if !repo
        .retirer_le_mbid_verifie(candidat.id, &candidat.mbid)
        .map_err(|e| e.to_string())?
    {
        return Ok(Verdict::RefuseParLaBase);
    }
    journaliser(
        backend,
        Retrait {
            artist_id: candidat.id,
            nom: candidat.nom.clone(),
            mbid: candidat.mbid.clone(),
            nom_musicbrainz: nom_musicbrainz.clone(),
            le: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        },
    );
    Ok(Verdict::Retire { nom_musicbrainz })
}

/// La requête réelle : un créneau du limiteur MusicBrainz partagé, puis la
/// fiche de l'artiste avec ses alias, avec le User-Agent de Tune.
pub async fn lire_la_fiche_musicbrainz(mbid: String) -> Result<Option<Value>, RefusMusicBrainz> {
    musicbrainz_release::rate_limit_delay().await;
    musicbrainz_release::lire_sur_musicbrainz(format!("artist/{}", mbid.trim()), "aliases").await
}

/// Un tour : parcourt les candidats après le curseur rangé, page par page,
/// jusqu'à la fin de la liste, une pause, ou le disjoncteur. L'état est écrit
/// après CHAQUE fiche : un arrêt du serveur ne perd rien. Rend l'état final.
///
/// `interroger` est [`lire_la_fiche_musicbrainz`] en service, un faux dans
/// les témoins — qui ne court-circuite ni le limiteur ni la base quand il
/// passe lui-même par [`lire_la_fiche_musicbrainz`] contre une doublure.
pub async fn executer_un_tour<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    mut interroger: F,
) -> EtatVerification
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
{
    let mut etat = lire_l_etat(backend);
    if etat.fin {
        return etat;
    }
    etat.status = "running".into();
    etat.raison = None;
    ecrire_l_etat(backend, &etat);
    let mut refus_d_affilee = 0usize;
    loop {
        let page = match candidats(backend, etat.dernier_artiste_id, FICHES_PAR_PAGE) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "verification_mbid_selection_echouee");
                etat.status = "stopped".into();
                etat.raison = Some("base_indisponible".into());
                ecrire_l_etat(backend, &etat);
                return etat;
            }
        };
        if page.is_empty() {
            etat.status = "done".into();
            etat.fin = true;
            ecrire_l_etat(backend, &etat);
            info!(
                verifies = etat.verifies,
                confirmes = etat.confirmes,
                retires = etat.retires,
                inconnus = etat.inconnus,
                pannes = etat.pannes,
                "verification_mbid_terminee"
            );
            return etat;
        }
        for candidat in page {
            // La frontière : pause de l'utilisateur, place à la lecture.
            crate::taches_de_fond::attendre_son_tour(Tache::Enrichissement).await;
            etat.requetes += 1;
            let verdict = match verifier_un_artiste(backend, &candidat, &mut interroger).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(artist_id = candidat.id, error = %e, "verification_mbid_base_echouee");
                    etat.dernier_artiste_id = candidat.id;
                    ecrire_l_etat(backend, &etat);
                    continue;
                }
            };
            match verdict {
                Verdict::Panne(r) => {
                    etat.pannes += 1;
                    refus_d_affilee += 1;
                    warn!(artist_id = candidat.id, refus = %r, "verification_mbid_panne_musicbrainz");
                    if refus_d_affilee >= REFUS_D_AFFILEE_MAX {
                        // Le curseur reste avant la série refusée : la reprise
                        // la retente.
                        etat.status = "stopped".into();
                        etat.raison = Some("musicbrainz_injoignable".into());
                        ecrire_l_etat(backend, &etat);
                        return etat;
                    }
                    continue;
                }
                Verdict::Confirme => etat.confirmes += 1,
                Verdict::Retire { .. } => etat.retires += 1,
                Verdict::RefuseParLaBase => etat.refuses_par_la_base += 1,
                Verdict::Inconnu => etat.inconnus += 1,
            }
            refus_d_affilee = 0;
            etat.verifies += 1;
            etat.dernier_artiste_id = candidat.id;
            ecrire_l_etat(backend, &etat);
        }
    }
}

#[cfg(test)]
#[path = "verification_mbid_artistes_tests.rs"]
mod tests;
