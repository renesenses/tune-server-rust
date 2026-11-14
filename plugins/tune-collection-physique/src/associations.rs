//! Les associations MANUELLES d'un disque à un album de la bibliothèque
//! (#8672, go de Bertrand du 13/11/2026).
//!
//! Un disque que le rapprochement automatique laisse « nulle part » (aucun
//! correspondant, ou plusieurs qu'on ne départage pas) s'associe à la main à
//! un album local. Le lien :
//!
//! * est rangé dans la base (`collection_physique_associations`, migration
//!   SQLite 153 / PostgreSQL 119), PAS dans la mémoire du greffon
//!   (`collection.json`) : « Effacer le cache » et les synchronisations
//!   Discogs ne le touchent pas ;
//! * est tenu par la release Discogs (`release_id`), stable d'une
//!   synchronisation à l'autre — deux exemplaires d'une même release vont au
//!   même album ;
//! * PRIME sur le rapprochement automatique ([`crate::correspondance::Par::Manuel`]) ;
//! * se défait (« Dissocier ») : le disque revient au rapprochement
//!   automatique.
//!
//! Un lien vers un album qui a quitté la bibliothèque reste rangé mais ne
//! compte pas : le disque retombe sur le rapprochement automatique, et
//! retrouve son album si celui-ci revient sous le même identifiant.

use tune_core::metadata::albums_manquants::titre_normalise;

use crate::correspondance::AlbumLocal;

/// Le rangement, dans tune-core (`collection_physique_associations`).
pub use tune_core::db::collection_physique_associations::{associer, dissocier, lire};

/// Le nombre d'albums que rend au plus une recherche.
pub const RESULTATS_MAX: usize = 50;

/// La recherche « Associer à un album » dans la bibliothèque : les albums
/// locaux dont l'artiste et le titre, normalisés (minuscules, sans accents
/// ni ponctuation), contiennent TOUS les mots de la requête. Les albums dont
/// le titre contient toute la requête d'abord, puis par artiste et titre.
pub fn chercher<'a>(albums: &'a [AlbumLocal], requete: &str) -> Vec<&'a AlbumLocal> {
    let q = titre_normalise(requete);
    let mots: Vec<&str> = q.split(' ').filter(|m| !m.is_empty()).collect();
    if mots.is_empty() {
        return Vec::new();
    }
    let mut trouves: Vec<(bool, String, String, &AlbumLocal)> = albums
        .iter()
        .filter_map(|a| {
            let titre = titre_normalise(&a.titre);
            let artiste = a
                .artiste
                .as_deref()
                .map(titre_normalise)
                .unwrap_or_default();
            let tout = format!(" {artiste} {titre} ");
            mots.iter()
                .all(|m| tout.contains(&format!(" {m}")))
                .then(|| (!titre.contains(q.as_str()), artiste, titre, a))
        })
        .collect();
    trouves.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.id.cmp(&b.3.id))
    });
    trouves
        .into_iter()
        .take(RESULTATS_MAX)
        .map(|(_, _, _, a)| a)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: i64, titre: &str, artiste: &str) -> AlbumLocal {
        AlbumLocal {
            id,
            titre: titre.into(),
            artiste: Some(artiste.into()),
            ..Default::default()
        }
    }

    #[test]
    fn la_recherche_veut_tous_les_mots_sans_accents_8672() {
        let albums = vec![
            album(1, "Abbey Road", "The Beatles"),
            album(2, "Let It Be", "The Beatles"),
            album(3, "Café Tacvba", "Café Tacvba"),
            album(4, "Road to Nowhere", "Talking Heads"),
            album(5, "Highway", "Road Runners"),
        ];
        let ids = |q: &str| {
            chercher(&albums, q)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("beatles abbey"), vec![1]);
        assert_eq!(ids("beatles"), vec![1, 2]);
        assert_eq!(ids("cafe"), vec![3], "sans accents");
        // Les titres qui contiennent la requête d'abord (par artiste), puis
        // l'album trouvé par son seul artiste.
        assert_eq!(ids("road"), vec![4, 1, 5]);
        // Début de mot seulement : « oad » ne trouve pas « Road ».
        assert!(ids("oad").is_empty());
        assert!(ids("  ").is_empty(), "une requête vide ne rend rien");
    }
}
