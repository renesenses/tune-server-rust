//! #8077 — la fiche d'un album Qobuz ramenée à ce qu'un album LOCAL peut en
//! reprendre : édition (label, date exacte, UPC, ℗ et ©), genres, livrets,
//! récompenses, disponibilité hi-res, pochette HD, pistes et leurs crédits.
//!
//! Analyse HORS RÉSEAU : tout se lit sur la réponse `album/get` (déjà mise en
//! cache par `QobuzService::detail_album`) ou `album/search`, et se teste sur
//! du JSON. Le jugement du rapprochement et la règle « compléter seulement »
//! vivent dans [`crate::library::apports_service`], pas ici.
//!
//! Champs lus (forme publique de l'API `album/get`) : `upc`, `label.name`,
//! `release_date_original`, `copyright`, `genre.name`, `genres_list`,
//! `goodies` (voir [`super::qobuz::livrets_qobuz`]), `awards`, `hires`,
//! `maximum_bit_depth`, `maximum_sampling_rate`, `image.large`,
//! `tracks.items[]` (`id`, `media_number`, `track_number`, `duration`,
//! `isrc`, `title`, `performers`, `composer.name`).

use serde_json::Value;

use super::qobuz::{code_barres_normalise, id_qobuz, livrets_qobuz};
use super::qobuz_credits::lignes_performers;
use super::traits::{CandidatAlbum, FicheAlbumService, PisteDeService, Recompense};
use crate::metadata::credits_mb::LigneCredit;

/// Le côté que Qobuz garantit pour `image.large` (suffixe `_600`).
pub const COTE_GARANTI_QOBUZ: u32 = 600;

fn texte(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Les albums d'une réponse `album/search`, sans doublon d'identifiant.
pub(crate) fn candidats_de_la_recherche(data: &Value) -> Vec<CandidatAlbum> {
    let mut vus = std::collections::HashSet::new();
    data["albums"]["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|a| {
            Some(CandidatAlbum {
                id: id_qobuz(&a["id"])?,
                titre: texte(&a["title"])?,
                artiste: texte(&a["artist"]["name"]).unwrap_or_default(),
                nombre_pistes: a["tracks_count"].as_u64().unwrap_or(0) as u32,
                upc: a["upc"].as_str().and_then(code_barres_normalise),
            })
        })
        .filter(|c| vus.insert(c.id.clone()))
        .collect()
}

/// Une date `AAAA-MM-JJ` valide, sinon rien.
fn date_exacte(brut: &str) -> Option<String> {
    let d = brut.trim().get(..10)?;
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .ok()
        .map(|_| d.to_string())
}

/// Sépare la mention de droits de Qobuz en ℗ et ©.
///
/// Qobuz rend une seule chaîne : « ℗ 2015 Warner Classics © 2015 Warner
/// Classics », « (P) 1959 Columbia (C) 2015 Sony », ou l'une des deux seule.
/// Le texte qui suit chaque symbole, jusqu'au symbole suivant, est sa mention.
/// Sans aucun symbole, la chaîne entière est prise pour le © (la mention
/// générale) — jamais pour le ℗, qui ne se devine pas.
pub fn mentions_de_droits(brut: &str) -> (Option<String>, Option<String>) {
    let s = brut
        .replace("(P)", "℗")
        .replace("(p)", "℗")
        .replace("(C)", "©")
        .replace("(c)", "©");
    let mut p = None;
    let mut c = None;
    let mut courant: Option<char> = None;
    let mut tampon = String::new();
    let mut ranger = |marque: Option<char>, t: &str| {
        let t = t.trim().trim_end_matches([',', ';']).trim();
        if t.is_empty() {
            return;
        }
        match marque {
            Some('℗') => p = p.take().or(Some(t.to_string())),
            Some('©') => c = c.take().or(Some(t.to_string())),
            _ => {}
        }
    };
    for ch in s.chars() {
        if ch == '℗' || ch == '©' {
            ranger(courant, &tampon);
            tampon.clear();
            courant = Some(ch);
        } else {
            tampon.push(ch);
        }
    }
    if courant.is_none() {
        let t = tampon.trim();
        return (None, (!t.is_empty()).then(|| t.to_string()));
    }
    ranger(courant, &tampon);
    (p, c)
}

/// Les genres de l'album : le dernier maillon de chaque chemin de
/// `genres_list` (« Jazz→Cool Jazz » donne « Cool Jazz »), sinon `genre.name`.
fn genres(data: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in data["genres_list"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(nom) = g
            .as_str()
            .and_then(|s| s.rsplit('→').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && !out.iter().any(|o| o.eq_ignore_ascii_case(nom))
        {
            out.push(nom.to_string());
        }
    }
    if out.is_empty()
        && let Some(nom) = texte(&data["genre"]["name"])
    {
        out.push(nom);
    }
    out
}

fn date_d_epoque(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

fn recompenses(data: &Value) -> Vec<Recompense> {
    data["awards"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|a| {
            Some(Recompense {
                nom: texte(&a["name"])?,
                publication: texte(&a["publication_name"])
                    .or_else(|| texte(&a["publication"]["name"])),
                date: a["awarded_at"].as_i64().and_then(date_d_epoque),
            })
        })
        .collect()
}

/// Profondeur et fréquence (Hz) quand l'album est disponible en haute
/// résolution : `hires` vrai, ou plus de 16 bits, ou plus de 48 kHz.
fn hires(data: &Value) -> Option<(u32, u32)> {
    let bits = data["maximum_bit_depth"].as_u64().unwrap_or(0) as u32;
    let hz = data["maximum_sampling_rate"]
        .as_f64()
        .map(|k| (k * 1000.0).round() as u32)
        .unwrap_or(0);
    let annonce =
        data["hires"].as_bool() == Some(true) || data["hires_streamable"].as_bool() == Some(true);
    (annonce || bits > 16 || hz > 48_000).then_some((bits, hz))
}

/// La pochette la plus grande : `image.large` (`…_600.jpg`) devient
/// `…_max.jpg`, que Qobuz sert à la taille d'origine. Seulement en `https`,
/// chez Qobuz. Le côté rendu est celui que Qobuz garantit (600) : la taille
/// réelle de `_max` n'est connue qu'au téléchargement.
fn pochette_hd(data: &Value) -> Option<(String, u32)> {
    let large = texte(&data["image"]["large"])?;
    let u = reqwest::Url::parse(&large).ok()?;
    let hote_qobuz = u
        .host_str()
        .is_some_and(|h| h == "qobuz.com" || h.ends_with(".qobuz.com"));
    if u.scheme() != "https" || !hote_qobuz {
        return None;
    }
    let hd = match large.rsplit_once("_600.") {
        Some((tete, ext)) => format!("{tete}_max.{ext}"),
        None => large,
    };
    Some((hd, COTE_GARANTI_QOBUZ))
}

/// Les crédits d'une piste : la chaîne `performers` (#4993), et le
/// compositeur de `composer.name` quand la chaîne n'en nomme aucun.
fn credits_de_la_piste(item: &Value) -> Vec<LigneCredit> {
    let mut lignes = lignes_performers(item["performers"].as_str().unwrap_or(""));
    if !lignes.iter().any(|l| l.role == "composer")
        && let Some(nom) = texte(&item["composer"]["name"])
    {
        lignes.push(LigneCredit {
            artist_name: nom,
            role: "composer".into(),
            instrument: None,
            artist_mbid: None,
        });
    }
    lignes
}

fn pistes(data: &Value) -> Vec<PisteDeService> {
    data["tracks"]["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|t| {
            Some(PisteDeService {
                id: id_qobuz(&t["id"])?,
                disque: t["media_number"].as_i64().unwrap_or(1),
                numero: t["track_number"].as_i64()?,
                duree_ms: t["duration"].as_i64().unwrap_or(0) * 1000,
                isrc: texte(&t["isrc"]).map(|i| i.to_ascii_uppercase()),
                titre: texte(&t["title"]).unwrap_or_default(),
                credits: credits_de_la_piste(t),
            })
        })
        .collect()
}

/// La fiche d'un album depuis une réponse `album/get`. Sans identifiant :
/// rien.
pub(crate) fn fiche_de_l_album(data: &Value) -> Option<FicheAlbumService> {
    let (phonogramme, copyright) = data["copyright"]
        .as_str()
        .map(mentions_de_droits)
        .unwrap_or((None, None));
    Some(FicheAlbumService {
        id: id_qobuz(&data["id"])?,
        titre: texte(&data["title"]).unwrap_or_default(),
        artiste: texte(&data["artist"]["name"]).unwrap_or_default(),
        upc: data["upc"].as_str().and_then(code_barres_normalise),
        label: texte(&data["label"]["name"]),
        date_de_sortie: data["release_date_original"].as_str().and_then(date_exacte),
        phonogramme,
        copyright,
        genres: genres(data),
        livrets: livrets_qobuz(data),
        recompenses: recompenses(data),
        hires: hires(data),
        pochette_hd: pochette_hd(data),
        pistes: pistes(data),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn les_mentions_de_droits_se_separent() {
        assert_eq!(
            mentions_de_droits("℗ 2015 Warner Classics © 2015 Warner Music"),
            (
                Some("2015 Warner Classics".into()),
                Some("2015 Warner Music".into())
            )
        );
        assert_eq!(
            mentions_de_droits("(C) 2015 Sony (P) 1959 Columbia"),
            (Some("1959 Columbia".into()), Some("2015 Sony".into()))
        );
        assert_eq!(
            mentions_de_droits("2001 Label Factice"),
            (None, Some("2001 Label Factice".into()))
        );
        assert_eq!(mentions_de_droits("  "), (None, None));
    }

    #[test]
    fn la_fiche_lit_tous_les_champs() {
        let data = json!({
            "id": "abc123", "title": "Disque", "artist": {"name": "Groupe"},
            "upc": "0123456789012", "label": {"name": "Label Factice"},
            "release_date_original": "2015-03-02",
            "copyright": "℗ 2015 Label © 2015 Label",
            "genres_list": ["Jazz", "Jazz→Cool Jazz"],
            "awards": [{"name": "Diapason d'or", "publication_name": "Diapason", "awarded_at": 1425254400}],
            "hires": true, "maximum_bit_depth": 24, "maximum_sampling_rate": 96.0,
            "image": {"large": "https://static.qobuz.com/images/covers/ab/cd/abc_600.jpg"},
            "tracks": {"items": [{
                "id": 9, "media_number": 1, "track_number": 1, "duration": 200,
                "isrc": "xxa0d2600001", "title": "Un",
                "performers": "Jean Factice, Piano, MainArtist - Paul Factice, Producer",
                "composer": {"name": "Compositeur Factice"}
            }]}
        });
        let f = fiche_de_l_album(&data).expect("fiche");
        assert_eq!(f.upc.as_deref(), Some("123456789012"));
        assert_eq!(f.label.as_deref(), Some("Label Factice"));
        assert_eq!(f.date_de_sortie.as_deref(), Some("2015-03-02"));
        assert_eq!(f.phonogramme.as_deref(), Some("2015 Label"));
        assert_eq!(f.genres, vec!["Jazz".to_string(), "Cool Jazz".to_string()]);
        assert_eq!(f.recompenses[0].date.as_deref(), Some("2015-03-02"));
        assert_eq!(f.hires, Some((24, 96_000)));
        assert_eq!(
            f.pochette_hd,
            Some((
                "https://static.qobuz.com/images/covers/ab/cd/abc_max.jpg".into(),
                600
            ))
        );
        let p = &f.pistes[0];
        assert_eq!((p.disque, p.numero, p.duree_ms), (1, 1, 200_000));
        assert_eq!(p.isrc.as_deref(), Some("XXA0D2600001"));
        assert!(
            p.credits
                .iter()
                .any(|l| l.role == "producer" && l.artist_name == "Paul Factice")
        );
        assert!(
            p.credits
                .iter()
                .any(|l| l.role == "composer" && l.artist_name == "Compositeur Factice")
        );
    }

    #[test]
    fn une_pochette_hors_de_qobuz_n_est_pas_proposee() {
        let data = json!({"id": 1, "image": {"large": "http://exemple.invalid/a_600.jpg"}});
        assert_eq!(fiche_de_l_album(&data).unwrap().pochette_hd, None);
    }

    #[test]
    fn un_album_cd_n_est_pas_hires() {
        let data = json!({"id": 1, "maximum_bit_depth": 16, "maximum_sampling_rate": 44.1});
        assert_eq!(fiche_de_l_album(&data).unwrap().hires, None);
    }
}
