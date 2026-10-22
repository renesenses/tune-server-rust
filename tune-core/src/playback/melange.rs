//! #6526 — l'aléatoire MÉLANGE réellement la file.
//!
//! Avant, `POST /zones/{id}/shuffle` n'armait qu'un mode : le serveur tirait un
//! ordre caché (`ZoneState::shuffle_order`, jamais sérialisé) et
//! `GET /zones/{id}/queue` continuait de rendre l'ordre d'origine. La file que
//! le web, l'app iOS et l'app Android affichaient n'était donc pas celle qui se
//! jouait (constat des PR tune-remote-ios#75 et tune-remote-flutter#73). Le
//! tirage caché avait deux autres défauts : il était refait à CHAQUE
//! changement de longueur de file, si bien qu'un « Lire ensuite » ne jouait
//! pas le titre ensuite, et qu'un simple ajout rebattait aussi les titres déjà
//! joués.
//!
//! Désormais, comme dans la plupart des lecteurs :
//!
//! - activer l'aléatoire réécrit l'ORDRE des lignes de la file : ce qui précède
//!   le titre en cours et le titre en cours ne bougent pas, seule la suite est
//!   rebattue. La file relue est l'ordre réellement joué, et l'avance se fait
//!   dans l'ordre de la file (l'ordre caché n'est plus jamais matérialisé) ;
//! - l'ordre d'origine est retenu (identifiants de ligne) ; désactiver
//!   l'aléatoire le rétablit, et la lecture continue sur le même titre ;
//! - un ajout pendant l'aléatoire suit la règle de la route d'ajout : « Lire
//!   ensuite » juste après le titre en cours, « Ajouter » en fin de file ;
//! - une file NEUVE lancée pendant l'aléatoire (album, liste…) est mélangée
//!   autour du titre lancé : il joue en premier, tout le reste suit dans le
//!   désordre — le comportement que le tirage caché donnait déjà.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{info, warn};

use super::{PlaybackManager, generate_shuffle_order};
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::PlayQueueRepo;

/// Combien de fois relire la file quand elle a changé entre la lecture et
/// l'écriture du nouvel ordre (un ajout concurrent, par exemple).
const ESSAIS: usize = 3;

/// Mélange `ids[debut..]` sur place, à la Fisher-Yates.
fn melanger_a_partir_de(ids: &mut [i64], debut: usize) {
    if debut >= ids.len() {
        return;
    }
    let suite = &mut ids[debut..];
    // `current` hors bornes : aucune ligne n'est ramenée en tête, la
    // permutation est un tirage pur.
    let ordre = generate_shuffle_order(suite.len(), usize::MAX);
    let copie: Vec<i64> = suite.to_vec();
    for (dest, src) in suite.iter_mut().zip(ordre) {
        *dest = copie[src];
    }
}

/// L'ordre de la file après activation : `ids[..=courant]` inchangé (titres
/// déjà joués et titre en cours), la suite rebattue. Sans titre en cours
/// (`None`), toute la file est rebattue.
pub fn melanger_la_suite(ids: &[i64], courant: Option<usize>) -> Vec<i64> {
    let mut nouvel = ids.to_vec();
    let debut = courant.map_or(0, |c| c.saturating_add(1));
    melanger_a_partir_de(&mut nouvel, debut);
    nouvel
}

/// L'ordre d'une file NEUVE lancée pendant l'aléatoire : le titre de départ en
/// tête, tout le reste (titres qui le précédaient compris) rebattu derrière.
pub fn melanger_une_file_neuve(ids: &[i64], depart: usize) -> Vec<i64> {
    let mut nouvel = ids.to_vec();
    if depart < nouvel.len() {
        let tete = nouvel.remove(depart);
        nouvel.insert(0, tete);
    }
    melanger_a_partir_de(&mut nouvel, 1);
    nouvel
}

/// L'ordre à rétablir quand l'aléatoire s'éteint.
///
/// `origine` est l'ordre retenu à l'activation, `actuel` l'ordre de la file
/// maintenant. Les lignes retirées entre-temps disparaissent ; les lignes
/// AJOUTÉES pendant l'aléatoire gardent leur voisinage :
///
/// - une ligne posée au milieu de la file (« Lire ensuite ») revient juste
///   après la ligne qui la précède maintenant — le titre en cours pour un
///   « Lire ensuite », qui reste donc le suivant ;
/// - les lignes ajoutées en FIN de file (« Ajouter à la file ») restent en fin
///   de file, dans leur ordre d'ajout.
///
/// Une `origine` qui ne recoupe pas la file (file remplacée par un chemin qui
/// ne la retient pas, instantané réinjecté au démarrage) rend `actuel` tel
/// quel : on ne rétablit rien plutôt que d'inventer un ordre.
pub fn retablir_l_ordre(actuel: &[i64], origine: &[i64]) -> Vec<i64> {
    let presents: HashSet<i64> = actuel.iter().copied().collect();
    let mut vus: HashSet<i64> = HashSet::with_capacity(actuel.len());
    let mut nouvel: Vec<i64> = origine
        .iter()
        .copied()
        .filter(|id| presents.contains(id) && vus.insert(*id))
        .collect();
    // Début de la traîne d'ajouts en fin de file : tout ce qui suit la
    // dernière ligne d'origine encore présente.
    let fin_d_origine = actuel
        .iter()
        .rposition(|id| vus.contains(id))
        .map_or(0, |i| i + 1);
    let mut traine: Vec<i64> = Vec::new();
    for (i, &id) in actuel.iter().enumerate() {
        if vus.contains(&id) {
            continue;
        }
        if i >= fin_d_origine {
            traine.push(id);
            continue;
        }
        // Ligne ajoutée au milieu : juste après sa voisine de gauche, qui est
        // déjà placée (on parcourt `actuel` dans l'ordre).
        let rang = if i == 0 {
            0
        } else {
            nouvel
                .iter()
                .position(|&x| x == actuel[i - 1])
                .map_or(nouvel.len(), |p| p + 1)
        };
        nouvel.insert(rang, id);
        vus.insert(id);
    }
    nouvel.extend(traine);
    nouvel
}

/// Le titre en cours, en rang de file : la ligne marquée en base, sinon le
/// curseur de la zone — la même règle que `GET /zones/{id}/queue`.
fn rang_courant(entrees: &[crate::db::play_queue_repo::QueueEntry], curseur: i64) -> Option<usize> {
    if entrees.is_empty() {
        return None;
    }
    if let Some(p) = entrees.iter().position(|e| e.is_current) {
        return Some(p);
    }
    usize::try_from(curseur)
        .ok()
        .map(|c| c.min(entrees.len() - 1))
}

/// Ce que la bascule a fait, pour la réponse de la route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilanAleatoire {
    /// Le mode après la bascule.
    pub shuffle: bool,
    /// Vrai si l'ordre des lignes de la file a été réécrit.
    pub reordonnee: bool,
    /// Rang du titre en cours APRÈS la bascule (il change quand l'ordre
    /// d'origine est rétabli).
    pub queue_position: i64,
    /// Longueur de la file.
    pub queue_length: i64,
}

/// Active ou désactive l'aléatoire de la zone, en réordonnant la file.
///
/// - activer (même déjà actif : c'est un nouveau tirage) rebat la suite de la
///   file à partir du titre en cours ; l'ordre d'origine retenu est celui
///   d'AVANT le premier tirage, un second tirage ne l'écrase pas ;
/// - désactiver rétablit l'ordre d'origine (voir [`retablir_l_ordre`]) et
///   recale le curseur sur le même titre ;
/// - l'événement `shuffle` part APRÈS l'écriture : un client qui relit la file
///   en le recevant lit déjà le nouvel ordre.
pub async fn basculer_l_aleatoire(
    db: &Arc<dyn DbBackend>,
    playback: &PlaybackManager,
    zone_id: i64,
    enabled: bool,
) -> Result<BilanAleatoire, String> {
    let repo = PlayQueueRepo::with_backend(db.clone());
    let mut dernier_refus = String::new();
    for _ in 0..ESSAIS {
        let etat = playback.get_state(zone_id).await;
        let entrees = repo.get_ordered_fort(zone_id)?;
        let ids: Vec<i64> = entrees.iter().map(|e| e.id).collect();
        let longueur = ids.len() as i64;
        let courant = rang_courant(&entrees, etat.queue_position);

        let (nouvel, origine) = if ids.len() < 2 {
            (None, Some(ids.clone()))
        } else if enabled {
            let origine = if etat.shuffle {
                let retenu = playback.ordre_d_origine(zone_id);
                if retenu.is_empty() {
                    ids.clone()
                } else {
                    retenu
                }
            } else {
                ids.clone()
            };
            (Some(melanger_la_suite(&ids, courant)), Some(origine))
        } else if etat.shuffle {
            let retenu = playback.ordre_d_origine(zone_id);
            (Some(retablir_l_ordre(&ids, &retenu)), None)
        } else {
            (None, None)
        };

        let mut position = courant.map_or(etat.queue_position, |c| c as i64);
        let mut reordonnee = false;
        if let Some(nouvel) = nouvel
            && nouvel != ids
        {
            if let Err(e) = repo.reordonner(zone_id, &nouvel) {
                // La file a bougé entre la lecture et l'écriture : on relit.
                dernier_refus = e;
                continue;
            }
            reordonnee = true;
            if let Some(c) = courant
                && let Some(p) = nouvel.iter().position(|&id| id == ids[c])
            {
                position = p as i64;
            }
        }
        if !ids.is_empty() {
            playback
                .update_queue_info(zone_id, position, longueur)
                .await;
        }
        match origine {
            Some(o) if enabled => playback.retenir_l_ordre_d_origine(zone_id, o),
            _ => playback.oublier_l_ordre_d_origine(zone_id),
        }
        playback.set_shuffle(zone_id, enabled).await;
        info!(
            zone_id,
            enabled, reordonnee, position, longueur, "aleatoire_file_reordonnee"
        );
        return Ok(BilanAleatoire {
            shuffle: enabled,
            reordonnee,
            queue_position: position,
            queue_length: longueur,
        });
    }
    Err(dernier_refus)
}

/// Une file NEUVE vient d'être écrite pour la zone, départ au rang `depart`.
///
/// Si l'aléatoire est actif, la file est mélangée autour du titre de départ
/// (voir [`melanger_une_file_neuve`]), l'ordre d'écriture est retenu comme
/// ordre d'origine, et le rang de départ rendu vaut `0`. Sinon rien ne bouge
/// et `depart` est rendu tel quel. Un échec d'écriture est journalisé et
/// laisse la file dans l'ordre demandé : on joue, dans l'ordre, plutôt que de
/// ne pas jouer.
pub async fn sur_file_neuve(
    db: &Arc<dyn DbBackend>,
    playback: &PlaybackManager,
    zone_id: i64,
    depart: i64,
) -> i64 {
    if !playback.get_state(zone_id).await.shuffle {
        return depart;
    }
    let repo = PlayQueueRepo::with_backend(db.clone());
    let entrees = match repo.get_ordered_fort(zone_id) {
        Ok(e) => e,
        Err(e) => {
            warn!(zone_id, error = %e, "aleatoire_file_neuve_lecture_echouee");
            return depart;
        }
    };
    let ids: Vec<i64> = entrees.iter().map(|e| e.id).collect();
    let Some(rang) = usize::try_from(depart).ok().filter(|&r| r < ids.len()) else {
        return depart;
    };
    let nouvel = melanger_une_file_neuve(&ids, rang);
    if nouvel != ids {
        if let Err(e) = repo.reordonner(zone_id, &nouvel) {
            warn!(zone_id, error = %e, "aleatoire_file_neuve_ecriture_echouee");
            return depart;
        }
        // La ligne marquée « en cours » a suivi son titre ; on la repose quand
        // même au rang 0, pour une file dont aucune ligne n'était marquée.
        repo.set_current_pos(zone_id, 0).ok();
    }
    playback.retenir_l_ordre_d_origine(zone_id, ids);
    info!(
        zone_id,
        depart,
        longueur = nouvel.len(),
        "aleatoire_file_neuve_melangee"
    );
    0
}

/// Au démarrage : rend à la zone son aléatoire tel que l'instantané le décrit.
///
/// - instantané écrit par un serveur qui mélange la file (`melange_dans_la_file`)
///   : la file en base est déjà dans l'ordre joué, on reprend le mode et
///   l'ordre d'origine retenu ;
/// - instantané plus ancien, aléatoire actif : la file en base est dans
///   l'ordre d'origine et le tirage caché est perdu — on mélange la suite
///   maintenant, sans quoi le bouton dirait « aléatoire » sur une lecture dans
///   l'ordre.
pub async fn restaurer_au_demarrage(
    db: &Arc<dyn DbBackend>,
    playback: &PlaybackManager,
    zone_id: i64,
    shuffle: bool,
    melange_dans_la_file: bool,
    ordre_d_origine: Vec<i64>,
) {
    if !shuffle {
        playback.set_shuffle(zone_id, false).await;
        return;
    }
    if melange_dans_la_file {
        playback.retenir_l_ordre_d_origine(zone_id, ordre_d_origine);
        playback.set_shuffle(zone_id, true).await;
        return;
    }
    if let Err(e) = basculer_l_aleatoire(db, playback, zone_id, true).await {
        warn!(zone_id, error = %e, "aleatoire_restauration_echouee");
        playback.set_shuffle(zone_id, true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: i64) -> Vec<i64> {
        (100..100 + n).collect()
    }

    fn trie(mut v: Vec<i64>) -> Vec<i64> {
        v.sort_unstable();
        v
    }

    #[test]
    fn la_suite_seule_est_rebattue_le_titre_en_cours_et_les_joues_restent() {
        let origine = ids(30);
        // Un tirage de 25 titres peut, en théorie, rendre l'ordre d'origine ;
        // on retire jusqu'à voir la suite bouger, sans jamais voir bouger la
        // tête.
        let mut a_bouge = false;
        for _ in 0..20 {
            let nouvel = melanger_la_suite(&origine, Some(4));
            assert_eq!(&nouvel[..5], &origine[..5], "joués + en cours figés");
            assert_eq!(trie(nouvel.clone()), origine, "une permutation");
            a_bouge |= nouvel[5..] != origine[5..];
        }
        assert!(a_bouge, "la suite n'a jamais été rebattue");
    }

    #[test]
    fn sans_titre_en_cours_toute_la_file_est_rebattue() {
        let origine = ids(30);
        let nouvel = melanger_la_suite(&origine, None);
        assert_eq!(trie(nouvel), origine);
    }

    #[test]
    fn une_file_neuve_part_du_titre_lance() {
        let origine = ids(20);
        let nouvel = melanger_une_file_neuve(&origine, 7);
        assert_eq!(nouvel[0], origine[7]);
        assert_eq!(trie(nouvel), origine);
    }

    #[test]
    fn retablir_rend_l_ordre_d_origine() {
        let origine = ids(10);
        let melangee = melanger_la_suite(&origine, Some(2));
        assert_eq!(retablir_l_ordre(&melangee, &origine), origine);
    }

    #[test]
    fn lire_ensuite_reste_le_suivant_apres_retablissement() {
        // Origine 1..=6, titre en cours 3 ; la suite a été rebattue en 6,4,5,
        // puis « Lire ensuite » a posé 99 juste après 3.
        let origine = vec![1, 2, 3, 4, 5, 6];
        let actuel = vec![1, 2, 3, 99, 6, 4, 5];
        assert_eq!(
            retablir_l_ordre(&actuel, &origine),
            vec![1, 2, 3, 99, 4, 5, 6]
        );
    }

    #[test]
    fn les_ajouts_en_fin_de_file_restent_en_fin_de_file() {
        let origine = vec![1, 2, 3, 4, 5];
        let actuel = vec![1, 2, 5, 3, 4, 50, 51];
        assert_eq!(
            retablir_l_ordre(&actuel, &origine),
            vec![1, 2, 3, 4, 5, 50, 51]
        );
    }

    #[test]
    fn une_ligne_retiree_disparait_de_l_ordre_retabli() {
        let origine = vec![1, 2, 3, 4, 5];
        let actuel = vec![1, 5, 2, 4];
        assert_eq!(retablir_l_ordre(&actuel, &origine), vec![1, 2, 4, 5]);
    }

    #[test]
    fn une_origine_etrangere_ne_touche_a_rien() {
        let actuel = vec![7, 9, 8];
        assert_eq!(retablir_l_ordre(&actuel, &[1, 2, 3]), actuel);
        assert_eq!(retablir_l_ordre(&actuel, &[]), actuel);
    }
}
