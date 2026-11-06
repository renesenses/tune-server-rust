//! « Chercher sur les services » (#7928, 05/11/2026).
//!
//! Sur un disque absent de la bibliothèque, l'écran demande, À LA DEMANDE,
//! les albums les plus proches chez chaque service connecté qui sait
//! chercher (Qobuz, TIDAL, Spotify, Deezer…, [`Hote::services_de_recherche`]),
//! pas seulement chez Qobuz et TIDAL que la synchronisation interroge.
//!
//! Les requêtes, dans l'ordre, pour chaque service : les codes-barres de la
//! fiche Discogs (lue une fois si elle ne l'est pas encore), les numéros de
//! catalogue, puis artiste + titre. Un service s'arrête à la première
//! requête qui donne l'album exact (même règle que la synchronisation,
//! [`choisir_sur_le_service`]). Les candidats sont notés et rangés du plus
//! proche au moins proche ; l'écran ouvre ou lance le premier.
//!
//! Rien n'est écrit dans la mémoire : un résultat trouvé ici n'est pas un
//! rapprochement, l'utilisateur juge.
//!
//! [`Hote::services_de_recherche`]: crate::Hote::services_de_recherche

use std::collections::HashSet;

use serde::Serialize;
use tune_core::metadata::albums_manquants::titre_normalise;

use crate::correspondance::{
    AlbumDeService, artiste_normalise, choisir_sur_le_service, cles_d_artiste,
};
use crate::discogs::Disque;
use crate::synchro::{Moteur, attendre, verrou};

/// Au plus tant de candidats par service dans la réponse.
pub const CANDIDATS_PAR_SERVICE: usize = 3;

/// Comment un candidat a été trouvé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Requete {
    CodeBarre,
    Catalogue,
    ArtisteTitre,
}

/// Un album proposé par un service, et sa proximité avec le disque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proposition {
    pub service: String,
    pub id: String,
    pub titre: String,
    pub artiste: String,
    pub par: Requete,
    /// De 0 à 100.
    pub score: u32,
    /// Le même album, selon la règle de la synchronisation.
    pub exact: bool,
}

/// Ce que la recherche a donné.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Resultat {
    /// Les services interrogés.
    pub services: Vec<String>,
    /// Ceux qui n'ont pas répondu à au moins une requête.
    pub sans_reponse: Vec<String>,
    /// Du plus proche au moins proche.
    pub propositions: Vec<Proposition>,
}

impl Resultat {
    /// Le plus proche, s'il y en a un.
    pub fn meilleure(&self) -> Option<&Proposition> {
        self.propositions.first()
    }
}

fn mots(s: &str) -> HashSet<String> {
    titre_normalise(s)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// La proximité d'un candidat avec le disque, de 0 à 100 : le titre compte
/// pour 60 (identique) ou en proportion des mots communs, l'artiste pour 30,
/// un code-barre ou un numéro de catalogue pour 10.
pub fn score(d: &Disque, c: &AlbumDeService, par: Requete) -> u32 {
    let titre = if titre_normalise(&c.titre) == titre_normalise(&d.titre) {
        60
    } else {
        let (a, b) = (mots(&d.titre), mots(&c.titre));
        let union = a.union(&b).count();
        if union == 0 {
            0
        } else {
            (a.intersection(&b).count() * 50 / union) as u32
        }
    };
    let artiste = if cles_d_artiste(d).contains(&artiste_normalise(&c.artiste)) {
        30
    } else {
        0
    };
    let identifiant = if par == Requete::ArtisteTitre { 0 } else { 10 };
    titre + artiste + identifiant
}

impl Moteur {
    /// Les requêtes d'un disque : codes-barres, numéros de catalogue, puis
    /// artiste + titre.
    fn requetes(d: &Disque, codes: &[String]) -> Vec<(String, Requete)> {
        let mut r: Vec<(String, Requete)> = codes
            .iter()
            .map(|c| (c.clone(), Requete::CodeBarre))
            .collect();
        for n in &d.numeros_catalogue {
            let n = n.trim();
            if n.len() >= 3 && !r.iter().any(|(q, _)| q == n) {
                r.push((n.to_string(), Requete::Catalogue));
            }
        }
        r.push((format!("{} {}", d.artiste, d.titre), Requete::ArtisteTitre));
        r
    }

    /// Cherche le disque `release` de la liste chez chaque service connecté
    /// qui sait chercher. `None` : pas dans la liste.
    pub async fn chercher_sur_les_services(&self, release: u64) -> Option<Resultat> {
        let d = {
            let m = verrou(&self.memoire);
            m.disques.iter().find(|d| d.release_id == release).cloned()
        }?;
        // La fiche (codes-barres) : lue une fois si la synchronisation ne
        // l'a pas encore fait ; une panne de Discogs n'empêche pas de
        // chercher par catalogue et par artiste + titre.
        let codes = self.lire_la_fiche(release).await.unwrap_or_default();
        let services = self.hote.services_de_recherche().await;
        let requetes = Self::requetes(&d, &codes);
        let mut resultat = Resultat {
            services: services.clone(),
            ..Default::default()
        };
        for service in &services {
            let mut du_service: Vec<Proposition> = Vec::new();
            for (q, par) in &requetes {
                attendre(self.cadence.service).await;
                let Ok(candidats) = self.hote.chercher_albums(service, q).await else {
                    if !resultat.sans_reponse.contains(service) {
                        resultat.sans_reponse.push(service.clone());
                    }
                    continue;
                };
                let par_code = *par != Requete::ArtisteTitre;
                let exact = choisir_sur_le_service(&d, &candidats, par_code).map(|a| a.id.clone());
                for c in &candidats {
                    let p = Proposition {
                        service: service.clone(),
                        id: c.id.clone(),
                        titre: c.titre.clone(),
                        artiste: c.artiste.clone(),
                        par: *par,
                        score: score(&d, c, *par),
                        exact: exact.as_deref() == Some(c.id.as_str()),
                    };
                    match du_service.iter_mut().find(|x| x.id == p.id) {
                        Some(x) if (p.exact, p.score) > (x.exact, x.score) => *x = p,
                        Some(_) => {}
                        None => du_service.push(p),
                    }
                }
                if exact.is_some() {
                    break;
                }
            }
            du_service.sort_by(|a, b| (b.exact, b.score).cmp(&(a.exact, a.score)));
            du_service.retain(|p| p.score > 0);
            du_service.truncate(CANDIDATS_PAR_SERVICE);
            resultat.propositions.extend(du_service);
        }
        // Stable : à égalité, l'ordre des services.
        resultat
            .propositions
            .sort_by(|a, b| (b.exact, b.score).cmp(&(a.exact, a.score)));
        tracing::info!(
            target: "plugin",
            release,
            services = services.len(),
            propositions = resultat.propositions.len(),
            "collection_physique_recherche_services"
        );
        Some(resultat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discogs::Famille;

    fn disque() -> Disque {
        Disque {
            instance_id: 1,
            release_id: 10,
            master_id: None,
            artistes: vec!["Miles Davis".into()],
            artiste: "Miles Davis".into(),
            titre: "Kind of Blue".into(),
            annee: None,
            labels: vec![],
            numeros_catalogue: vec!["CL 1355".into(), "CL 1355".into(), "-".into()],
            formats: vec![],
            precisions: vec![],
            famille: Famille::Vinyle,
            ajoute_le: None,
            vignette: None,
            pochette: None,
            dossier: None,
            genres: vec![],
            styles: vec![],
        }
    }

    fn album(titre: &str, artiste: &str) -> AlbumDeService {
        AlbumDeService {
            id: "x".into(),
            titre: titre.into(),
            artiste: artiste.into(),
        }
    }

    #[test]
    fn le_meme_album_passe_devant_un_titre_voisin() {
        let d = disque();
        let exact = score(
            &d,
            &album("Kind Of Blue", "Miles Davis"),
            Requete::ArtisteTitre,
        );
        let voisin = score(
            &d,
            &album("Kind of Blue Live", "Miles Davis"),
            Requete::ArtisteTitre,
        );
        let autre = score(
            &d,
            &album("Kind of Blue", "Tribute Band"),
            Requete::ArtisteTitre,
        );
        assert_eq!(exact, 90);
        assert!(voisin < exact && voisin > 30, "{voisin}");
        assert!(autre < exact, "{autre}");
        assert_eq!(
            score(
                &d,
                &album("Bitches Brew", "Weather Report"),
                Requete::ArtisteTitre
            ),
            0
        );
    }

    #[test]
    fn les_requetes_vont_du_code_barre_a_l_artiste_et_au_titre() {
        let r = Moteur::requetes(&disque(), &["074646193527".into()]);
        assert_eq!(
            r,
            vec![
                ("074646193527".to_string(), Requete::CodeBarre),
                ("CL 1355".to_string(), Requete::Catalogue),
                (
                    "Miles Davis Kind of Blue".to_string(),
                    Requete::ArtisteTitre
                ),
            ],
            "le catalogue en double et le « - » sont écartés"
        );
    }
}
