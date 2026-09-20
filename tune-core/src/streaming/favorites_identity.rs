//! L'identité STABLE d'un favori de service (#4577).
//!
//! # Le défaut, mesuré
//!
//! FabienM, fil 1862 point 4, en 0.9.158 : « on peut mettre un titre bandcamp
//! en favori mais celui-ci n'est pas conservé » (exemple donné : *Tiny
//! Darkness* de Soda Blonde).
//!
//! Le favori était pourtant bien ÉCRIT. La table `streaming_favorites` a pour
//! clé `(profile_id, item_type, service, service_id)`, et pour un titre
//! Bandcamp le `service_id` est l'URL de flux — c'est délibéré, `StreamTrack::id`
//! porte l'URL mp3-128 parce que `resolve_direct_url` la joue telle quelle
//! (voir `plugins/tune-bandcamp/src/service.rs`).
//!
//! Or **cette URL n'est pas stable**. Mesuré le 20/09/2026, deux lectures de
//! `https://sodablonde.bandcamp.com/album/dream-big` à trois secondes
//! d'intervalle, même piste « Midnight Show » :
//!
//! ```text
//! .../stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545
//!     ?p=0&ts=1789982173&t=a130059f…&token=1789982173_7285db07…
//! .../stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545
//!     ?p=0&ts=1789982176&t=d8bb27b0…&token=1789982176_edadeaf9…
//! ```
//!
//! Le CHEMIN est identique — l'empreinte et l'identifiant de piste de Bandcamp
//! ne bougent pas — mais `ts`, `t` et `token` sont resignés à chaque requête.
//! Une ligne écrite sous la première URL n'est donc jamais retrouvée sous la
//! seconde : le cœur repart vide au rechargement suivant, le retrait ne trouve
//! rien à retirer, et la reprise réécrit une ligne de plus à chaque passage.
//! C'est exactement « pas conservé », sans aucune erreur pour l'annoncer.
//!
//! # Pourquoi on ne peut pas simplement stabiliser l'URL de flux
//!
//! Parce que la signature est EXIGÉE pour lire. Mesuré le même jour, sur la
//! même piste : le chemin nu, sans requête, rend **403**. L'URL jouable doit
//! donc rester signée ; c'est l'identité du FAVORI, et elle seule, qui se
//! normalise.
//!
//! # La règle
//!
//! [`identite_de_favori`] retire la requête et le fragment d'une URL de flux
//! `bcbits.com`, et ne touche à rien d'autre. Elle se décide sur la FORME de
//! l'identifiant, pas sur le nom du service : le client web étiquette les
//! vignettes de l'onglet Bandcamp avec sa clé locale `__bandcamp__` aussi bien
//! qu'avec `bandcamp`, et une règle qui lirait le nom laisserait passer la
//! moitié des cas.
//!
//! ⚠️ Son jumeau côté client vit dans `tune-web-client`,
//! `src/lib/bandcampFavori.ts` : le magasin de cœurs du client est indexé par
//! `service:service_id`, donc la clé construite depuis une vignette fraîche
//! doit tomber sur celle de la ligne enregistrée ici. Les deux portent le même
//! jeu de cas.

use std::borrow::Cow;

/// L'hôte des flux Bandcamp. Les CDN se numérotent (`t4`, `t5`…), donc on
/// reconnaît le domaine, pas un sous-domaine précis.
const DOMAINE_FLUX_BANDCAMP: &str = "bcbits.com";

/// L'identité durable d'un favori de service, à partir de l'identifiant que le
/// client ou le service a donné.
///
/// Rend l'identifiant inchangé — donc `Cow::Borrowed`, sans allocation — dans
/// tous les cas sauf celui qui le demande : une URL de flux Bandcamp, dont la
/// requête est resignée à chaque lecture (voir l'en-tête du module).
///
/// Volontairement tolérante : un identifiant vide, un identifiant qui n'est pas
/// une URL, une URL d'album ou de page d'artiste ressortent tels quels. Ce
/// n'est pas une validation, c'est une normalisation.
pub fn identite_de_favori(service_id: &str) -> Cow<'_, str> {
    if !est_un_flux_bandcamp(service_id) {
        return Cow::Borrowed(service_id);
    }
    // Couper au PREMIER des deux séparateurs : une URL peut porter un fragment
    // sans requête, et `?` après `#` appartiendrait alors au fragment.
    match service_id.find(['?', '#']) {
        Some(i) => Cow::Borrowed(&service_id[..i]),
        None => Cow::Borrowed(service_id),
    }
}

/// Une URL de flux Bandcamp, telle que `data-tralbum` la sert.
///
/// Les deux conditions comptent. L'hôte seul attraperait les pochettes
/// (`f4.bcbits.com/img/…`), dont l'URL est stable et sert d'identité ailleurs ;
/// `/stream/` seul attraperait les flux d'autres services.
///
/// 🔴 Le domaine se compare par COMPOSANT (`== bcbits.com` ou `.bcbits.com`) et
/// jamais par fin de chaîne : `evilbcbits.com` se termine par `bcbits.com`, et
/// une garde en `ends_with` lui laisserait tronquer nos identifiants.
fn est_un_flux_bandcamp(id: &str) -> bool {
    let Some(reste) = id
        .strip_prefix("https://")
        .or_else(|| id.strip_prefix("http://"))
    else {
        return false;
    };
    let Some(fin_hote) = reste.find('/') else {
        return false;
    };
    let hote = &reste[..fin_hote];
    let bon_domaine = hote == DOMAINE_FLUX_BANDCAMP
        || hote
            .strip_suffix(DOMAINE_FLUX_BANDCAMP)
            .is_some_and(|prefixe| prefixe.ends_with('.'));
    bon_domaine && reste[fin_hote..].starts_with("/stream/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les deux URL MESURÉES le 20/09/2026 sur « Midnight Show », à trois
    /// secondes d'intervalle. C'est le cas de FabienM, tel quel.
    const SIGNEE_A: &str = "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545?p=0&ts=1789982173&t=a130059f109193afe2e59864b82c20f5f3446c7d&token=1789982173_7285db0763aa47e79bc49785feea7c459b90ec0e";
    const SIGNEE_B: &str = "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545?p=0&ts=1789982176&t=d8bb27b0915e21b4430a9c4eca79f57ac9fb39c7&token=1789982176_edadeaf99874538aee5934700e58a102414dfd8c";

    #[test]
    fn deux_signatures_de_la_meme_piste_donnent_la_meme_identite() {
        assert_ne!(SIGNEE_A, SIGNEE_B, "les deux mesures doivent différer");
        assert_eq!(identite_de_favori(SIGNEE_A), identite_de_favori(SIGNEE_B));
    }

    #[test]
    fn l_identite_garde_l_empreinte_et_l_identifiant_de_piste() {
        assert_eq!(
            identite_de_favori(SIGNEE_A),
            "https://t4.bcbits.com/stream/58db28886c8795a747dc69be6491159c/mp3-128/2639113545"
        );
    }

    #[test]
    fn deux_pistes_differentes_gardent_deux_identites() {
        let autre = "https://t4.bcbits.com/stream/2b7a93eadf73639d40ff53eb25f11703/mp3-128/2031798614?p=0&ts=1789982173&t=6cea60c";
        assert_ne!(identite_de_favori(SIGNEE_A), identite_de_favori(autre));
    }

    /// Un autre CDN du même domaine : `t4` n'est pas une constante de Bandcamp.
    #[test]
    fn un_autre_cdn_du_meme_domaine_est_reconnu() {
        assert_eq!(
            identite_de_favori("https://t5.bcbits.com/stream/abc/mp3-128/42?ts=1"),
            "https://t5.bcbits.com/stream/abc/mp3-128/42"
        );
    }

    /// L'URL d'une page d'album EST l'identifiant d'un album Bandcamp, et elle
    /// est déjà stable : y toucher casserait le favori d'album.
    #[test]
    fn une_page_d_album_ne_bouge_pas() {
        let url = "https://sodablonde.bandcamp.com/album/dream-big";
        assert_eq!(identite_de_favori(url), url);
    }

    /// Une pochette est servie par le même domaine, sans `/stream/`, et son URL
    /// est stable : la couper retirerait une information utile.
    #[test]
    fn une_pochette_bcbits_ne_bouge_pas() {
        let url = "https://f4.bcbits.com/img/a4029072179_10.jpg?v=2";
        assert_eq!(identite_de_favori(url), url);
    }

    /// Le flux d'un AUTRE service garde sa requête : elle peut être son
    /// identité.
    #[test]
    fn un_flux_d_un_autre_service_ne_bouge_pas() {
        let url = "https://streaming.qobuz.com/stream/1234?format=27";
        assert_eq!(identite_de_favori(url), url);
    }

    #[test]
    fn un_identifiant_numerique_ne_bouge_pas() {
        assert_eq!(identite_de_favori("123456"), "123456");
        assert_eq!(identite_de_favori(""), "");
    }

    /// Un domaine qui se TERMINE par le nôtre sans l'être — la garde doit
    /// porter sur le composant d'hôte, pas sur une sous-chaîne.
    #[test]
    fn un_domaine_sosie_n_est_pas_bandcamp() {
        for url in [
            "https://evilbcbits.com/stream/x?ts=1",
            "https://bcbits.com.attaquant.example/stream/x?ts=1",
        ] {
            assert_eq!(identite_de_favori(url), url, "sosie accepté : {url}");
        }
    }

    /// Et le domaine nu, lui, EST le nôtre.
    #[test]
    fn le_domaine_nu_est_reconnu() {
        assert_eq!(
            identite_de_favori("https://bcbits.com/stream/abc/mp3-128/42?ts=1"),
            "https://bcbits.com/stream/abc/mp3-128/42"
        );
    }

    #[test]
    fn un_fragment_seul_est_coupe_aussi() {
        assert_eq!(
            identite_de_favori("https://t4.bcbits.com/stream/abc/mp3-128/42#t=10"),
            "https://t4.bcbits.com/stream/abc/mp3-128/42"
        );
    }

    /// Rejouer la normalisation ne change plus rien : c'est ce qui permet de
    /// l'appeler à l'écriture ET à la lecture sans se demander qui l'a déjà
    /// appelée.
    #[test]
    fn la_normalisation_est_idempotente() {
        let une_fois = identite_de_favori(SIGNEE_A).into_owned();
        assert_eq!(identite_de_favori(&une_fois), une_fois);
    }
}
