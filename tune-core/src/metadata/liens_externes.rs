//! Liens sortants « Voir sur ProgArchives / Metal Archives / AllMusic » d'un
//! artiste ou d'un album (#6001).
//!
//! # La décision
//!
//! Bertrand, 10/10/2026 : **des liens seulement**, sans recopier les contenus
//! de ces trois sites. Aucun d'eux n'autorise une reprise de ses données dans
//! un logiciel commercial (ProgArchives `Legalnotice.asp`, conditions
//! d'AllMusic clause 9.1, aucun texte chez Metal Archives) : on ne lit jamais
//! leurs pages, on ne fait que pointer vers elles.
//!
//! # D'où viennent les adresses
//!
//! Des relations d'URL de MusicBrainz (`inc=url-rels`) de l'artiste ou du
//! groupe de parutions : MusicBrainz range ProgArchives et Metal Archives sous
//! « other databases » et AllMusic sous son type propre. Ces relations sont des
//! données de base de MusicBrainz, sous CC0. On ne garde que les adresses
//! dont l'hôte est l'un des trois sites, réécrites en `https`.
//!
//! # Transport
//!
//! Injecté (`lire`), comme `wikidata::instruments_de_l_artiste_par` : le chemin
//! relatif à `/ws/2` (`artist/{mbid}`), et la réponse `Ok(Some)`, `Ok(None)`
//! (MusicBrainz ne connaît pas l'identifiant) ou `Err` (pas de réponse). Le
//! transport de production, [`lire_sur_musicbrainz_avec_creneau`], attend le
//! créneau du limiteur MusicBrainz PARTAGÉ avant chaque requête.

use std::future::Future;

use serde::Serialize;
use serde_json::Value;

pub use super::musicbrainz_release::SansReponse;

/// L'objet de la bibliothèque dont on cherche les liens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entite {
    Artiste,
    /// Un album, par son groupe de parutions MusicBrainz.
    GroupeDeParutions,
}

impl Entite {
    fn chemin(self) -> &'static str {
        match self {
            Entite::Artiste => "artist",
            Entite::GroupeDeParutions => "release-group",
        }
    }
}

/// Un lien sortant vers l'un des trois sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LienExterne {
    /// Identifiant stable du site, pour l'interface : `progarchives`,
    /// `metal-archives`, `allmusic`.
    pub site: &'static str,
    /// Le nom du site tel qu'il s'affiche (un nom propre, non traduit).
    pub nom: &'static str,
    pub url: String,
}

/// `(identifiant, nom affiché, hôtes acceptés)`, dans l'ordre d'affichage.
const SITES: [(&str, &str, &[&str]); 3] = [
    (
        "progarchives",
        "ProgArchives",
        &["progarchives.com", "www.progarchives.com"],
    ),
    (
        "metal-archives",
        "Metal Archives",
        &["metal-archives.com", "www.metal-archives.com"],
    ),
    (
        "allmusic",
        "AllMusic",
        &["allmusic.com", "www.allmusic.com"],
    ),
];

/// Les liens vers les trois sites portés par une réponse MusicBrainz
/// `…?inc=url-rels`. Pur : aucune requête. Une adresse d'un autre hôte, mal
/// formée ou hors `http(s)`, est écartée ; une adresse en double n'apparaît
/// qu'une fois.
pub fn liens_depuis_relations(entite: &Value) -> Vec<LienExterne> {
    let mut liens: Vec<LienExterne> = Vec::new();
    let Some(relations) = entite["relations"].as_array() else {
        return liens;
    };
    for r in relations {
        let Some(brut) = r["url"]["resource"].as_str() else {
            continue;
        };
        let Ok(mut url) = reqwest::Url::parse(brut.trim()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let Some(hote) = url.host_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        let Some((site, nom, _)) = SITES.iter().find(|(_, _, h)| h.contains(&hote.as_str())) else {
            continue;
        };
        if url.set_scheme("https").is_err() {
            continue;
        }
        let lien = LienExterne {
            site,
            nom,
            url: url.to_string(),
        };
        if !liens.contains(&lien) {
            liens.push(lien);
        }
    }
    let rang = |site: &str| SITES.iter().position(|(s, _, _)| *s == site);
    liens.sort_by_key(|l| rang(l.site));
    liens
}

/// Un identifiant MusicBrainz plausible : 36 caractères, hexadécimal et
/// tirets. Garde le chemin de la requête à l'abri d'une valeur de la base
/// qui contiendrait `/` ou `?`.
pub fn mbid_plausible(mbid: &str) -> bool {
    mbid.len() == 36 && mbid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Ce que la recherche des liens a donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// MusicBrainz a répondu : les liens (éventuellement aucun).
    Liens(Vec<LienExterne>),
    /// L'identifiant est mal formé ou inconnu de MusicBrainz.
    Inconnu,
    /// MusicBrainz n'a pas répondu : rien à conclure, rien à garder.
    Indisponible,
}

/// Les liens d'un artiste ou d'un groupe de parutions, par le transport
/// injecté `lire(chemin, inc)`.
pub async fn liens_par<F, Fut>(entite: Entite, mbid: &str, lire: &F) -> Issue
where
    F: Fn(String, &'static str) -> Fut,
    Fut: Future<Output = Result<Option<Value>, SansReponse>>,
{
    let mbid = mbid.trim();
    if !mbid_plausible(mbid) {
        return Issue::Inconnu;
    }
    match lire(format!("{}/{mbid}", entite.chemin()), "url-rels").await {
        Ok(Some(v)) => Issue::Liens(liens_depuis_relations(&v)),
        Ok(None) => Issue::Inconnu,
        Err(SansReponse) => Issue::Indisponible,
    }
}

/// Le transport de production : attend le créneau du limiteur MusicBrainz
/// partagé, puis lit `/{chemin}?inc=…`.
pub async fn lire_sur_musicbrainz_avec_creneau(
    chemin: String,
    inc: &'static str,
) -> Result<Option<Value>, SansReponse> {
    super::musicbrainz_release::rate_limit_delay().await;
    super::musicbrainz_release::lire_sur_musicbrainz(chemin, inc)
        .await
        .map_err(|_| SansReponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn relation(url: &str, kind: &str) -> Value {
        json!({"type": kind, "target-type": "url", "url": {"resource": url}})
    }

    /// Forme d'une réponse `artist/{mbid}?inc=url-rels&fmt=json` : les trois
    /// sites, plus des voisins à écarter.
    fn reponse_artiste() -> Value {
        json!({
            "id": "83b9cbe7-9857-49e2-ab8e-b57b01038103",
            "name": "Pink Floyd",
            "relations": [
                relation("https://www.wikidata.org/wiki/Q2306", "wikidata"),
                relation("https://www.allmusic.com/artist/mn0000346336", "allmusic"),
                relation("http://www.progarchives.com/artist.asp?id=78", "other databases"),
                relation("https://www.metal-archives.com/bands/Pink_Floyd/3540344064", "other databases"),
                relation("https://www.discogs.com/artist/45467", "discogs"),
                relation("https://www.progarchives.com/artist.asp?id=78", "other databases"),
                relation("javascript:alert(1)", "other databases"),
                relation("https://progarchives.com.example.org/x", "other databases"),
            ]
        })
    }

    #[test]
    fn ne_garde_que_les_trois_sites_en_https_dans_l_ordre_6001() {
        let liens = liens_depuis_relations(&reponse_artiste());
        assert_eq!(
            liens,
            vec![
                LienExterne {
                    site: "progarchives",
                    nom: "ProgArchives",
                    url: "https://www.progarchives.com/artist.asp?id=78".into(),
                },
                LienExterne {
                    site: "metal-archives",
                    nom: "Metal Archives",
                    url: "https://www.metal-archives.com/bands/Pink_Floyd/3540344064".into(),
                },
                LienExterne {
                    site: "allmusic",
                    nom: "AllMusic",
                    url: "https://www.allmusic.com/artist/mn0000346336".into(),
                },
            ]
        );
    }

    #[test]
    fn sans_relation_aucun_lien_6001() {
        assert!(liens_depuis_relations(&json!({"id": "x"})).is_empty());
        assert!(liens_depuis_relations(&json!({"relations": []})).is_empty());
    }

    #[tokio::test]
    async fn le_chemin_demande_suit_l_entite_et_l_issue_suit_la_reponse_6001() {
        let demandes: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let lire = |chemin: String, inc: &'static str| {
            demandes.lock().unwrap().push(format!("{chemin}?inc={inc}"));
            let r = if chemin.starts_with("artist/") {
                Ok(Some(reponse_artiste()))
            } else if chemin.ends_with("0000") {
                Err(SansReponse)
            } else {
                Ok(None)
            };
            std::future::ready(r)
        };
        let artiste = "83b9cbe7-9857-49e2-ab8e-b57b01038103";
        let Issue::Liens(l) = liens_par(Entite::Artiste, artiste, &lire).await else {
            panic!("des liens étaient attendus");
        };
        assert_eq!(l.len(), 3);
        assert_eq!(
            liens_par(
                Entite::GroupeDeParutions,
                "a1b2c3d4-0000-0000-0000-000000000000",
                &lire
            )
            .await,
            Issue::Indisponible
        );
        assert_eq!(
            liens_par(
                Entite::GroupeDeParutions,
                "a1b2c3d4-0000-0000-0000-00000000000f",
                &lire
            )
            .await,
            Issue::Inconnu
        );
        // Un identifiant forgé ne part jamais en requête.
        assert_eq!(
            liens_par(Entite::Artiste, "../release/x?inc=", &lire).await,
            Issue::Inconnu
        );
        assert_eq!(
            *demandes.lock().unwrap(),
            vec![
                format!("artist/{artiste}?inc=url-rels"),
                "release-group/a1b2c3d4-0000-0000-0000-000000000000?inc=url-rels".to_string(),
                "release-group/a1b2c3d4-0000-0000-0000-00000000000f?inc=url-rels".to_string(),
            ]
        );
    }
}
