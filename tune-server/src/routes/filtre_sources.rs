//! `sources` — QUI repond, la bibliotheque de la machine ou les services.
//!
//! ## Pourquoi ce module existe
//!
//! Plusieurs routes melangent, dans une meme reponse, ce que la machine
//! possede et ce que les services de streaming proposent. Jusqu'ici chacune
//! decidait seule, et aucune ne se laissait dire ce qu'on voulait :
//!
//! - `GET /search` — corrige par #3226 ;
//! - `GET /home/other-versions` ;
//! - `GET /home/artist-releases` ;
//! - `GET /library/tracks/{id}/versions`.
//!
//! Le contrat est le MEME pour l'utilisateur : une pilule « Local », une
//! pilule par service, une pilule « Tous ». Il ne doit donc exister qu'UNE
//! lecture de `sources`, et c'est celle-ci.
//!
//! ## Le contrat
//!
//! | `sources`                | local (et radios) | services               |
//! |--------------------------|-------------------|------------------------|
//! | absent                   | rendu             | tous ceux authentifies |
//! | `local`                  | rendu             | aucun                  |
//! | `all`                    | rendu             | tous ceux authentifies |
//! | `qobuz` (un service)     | **vide**          | ce service             |
//! | `local,qobuz`            | rendu             | ce service             |
//! | valeur inconnue, ou vide | **vide**          | aucun                  |
//!
//! **Absent ne change rien.** C'est la pilule « Tous », le seul cas qui
//! marchait deja partout, et le seul temoin de non-regression qui vaille.
//!
//! **Present, `sources` est une liste blanche EXPLICITE**, et le local y entre
//! sous son propre jeton. Elle ne se replie pas sur « tout » quand elle ne
//! reconnait rien : `sources=service-inexistant` ne selectionne aucun service —
//! ce que la boucle des services faisait deja — ET aucun local, ce qui est la
//! meme regle appliquee aux deux moities.
//!
//! Ce n'est pas une invention du serveur : c'est la regle que le client
//! applique DEJA de son cote, `includeLocal = !activeSources ||
//! activeSources.includes('local')` (`tune-web-client`,
//! `src/components/SearchView.svelte`). Le serveur cesse simplement de la
//! contredire.
//!
//! ## ⚠️ Vivier n'est pas contenu
//!
//! Ce filtre dit ce qui est RENDU, jamais ce qui est LU pour y arriver. Trois
//! routes sur quatre partent d'une donnee locale qui est leur ENTREE :
//!
//! - `/library/tracks/{id}/versions` lit la ligne `tracks` de la piste
//!   designee pour connaitre titre, artiste, ISRC, duree. `sources=qobuz` ne
//!   doit pas la faire repondre 404 : la piste reste le sujet de la question.
//! - `/home/other-versions` lit `listen_history` pour savoir QUOI chercher.
//!   `sources=qobuz` doit toujours pouvoir chercher les dernieres ecoutes chez
//!   Qobuz.
//! - `/home/artist-releases` lit les artistes possedes pour savoir QUI suivre.
//!
//! Couper ces lectures-la ne filtrerait pas la reponse, elle la viderait.
//!
//! ## Une seule implementation, et c'est celle-ci
//!
//! La PR #3265 (#3226, `GET /search`) avait pose ce contrat la premiere, sous
//! la forme d'une fonction `le_local_est_demande` et de deux constantes
//! `JETON_LOCAL` / `JETON_TOUTES` locales a `routes/search.rs`. Elle a fusionne
//! pendant l'ecriture de ce module.
//!
//! Ces trois elements ont donc ete SUPPRIMES de `search.rs`, qui appelle
//! desormais [`FiltreSources`] comme les trois autres routes. Ecrire un second
//! predicat a cote du premier aurait fait deux implementations d'une meme
//! regle, libres de diverger a la premiere correction — le defaut qu'on passe
//! la semaine a corriger ailleurs.
//!
//! La substitution n'a rien change au comportement de `/search` : les
//! semantiques etaient identiques, jeton pour jeton et bord pour bord, et
//! `tests/recherche_sources_i3226.rs` — le banc de #3265, garde tel quel — le
//! verifie encore.

/// Le jeton qui designe la bibliotheque de la machine. C'est celui que les
/// clients envoient deja pour leur pilule « Local ».
pub(crate) const JETON_LOCAL: &str = "local";

/// Le joker « toutes les sources ». Le serveur l'acceptait deja pour les
/// services ; il vaut donc aussi pour le local, sans quoi `sources=all`
/// rendrait MOINS que `sources` absent.
pub(crate) const JETON_TOUTES: &str = "all";

/// La lecture de `sources`, faite une seule fois.
///
/// `None` = parametre absent = « Tous ». `Some(liste)` = selection explicite,
/// meme vide — et une selection vide ne selectionne rien.
#[derive(Debug, Clone, Default)]
pub(crate) struct FiltreSources(Option<Vec<String>>);

impl FiltreSources {
    /// Lit le parametre brut de l'URL.
    ///
    /// La liste est separee par des virgules et chaque jeton est deborde de
    /// ses espaces — `sources=local, qobuz` est ce qu'un humain ecrit.
    ///
    /// ⚠️ `sources=` (present mais vide) rend `Some(vec![""])`, donc une
    /// selection qui ne reconnait rien : ni local, ni service. C'est la ligne
    /// « valeur vide » du contrat, et elle tombe toute seule de la regle
    /// generale — il n'y a pas de cas particulier a maintenir.
    pub(crate) fn depuis(brut: Option<&str>) -> Self {
        Self(brut.map(|s| s.split(',').map(|j| j.trim().to_string()).collect()))
    }

    /// « Tous » : ce que rend un appel sans `sources`. Le comportement
    /// d'avant, et le point fixe de toute non-regression.
    ///
    /// `#[cfg(test)]` : en production, ce cas arrive par
    /// `depuis(None)` — les handlers lisent toujours l'URL. Ce raccourci ne
    /// sert qu'aux essais, ou il DIT que l'essai ne filtre rien, la ou un
    /// `depuis(None)` laisserait croire a un oubli.
    #[cfg(test)]
    pub(crate) fn tout() -> Self {
        Self(None)
    }

    /// La bibliotheque de la machine (et les radios) entre-t-elle dans cette
    /// reponse ?
    pub(crate) fn local_demande(&self) -> bool {
        match &self.0 {
            None => true,
            Some(liste) => liste.iter().any(|s| s == JETON_LOCAL || s == JETON_TOUTES),
        }
    }

    /// Ce service repond-il dans cette reponse ?
    ///
    /// Le nom est celui du registre (`qobuz`, `tidal`, `deezer`, `spotify`,
    /// `bandcamp`…), compare tel quel : le registre est la seule autorite sur
    /// l'orthographe d'un service, et une normalisation ici en creerait une
    /// seconde.
    pub(crate) fn service_demande(&self, nom: &str) -> bool {
        match &self.0 {
            None => true,
            Some(liste) => liste.iter().any(|s| s == nom || s == JETON_TOUTES),
        }
    }
}
