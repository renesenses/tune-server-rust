//! Réconciliation du catalogue en ligne : effacer ce que le serveur ne possède
//! plus.
//!
//! `library_sync::push_changes` sait dire `"delete"` — mais rien ne le lui
//! demande jamais pour une entité disparue autrement que par le journal des
//! changements. `full_sync` ne met en file que des `upsert`, et seulement pour
//! ce qui existe ENCORE localement. Un artiste fusionné par un scan, un album
//! nettoyé par le prune post-scan : ils s'évaporent de la base locale sans
//! produire le moindre ordre de suppression. Le cloud les garde à vie.
//!
//! Mesuré le 03/09/2026 après une synchro complète : **+872 artistes et +47
//! albums** que le serveur ne possède plus. Sans ce module, un ami à qui l'on
//! ouvre sa bibliothèque (Tune Circle, T2) verrait des albums fantômes.
//!
//! Le rapprochement se fait sur `remote_id`, l'id LOCAL de Tune, que le cloud
//! stocke avec une contrainte d'unicité par `(server_id, remote_id)`.
//!
//! ## Trois gardes, et pourquoi
//!
//! 1. **Jamais pendant un scan.** Un scan en cours vide et repeuple des tables ;
//!    lire les ids locaux à ce moment ferait passer des entités vivantes pour
//!    des orphelines. Même prudence que `favorites_reconcile`, qui ne supprime
//!    qu'après un scan complet et sain.
//! 2. **Jamais sur une lecture locale vide.** Si `SELECT id FROM artists` rend
//!    zéro ligne, ce n'est pas « tout a disparu », c'est une base non montée.
//! 3. **Jamais au-delà d'une proportion.** Au-dessus de [`PART_MAX_SUPPRIMEE`]
//!    du catalogue en ligne, on refuse et on journalise : c'est le signe d'une
//!    lecture partielle, pas d'une dérive réelle.
//!
//! Le nombre de suppressions est journalisé AVANT d'être émis, jamais après.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{info, warn};

use super::library_sync::record_change;
use crate::db::backend::DbBackend;

const CLOUD_LIBRARY_API: &str = "https://mozaiklabs.fr/api/v1/cloud-library";

/// Le maximum que la pagination Laravel accepte (`min(per_page, 200)`).
const PAR_PAGE: u32 = 200;

/// Au-delà de cette part du catalogue en ligne jugée orpheline, on refuse.
/// Une dérive réelle porte sur quelques pour cent ; la moitié d'un catalogue
/// signale une lecture locale incomplète.
const PART_MAX_SUPPRIMEE: f64 = 0.25;

/// Ce qu'une réconciliation a fait, ou refusé de faire.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RapportReconciliation {
    pub artistes_orphelins: usize,
    pub albums_orphelins: usize,
    pub pistes_orphelines: usize,
    /// Renseigné quand la réconciliation a refusé d'agir, avec la raison.
    pub refus: Option<String>,
    /// Vrai quand rien n'a été mis en file : le rapport dit ce qui SERAIT
    /// supprimé, pas ce qui l'a été.
    pub a_blanc: bool,
}

impl RapportReconciliation {
    pub fn total(&self) -> usize {
        self.artistes_orphelins + self.albums_orphelins + self.pistes_orphelines
    }

    fn refuse(raison: impl Into<String>) -> Self {
        Self {
            refus: Some(raison.into()),
            a_blanc: true,
            ..Default::default()
        }
    }
}

/// Les entités que l'on sait réconcilier, avec leur table locale et le nom que
/// le journal des changements leur donne.
#[derive(Clone, Copy, Debug)]
pub enum Genre {
    Artiste,
    Album,
    Piste,
}

impl Genre {
    fn chemin(self) -> &'static str {
        match self {
            Genre::Artiste => "artists",
            Genre::Album => "albums",
            Genre::Piste => "tracks",
        }
    }

    fn table_locale(self) -> &'static str {
        // Même mot que le chemin distant aujourd'hui, mais les deux n'ont
        // aucune raison de rester liés — le cloud peut renommer sa route.
        match self {
            Genre::Artiste => "artists",
            Genre::Album => "albums",
            Genre::Piste => "tracks",
        }
    }

    /// Le nom que `sync_changelog` attend, au singulier.
    fn entite(self) -> &'static str {
        match self {
            Genre::Artiste => "artist",
            Genre::Album => "album",
            Genre::Piste => "track",
        }
    }
}

/// Décider quels ids en ligne n'ont plus de correspondant local.
///
/// Séparé de tout appel réseau et de toute base : c'est la seule partie qui
/// contient une décision, donc la seule qu'il vaille la peine de tester.
/// Rend `Err(raison)` quand une garde s'oppose à la suppression.
pub fn orphelins(en_ligne: &[i64], locaux: &HashSet<i64>) -> Result<Vec<i64>, String> {
    if en_ligne.is_empty() {
        return Ok(Vec::new());
    }
    if locaux.is_empty() {
        return Err(
            "lecture locale vide — une base non montee n'est pas un catalogue efface".to_string(),
        );
    }

    let orphelins: Vec<i64> = en_ligne
        .iter()
        .copied()
        .filter(|id| !locaux.contains(id))
        .collect();

    let part = orphelins.len() as f64 / en_ligne.len() as f64;
    if part > PART_MAX_SUPPRIMEE {
        return Err(format!(
            "{} orphelins sur {} en ligne ({:.0} %) — au-dela de {:.0} %, \
             c'est une lecture partielle et non une derive",
            orphelins.len(),
            en_ligne.len(),
            part * 100.0,
            PART_MAX_SUPPRIMEE * 100.0
        ));
    }

    Ok(orphelins)
}

/// Lire tous les `remote_id` d'un genre depuis le cloud, page par page.
async fn ids_en_ligne(
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
    genre: Genre,
) -> Result<Vec<i64>, String> {
    let mut ids = Vec::new();
    let mut page = 1u32;

    loop {
        let url = format!(
            "{CLOUD_LIBRARY_API}/{server_id}/{}?per_page={PAR_PAGE}&page={page}",
            genre.chemin()
        );
        let reponse = http_client
            .get(&url)
            .bearer_auth(access_token)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("lecture {} page {page} : {e}", genre.chemin()))?;

        if !reponse.status().is_success() {
            return Err(format!(
                "lecture {} page {page} : HTTP {}",
                genre.chemin(),
                reponse.status()
            ));
        }

        let corps: serde_json::Value = reponse.json().await.map_err(|e| {
            format!(
                "lecture {} page {page} : corps illisible : {e}",
                genre.chemin()
            )
        })?;

        let lignes = corps
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| {
                format!(
                    "lecture {} page {page} : pas de champ `data`",
                    genre.chemin()
                )
            })?;

        for ligne in lignes {
            if let Some(id) = ligne.get("remote_id").and_then(|v| v.as_i64()) {
                ids.push(id);
            }
        }

        let derniere = corps.get("last_page").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        if page >= derniere {
            break;
        }
        page += 1;
    }

    Ok(ids)
}

/// Lire les ids locaux d'un genre.
fn ids_locaux(backend: &Arc<dyn DbBackend>, genre: Genre) -> Result<HashSet<i64>, String> {
    let sql = format!("SELECT id FROM {}", genre.table_locale());
    let lignes = backend
        .query_many(&sql, &[])
        .map_err(|e| format!("lecture locale {} : {e}", genre.table_locale()))?;
    Ok(lignes
        .iter()
        .filter_map(|l| l.first().and_then(|v| v.as_i64()))
        .collect())
}

/// Réconcilier un genre : lire les deux inventaires, décider, mettre en file.
///
/// Ne pousse rien — `library_sync::push_changes` s'en charge au passage
/// suivant. Rend le nombre d'ordres de suppression mis en file.
async fn reconcilier_genre(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
    genre: Genre,
    a_blanc: bool,
) -> Result<usize, String> {
    let en_ligne = ids_en_ligne(http_client, server_id, access_token, genre).await?;
    let locaux = ids_locaux(backend, genre)?;

    let a_supprimer = orphelins(&en_ligne, &locaux)?;

    // Journaliser AVANT d'emettre, jamais apres : une reconciliation qui
    // supprime trop doit etre lisible dans le journal meme si le processus
    // s'arrete au milieu.
    info!(
        genre = genre.chemin(),
        en_ligne = en_ligne.len(),
        locaux = locaux.len(),
        orphelins = a_supprimer.len(),
        a_blanc,
        "cloud_library_reconcile_plan"
    );

    if a_blanc {
        // Le plan est journalise, rien n'est mis en file. C'est le mode par
        // defaut de la route manuelle : regarder avant de supprimer.
        return Ok(a_supprimer.len());
    }

    for id in &a_supprimer {
        record_change(backend, genre.entite(), *id, "delete");
    }

    Ok(a_supprimer.len())
}

/// Réconcilier le catalogue en ligne avec ce que le serveur possède encore.
///
/// `a_blanc` : calculer et journaliser le plan sans rien mettre en file. Le
/// rapport dit alors ce qui SERAIT supprime. C'est le mode par defaut de la
/// route manuelle — on regarde le plan avant de l'executer.
///
/// `avec_pistes` : les pistes sont 235 pages à 200 par page et le point d'accès
/// est limité à 60 requêtes par minute — comptez quatre minutes. L'écart mesuré
/// sur les pistes était de −1, donc le passage courant s'en dispense.
pub async fn reconcilier(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
    avec_pistes: bool,
    a_blanc: bool,
) -> RapportReconciliation {
    if crate::scanner::activite::scan_bibliotheque_en_cours() {
        let raison = "scan de bibliotheque en cours — les ids locaux ne sont pas stables";
        warn!("cloud_library_reconcile_refuse: {raison}");
        return RapportReconciliation::refuse(raison);
    }

    let mut rapport = RapportReconciliation {
        a_blanc,
        ..Default::default()
    };
    let mut genres = vec![Genre::Artiste, Genre::Album];
    if avec_pistes {
        genres.push(Genre::Piste);
    }

    for genre in genres {
        match reconcilier_genre(
            backend,
            http_client,
            server_id,
            access_token,
            genre,
            a_blanc,
        )
        .await
        {
            Ok(n) => match genre {
                Genre::Artiste => rapport.artistes_orphelins = n,
                Genre::Album => rapport.albums_orphelins = n,
                Genre::Piste => rapport.pistes_orphelines = n,
            },
            Err(e) => {
                warn!(genre = genre.chemin(), erreur = %e, "cloud_library_reconcile_genre_refuse");
                rapport.refus = Some(match rapport.refus.take() {
                    Some(deja) => format!("{deja} ; {e}"),
                    None => e,
                });
            }
        }
    }

    info!(
        artistes = rapport.artistes_orphelins,
        albums = rapport.albums_orphelins,
        pistes = rapport.pistes_orphelines,
        a_blanc,
        "cloud_library_reconcile_complete"
    );

    rapport
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locaux(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn les_orphelins_sont_ceux_que_le_local_ne_porte_plus() {
        let en_ligne = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let locaux = locaux(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(orphelins(&en_ligne, &locaux).unwrap(), vec![8]);
    }

    #[test]
    fn un_catalogue_en_ligne_a_jour_ne_produit_aucune_suppression() {
        let en_ligne = vec![1, 2, 3];
        assert!(
            orphelins(&en_ligne, &locaux(&[1, 2, 3]))
                .unwrap()
                .is_empty()
        );
    }

    /// Garde 2. Une base non montee rend zero ligne ; la traiter comme un
    /// catalogue efface viderait le cloud.
    #[test]
    fn une_lecture_locale_vide_est_refusee() {
        let e = orphelins(&[1, 2, 3], &HashSet::new()).unwrap_err();
        assert!(e.contains("lecture locale vide"), "{e}");
    }

    /// Garde 3. Contre-epreuve de la garde de proportion : 3 orphelins sur 8
    /// font 37 %, au-dela des 25 % admis.
    #[test]
    fn une_proportion_excessive_est_refusee() {
        let e = orphelins(&[1, 2, 3, 4, 5, 6, 7, 8], &locaux(&[1, 2, 3, 4, 5])).unwrap_err();
        assert!(e.contains("lecture partielle"), "{e}");
    }

    /// Et juste en dessous du seuil, elle passe : 2 sur 8 font 25 %, admis.
    #[test]
    fn juste_sous_le_seuil_la_suppression_est_admise() {
        let r = orphelins(&[1, 2, 3, 4, 5, 6, 7, 8], &locaux(&[1, 2, 3, 4, 5, 6])).unwrap();
        assert_eq!(r, vec![7, 8]);
    }

    /// Un cloud vide n'est pas une anomalie : rien a supprimer, et surtout pas
    /// de division par zero dans le calcul de proportion.
    #[test]
    fn un_cloud_vide_ne_divise_pas_par_zero() {
        assert!(orphelins(&[], &locaux(&[1, 2])).unwrap().is_empty());
    }
}
