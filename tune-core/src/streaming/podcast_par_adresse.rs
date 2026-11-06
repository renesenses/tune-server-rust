//! Ajouter un podcast à la main, par l'adresse de son flux (#7819).
//!
//! L'utilisateur colle une adresse ; le serveur la normalise, télécharge le
//! flux, vérifie que c'est bien un flux de podcast (RSS ou Atom) et en lit
//! l'en-tête : titre, auteur, image et description. Une page HTML, un JSON ou
//! une adresse injoignable sont refusés avec un code stable, que la route
//! traduit.

use std::time::Duration;

use reqwest::{Client, Url};
use tracing::debug;

use super::podcasts::{extraire_balise, image_de_la_chaine, texte_sans_balises};

/// Au-delà, le flux est déclaré injoignable : l'utilisateur attend devant son
/// champ, il ne doit pas attendre la minute du client HTTP partagé.
pub const DELAI_DE_LECTURE: Duration = Duration::from_secs(20);

/// Taille lue au plus. Les gros flux (des milliers d'épisodes) dépassent
/// rarement quelques Mo ; l'en-tête, seul utile ici, est au début.
pub const TAILLE_MAX: usize = 16 * 1024 * 1024;

/// Pourquoi une adresse n'a pas donné d'abonnement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchecAjoutParAdresse {
    /// Ce n'est pas une adresse web (ni http, ni https, ni feed).
    AdresseInvalide,
    /// Le serveur distant ne répond pas, ou répond en erreur.
    Injoignable(String),
    /// L'adresse répond, mais pas par un flux RSS ou Atom.
    PasUnFlux,
}

impl EchecAjoutParAdresse {
    /// Code stable, lu par les clients.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AdresseInvalide => "podcast_adresse_invalide",
            Self::Injoignable(_) => "podcast_flux_injoignable",
            Self::PasUnFlux => "podcast_pas_un_flux",
        }
    }
}

/// L'en-tête d'un flux de podcast.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnteteDuFlux {
    pub titre: Option<String>,
    pub auteur: Option<String>,
    pub image_url: Option<String>,
    pub description: Option<String>,
}

/// L'adresse telle qu'elle sera enregistrée : sans espaces autour, `feed://`
/// ramené à `http://` (`feed:https://…` à `https://…`), `https://` ajouté à
/// une adresse collée sans schéma, schéma et hôte en minuscules. Tout autre
/// schéma que http et https est refusé.
pub fn normaliser_adresse_de_flux(brut: &str) -> Result<String, EchecAjoutParAdresse> {
    let brut = brut.trim();
    if brut.is_empty() {
        return Err(EchecAjoutParAdresse::AdresseInvalide);
    }
    let adresse = match prefixe_insensible(brut, "feed:") {
        Some(reste) if reste.starts_with("//") => format!("http:{reste}"),
        Some(reste) => reste.to_string(),
        None if !brut.contains("://") => format!("https://{brut}"),
        None => brut.to_string(),
    };
    let url = Url::parse(&adresse).map_err(|_| EchecAjoutParAdresse::AdresseInvalide)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(EchecAjoutParAdresse::AdresseInvalide);
    }
    Ok(url.to_string())
}

fn prefixe_insensible<'a>(texte: &'a str, prefixe: &str) -> Option<&'a str> {
    let tete = texte.get(..prefixe.len())?;
    tete.eq_ignore_ascii_case(prefixe)
        .then(|| &texte[prefixe.len()..])
}

/// Télécharge le flux à `adresse` (déjà normalisée) et en lit l'en-tête.
pub async fn lire_le_flux(
    client: &Client,
    adresse: &str,
) -> Result<EnteteDuFlux, EchecAjoutParAdresse> {
    let texte = tokio::time::timeout(DELAI_DE_LECTURE, telecharger(client, adresse))
        .await
        .map_err(|_| EchecAjoutParAdresse::Injoignable("timeout".into()))??;
    debug!(adresse, octets = texte.len(), "podcast_flux_par_adresse_lu");
    lire_l_entete(&texte)
}

async fn telecharger(client: &Client, adresse: &str) -> Result<String, EchecAjoutParAdresse> {
    let mut reponse = client
        .get(adresse)
        .header(
            "Accept",
            "application/rss+xml, application/atom+xml, application/xml, text/xml, */*",
        )
        .send()
        .await
        .map_err(|e| {
            EchecAjoutParAdresse::Injoignable(crate::http::client::decrire_erreur_http(&e))
        })?;
    if !reponse.status().is_success() {
        return Err(EchecAjoutParAdresse::Injoignable(format!(
            "HTTP {}",
            reponse.status()
        )));
    }
    let mut octets = Vec::new();
    while let Some(morceau) = reponse.chunk().await.map_err(|e| {
        EchecAjoutParAdresse::Injoignable(crate::http::client::decrire_erreur_http(&e))
    })? {
        octets.extend_from_slice(&morceau);
        if octets.len() >= TAILLE_MAX {
            octets.truncate(TAILLE_MAX);
            break;
        }
    }
    Ok(String::from_utf8_lossy(&octets).into_owned())
}

/// Vérifie que `xml` est un flux RSS (racine `<rss>` ou `<rdf:RDF>` avec un
/// `<channel>`) ou Atom (racine `<feed>`), puis lit son en-tête : ce qui
/// précède le premier épisode.
pub fn lire_l_entete(xml: &str) -> Result<EnteteDuFlux, EchecAjoutParAdresse> {
    let racine = nom_de_la_racine(xml).ok_or(EchecAjoutParAdresse::PasUnFlux)?;
    let atom = match racine {
        "rss" | "rdf:RDF" if xml.contains("<channel") => false,
        "feed" | "atom:feed" => true,
        _ => return Err(EchecAjoutParAdresse::PasUnFlux),
    };
    let debut_des_episodes = ["<item", "<entry"]
        .iter()
        .filter_map(|b| xml.find(b))
        .min()
        .unwrap_or(xml.len());
    let tete = &xml[..debut_des_episodes];
    // Le `<title>` du bloc `<image>` d'un RSS n'est pas celui du podcast.
    let tete_sans_image = sans_bloc(tete, "<image", "</image>");

    let texte = |balise: &str| {
        extraire_balise(&tete_sans_image, balise)
            .map(|t| texte_sans_balises(&t).trim().to_string())
            .filter(|t| !t.is_empty())
    };
    let titre = texte("title");
    let auteur = texte("itunes:author").or_else(|| {
        extraire_balise(tete, "author")
            .and_then(|a| extraire_balise(&a, "name"))
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
    });
    let description = if atom {
        texte("subtitle").or_else(|| texte("itunes:summary"))
    } else {
        texte("description").or_else(|| texte("itunes:summary"))
    };
    let image_url = Some(image_de_la_chaine(tete))
        .filter(|i| !i.is_empty())
        .or_else(|| texte("logo"))
        .or_else(|| texte("icon"));
    Ok(EnteteDuFlux {
        titre,
        auteur,
        image_url,
        description,
    })
}

/// Le nom de l'élément racine, après le BOM, les déclarations `<?…?>`, les
/// commentaires et un `<!DOCTYPE …>`.
fn nom_de_la_racine(xml: &str) -> Option<&str> {
    let mut reste = xml.trim_start_matches('\u{feff}');
    loop {
        reste = reste.trim_start();
        if let Some(r) = reste.strip_prefix("<?") {
            reste = &r[r.find("?>")? + 2..];
        } else if let Some(r) = reste.strip_prefix("<!--") {
            reste = &r[r.find("-->")? + 3..];
        } else if let Some(r) = reste.strip_prefix("<!") {
            reste = &r[r.find('>')? + 1..];
        } else {
            let r = reste.strip_prefix('<')?;
            let fin = r.find(|c: char| c.is_whitespace() || c == '>' || c == '/')?;
            return Some(&r[..fin]);
        }
    }
}

fn sans_bloc(texte: &str, ouverture: &str, fermeture: &str) -> String {
    if let Some(debut) = texte.find(ouverture)
        && let Some(fin) = texte[debut..].find(fermeture)
    {
        let mut reste = texte[..debut].to_string();
        reste.push_str(&texte[debut + fin + fermeture.len()..]);
        return reste;
    }
    texte.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
<channel>
  <image><url>https://ex.test/petite.jpg</url><title>Logo du site</title></image>
  <title>Les Pieds sur terre</title>
  <itunes:author>France Culture</itunes:author>
  <description><![CDATA[<p>Des histoires vraies.</p>]]></description>
  <itunes:image href="https://ex.test/grande.jpg"/>
  <item><title>Épisode 1</title><enclosure url="https://ex.test/1.mp3" type="audio/mpeg"/></item>
</channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- un commentaire avant la racine -->
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Un podcast Atom</title>
  <subtitle>Sous-titre &amp; résumé</subtitle>
  <author><name>Jeanne</name></author>
  <logo>https://ex.test/logo.png</logo>
  <entry><title>E1</title><link rel="enclosure" href="https://ex.test/e1.mp3"/></entry>
</feed>"#;

    #[test]
    fn un_flux_rss_donne_son_titre_son_auteur_son_image_et_sa_description() {
        let e = lire_l_entete(RSS).unwrap();
        assert_eq!(e.titre.as_deref(), Some("Les Pieds sur terre"));
        assert_eq!(e.auteur.as_deref(), Some("France Culture"));
        assert_eq!(e.image_url.as_deref(), Some("https://ex.test/grande.jpg"));
        assert_eq!(e.description.as_deref(), Some("Des histoires vraies."));
    }

    #[test]
    fn un_flux_atom_est_accepte() {
        let e = lire_l_entete(ATOM).unwrap();
        assert_eq!(e.titre.as_deref(), Some("Un podcast Atom"));
        assert_eq!(e.auteur.as_deref(), Some("Jeanne"));
        assert_eq!(e.image_url.as_deref(), Some("https://ex.test/logo.png"));
        assert_eq!(e.description.as_deref(), Some("Sous-titre & résumé"));
    }

    #[test]
    fn une_page_html_n_est_pas_un_flux() {
        let html =
            "<!DOCTYPE html>\n<html><head><title>Accueil</title></head><body>rss</body></html>";
        assert_eq!(lire_l_entete(html), Err(EchecAjoutParAdresse::PasUnFlux));
        assert_eq!(
            lire_l_entete("{\"rss\": true}"),
            Err(EchecAjoutParAdresse::PasUnFlux)
        );
        assert_eq!(lire_l_entete(""), Err(EchecAjoutParAdresse::PasUnFlux));
        assert_eq!(
            lire_l_entete("<rss version=\"2.0\"></rss>"),
            Err(EchecAjoutParAdresse::PasUnFlux),
            "un <rss> sans <channel> n'est pas un flux"
        );
    }

    #[test]
    fn la_normalisation_ramene_feed_a_http_et_refuse_les_autres_schemas() {
        let n = |s| normaliser_adresse_de_flux(s);
        assert_eq!(n("  feed://Ex.TEST/rss  ").unwrap(), "http://ex.test/rss");
        assert_eq!(n("FEED://ex.test/rss").unwrap(), "http://ex.test/rss");
        assert_eq!(
            n("feed:https://ex.test/rss").unwrap(),
            "https://ex.test/rss"
        );
        assert_eq!(n("https://ex.test/rss").unwrap(), "https://ex.test/rss");
        assert_eq!(
            n("ex.test/podcast.xml").unwrap(),
            "https://ex.test/podcast.xml"
        );
        for mauvaise in [
            "",
            "   ",
            "ftp://ex.test/rss",
            "file:///etc/passwd",
            "feed:ftp://x",
        ] {
            assert_eq!(
                n(mauvaise),
                Err(EchecAjoutParAdresse::AdresseInvalide),
                "{mauvaise:?} doit être refusée"
            );
        }
    }
}
