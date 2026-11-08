//! #8077 — compléter un album LOCAL depuis l'album Qobuz qui lui correspond,
//! sans jamais écraser une donnée locale.
//!
//! Ce module ne lit ni n'écrit la base, et n'appelle aucun service : il juge
//! un rapprochement, apparie les pistes et calcule les apports, sur des
//! valeurs. L'orchestration (lecture de l'album, appels Qobuz sous le
//! limiteur, rangement) est dans `tune-server/src/routes/library/apports_qobuz.rs`,
//! le stockage dans [`crate::db::apports_service`].
//!
//! # Rapprochement de l'album
//!
//! 1. **Codes** : code-barres (UPC/EAN) de l'album, sinon celui de la release
//!    MusicBrainz gardée en base pour son MBID, sinon les ISRC de ses titres
//!    (un seul album Qobuz doit les porter tous) — voir
//!    [`crate::streaming::StreamingService::album_par_codes`]. Sûr.
//! 2. **Titre** : artiste et titre normalisés ([`cle_artiste`]) ÉGAUX, et même
//!    nombre de pistes, pour UN SEUL album Qobuz ([`juger_par_titre`]) ; puis
//!    TOUTES les pistes locales doivent se rapprocher ([`statut_apres_pistes`]).
//!
//! Tout le reste est **douteux** : rien n'est appliqué, le rapprochement est
//! rangé `a_confirmer` avec son motif, et l'utilisateur peut le confirmer.
//!
//! # Rapprochement des pistes
//!
//! Disque et numéro, à durée égale à [`tolerance_ms`] près ; sinon l'ISRC
//! ([`rapprocher_pistes`]). Une piste Qobuz ne sert qu'une fois.
//!
//! # Compléter seulement
//!
//! Chaque champ passe par la MÊME garde, [`completer`] : présent en local, il
//! n'est pas repris. Les crédits ne complètent que les rôles qu'aucune autre
//! source ne porte pour la piste.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::db::artist_repo::cle_artiste;
use crate::metadata::credits_mb::LigneCredit;
use crate::streaming::traits::{CandidatAlbum, FicheAlbumService, PisteDeService};

/// La source posée sur chaque apport, et sur les crédits repris.
pub const SOURCE: &str = crate::metadata::source_des_credits::QOBUZ;

/// Écart de durée toléré au moins entre une piste locale et sa piste Qobuz.
pub const TOLERANCE_MIN_MS: i64 = 3_000;

/// Les champs d'album qu'un apport peut porter (`apports_service.champ`).
pub mod champ {
    pub const LABEL: &str = "label";
    pub const DATE_DE_SORTIE: &str = "date_de_sortie";
    pub const UPC: &str = "upc";
    pub const PHONOGRAMME: &str = "phonogramme";
    pub const COPYRIGHT: &str = "copyright";
    pub const GENRES: &str = "genres";
    pub const LIVRETS: &str = "livrets";
    pub const RECOMPENSES: &str = "recompenses";
    pub const HIRES: &str = "hires";
    pub const POCHETTE_HD: &str = "pochette_hd";
    /// Champ de PISTE.
    pub const ISRC: &str = "isrc";
}

/// Comment l'album Qobuz a été retrouvé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Methode {
    Upc,
    Mbid,
    Isrc,
    Titre,
    /// Un rapprochement douteux confirmé par l'utilisateur.
    Confirme,
}

impl Methode {
    pub fn as_str(self) -> &'static str {
        match self {
            Methode::Upc => "upc",
            Methode::Mbid => "mbid",
            Methode::Isrc => "isrc",
            Methode::Titre => "titre",
            Methode::Confirme => "confirme",
        }
    }
}

/// L'état d'un rapprochement (`apports_service_albums.statut`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statut {
    /// Sûr : les apports sont rangés.
    Applique,
    /// Douteux : rien n'est appliqué tant que l'utilisateur ne confirme pas.
    AConfirmer,
    /// Aucun album Qobuz retrouvé.
    Aucun,
    /// Apports retirés par l'utilisateur : la passe de fond ne les remet pas.
    Retire,
}

impl Statut {
    pub fn as_str(self) -> &'static str {
        match self {
            Statut::Applique => "applique",
            Statut::AConfirmer => "a_confirmer",
            Statut::Aucun => "aucun",
            Statut::Retire => "retire",
        }
    }

    pub fn depuis(s: &str) -> Option<Self> {
        Some(match s {
            "applique" => Statut::Applique,
            "a_confirmer" => Statut::AConfirmer,
            "aucun" => Statut::Aucun,
            "retire" => Statut::Retire,
            _ => return None,
        })
    }
}

/// Ce que la bibliothèque sait d'un album pour le juger par son titre.
#[derive(Debug, Clone, Default)]
pub struct AlbumLocal {
    pub titre: String,
    pub artiste: String,
    pub nombre_pistes: u32,
    pub est_compilation: bool,
}

/// Le jugement d'une recherche par titre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jugement {
    /// Un seul album, même artiste, même titre, même nombre de pistes.
    Sur(String),
    /// Un candidat plausible, pas sûr : `motif` dit pourquoi.
    Douteux {
        id: String,
        motif: &'static str,
    },
    Aucun,
}

fn est_artistes_divers(cle: &str) -> bool {
    cle.is_empty() || cle == "various artists" || cle == "various" || cle == "artistes divers"
}

/// Juge les candidats d'une recherche « artiste titre ».
pub fn juger_par_titre(local: &AlbumLocal, candidats: &[CandidatAlbum]) -> Jugement {
    let (artiste, titre) = (cle_artiste(&local.artiste), cle_artiste(&local.titre));
    if titre.is_empty() {
        return Jugement::Aucun;
    }
    let meme_artiste = |c: &&CandidatAlbum| cle_artiste(&c.artiste) == artiste;
    let exacts: Vec<&CandidatAlbum> = candidats
        .iter()
        .filter(meme_artiste)
        .filter(|c| cle_artiste(&c.titre) == titre)
        .collect();
    let premier_douteux = |motif: &'static str| {
        let proche = candidats.iter().filter(meme_artiste).find(|c| {
            let t = cle_artiste(&c.titre);
            !t.is_empty() && (t.starts_with(&titre) || titre.starts_with(&t))
        });
        match exacts.first().copied().or(proche) {
            Some(c) => Jugement::Douteux {
                id: c.id.clone(),
                motif,
            },
            None => Jugement::Aucun,
        }
    };
    // Une compilation, ou un artiste inconnu, ne désigne pas un disque : le
    // titre seul (« Greatest Hits ») ne suffit jamais.
    if local.est_compilation || est_artistes_divers(&artiste) {
        return premier_douteux("compilation");
    }
    let meme_nombre: Vec<&&CandidatAlbum> = exacts
        .iter()
        .filter(|c| c.nombre_pistes == local.nombre_pistes)
        .collect();
    match meme_nombre.len() {
        1 => Jugement::Sur(meme_nombre[0].id.clone()),
        0 if exacts.is_empty() => premier_douteux("titre_approche"),
        0 => premier_douteux("nombre_de_pistes"),
        _ => premier_douteux("plusieurs_editions"),
    }
}

/// Une piste locale, pour l'appariement.
#[derive(Debug, Clone, Default)]
pub struct PisteLocale {
    pub id: i64,
    pub disque: i64,
    pub numero: i64,
    pub duree_ms: i64,
    pub isrc: Option<String>,
}

/// L'écart de durée toléré pour une piste : 3 s, ou 2 % de sa durée.
pub fn tolerance_ms(duree_ms: i64) -> i64 {
    TOLERANCE_MIN_MS.max(duree_ms / 50)
}

fn isrc_normalise(i: &str) -> String {
    i.trim().replace('-', "").to_ascii_uppercase()
}

/// Couples `(id de piste locale, rang dans service)` : disque + numéro à
/// durée tolérée d'abord, l'ISRC ensuite. Une durée locale inconnue (0) ne
/// rapproche que par ISRC.
pub fn rapprocher_pistes(locales: &[PisteLocale], service: &[PisteDeService]) -> Vec<(i64, usize)> {
    let mut prises = vec![false; service.len()];
    let mut out: Vec<(i64, usize)> = Vec::new();
    let mut restantes: Vec<&PisteLocale> = Vec::new();
    for l in locales {
        let trouve = (l.duree_ms > 0)
            .then(|| {
                service.iter().enumerate().position(|(i, s)| {
                    !prises[i]
                        && s.disque == l.disque.max(1)
                        && s.numero == l.numero
                        && s.duree_ms > 0
                        && (s.duree_ms - l.duree_ms).abs() <= tolerance_ms(l.duree_ms)
                })
            })
            .flatten();
        match trouve {
            Some(i) => {
                prises[i] = true;
                out.push((l.id, i));
            }
            None => restantes.push(l),
        }
    }
    for l in restantes {
        let Some(isrc) = l
            .isrc
            .as_deref()
            .map(isrc_normalise)
            .filter(|i| !i.is_empty())
        else {
            continue;
        };
        if let Some(i) = service.iter().enumerate().position(|(i, s)| {
            !prises[i] && s.isrc.as_deref().map(isrc_normalise).as_deref() == Some(isrc.as_str())
        }) {
            prises[i] = true;
            out.push((l.id, i));
        }
    }
    out
}

/// Le statut d'un rapprochement une fois les pistes appariées.
///
/// Par code (UPC, MBID, ISRC) ou confirmé : sûr, quelles que soient les
/// pistes (seules les pistes appariées reçoivent leurs apports). Par titre :
/// sûr seulement si TOUTES les pistes locales sont appariées.
pub fn statut_apres_pistes(methode: Methode, locales: usize, rapprochees: usize) -> Statut {
    match methode {
        Methode::Titre if locales == 0 || rapprochees < locales => Statut::AConfirmer,
        _ => Statut::Applique,
    }
}

/// Ce que la bibliothèque sait déjà de l'album.
#[derive(Debug, Clone, Default)]
pub struct EtatLocalAlbum {
    pub label: Option<String>,
    pub date_de_sortie: Option<String>,
    pub annee: Option<i32>,
    pub upc: Option<String>,
    /// Une piste porte une mention de droits (`track_metadata.copyright`).
    pub copyright: bool,
    pub genres: bool,
    /// Un livret PDF est dans le dossier de l'album.
    pub livret: bool,
    pub pochette: PochetteLocale,
}

/// La pochette locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PochetteLocale {
    #[default]
    Absente,
    /// Le plus petit côté de l'image, en pixels.
    Cote(u32),
    /// Une pochette existe, de taille illisible : elle n'est pas remplacée.
    Inconnue,
}

/// Ce que la bibliothèque sait déjà d'une piste.
#[derive(Debug, Clone, Default)]
pub struct EtatLocalPiste {
    pub id: i64,
    pub isrc: Option<String>,
    /// Les rôles déjà crédités par une autre source que Qobuz (balises,
    /// MusicBrainz, Roon, saisie…), `composer` compris si la piste a un
    /// compositeur.
    pub roles: BTreeSet<String>,
}

/// Un champ repris : pour l'album (`track_id = 0`) ou pour une piste.
#[derive(Debug, Clone, PartialEq)]
pub struct Apport {
    pub track_id: i64,
    pub champ: &'static str,
    pub valeur: Value,
}

/// Tout ce qu'un rapprochement sûr apporte.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Apports {
    pub champs: Vec<Apport>,
    /// Crédits à écrire, par piste locale, avec `source = 'qobuz'`.
    pub credits: Vec<(i64, Vec<LigneCredit>)>,
}

fn texte_present(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// LA garde « compléter seulement » : une donnée présente en local n'est
/// jamais reprise. Tous les champs et tous les crédits passent par elle.
pub fn completer<T>(present_en_local: bool, apport: Option<T>) -> Option<T> {
    if present_en_local { None } else { apport }
}

/// Les apports d'un album rapproché SÛREMENT, champ par champ.
pub fn calculer(
    local: &EtatLocalAlbum,
    pistes_locales: &[EtatLocalPiste],
    fiche: &FicheAlbumService,
    paires: &[(i64, usize)],
) -> Apports {
    let mut champs: Vec<Apport> = Vec::new();
    let mut pousser = |champ: &'static str, valeur: Option<Value>| {
        if let Some(valeur) = valeur {
            champs.push(Apport {
                track_id: 0,
                champ,
                valeur,
            });
        }
    };
    pousser(
        champ::LABEL,
        completer(
            texte_present(&local.label),
            fiche.label.clone().map(Value::from),
        ),
    );
    // La date exacte complète une année seule, jamais une année différente :
    // Qobuz date souvent la RÉÉDITION, et 2015 ne précise pas 1959.
    let annee_qobuz = fiche
        .date_de_sortie
        .as_deref()
        .and_then(|d| d.get(..4)?.parse::<i32>().ok());
    let date_locale_exacte = local.date_de_sortie.as_deref().is_some_and(|d| {
        chrono::NaiveDate::parse_from_str(d.get(..10).unwrap_or(""), "%Y-%m-%d").is_ok()
    });
    let annee_differente = matches!((local.annee, annee_qobuz), (Some(a), Some(b)) if a != b);
    pousser(
        champ::DATE_DE_SORTIE,
        completer(
            date_locale_exacte || annee_differente,
            fiche.date_de_sortie.clone().map(Value::from),
        ),
    );
    pousser(
        champ::UPC,
        completer(
            texte_present(&local.upc),
            fiche.upc.clone().map(Value::from),
        ),
    );
    pousser(
        champ::PHONOGRAMME,
        completer(local.copyright, fiche.phonogramme.clone().map(Value::from)),
    );
    pousser(
        champ::COPYRIGHT,
        completer(local.copyright, fiche.copyright.clone().map(Value::from)),
    );
    pousser(
        champ::GENRES,
        completer(
            local.genres,
            (!fiche.genres.is_empty()).then(|| json!(fiche.genres)),
        ),
    );
    pousser(
        champ::LIVRETS,
        completer(
            local.livret,
            (!fiche.livrets.is_empty()).then(|| json!(fiche.livrets)),
        ),
    );
    // Récompenses et hi-res n'ont pas d'équivalent local : rien à écraser.
    pousser(
        champ::RECOMPENSES,
        completer(
            false,
            (!fiche.recompenses.is_empty()).then(|| json!(fiche.recompenses)),
        ),
    );
    pousser(
        champ::HIRES,
        completer(
            false,
            fiche
                .hires
                .map(|(bits, hz)| json!({"bit_depth": bits, "sample_rate": hz})),
        ),
    );
    let pochette_suffisante = match local.pochette {
        PochetteLocale::Absente => false,
        PochetteLocale::Inconnue => true,
        PochetteLocale::Cote(c) => fiche.pochette_hd.as_ref().is_none_or(|(_, q)| c >= *q),
    };
    pousser(
        champ::POCHETTE_HD,
        completer(
            pochette_suffisante,
            fiche
                .pochette_hd
                .as_ref()
                .map(|(url, cote)| json!({"url": url, "cote_min": cote})),
        ),
    );

    let mut credits = Vec::new();
    for (track_id, rang) in paires {
        let (Some(piste), Some(locale)) = (
            fiche.pistes.get(*rang),
            pistes_locales.iter().find(|p| p.id == *track_id),
        ) else {
            continue;
        };
        if let Some(isrc) = completer(texte_present(&locale.isrc), piste.isrc.clone()) {
            champs.push(Apport {
                track_id: *track_id,
                champ: champ::ISRC,
                valeur: Value::from(isrc),
            });
        }
        // `artist` (MainArtist) : la piste locale a toujours son artiste.
        let lignes: Vec<LigneCredit> = piste
            .credits
            .iter()
            .filter(|l| l.role != "artist")
            .filter_map(|l| completer(locale.roles.contains(&l.role), Some(l.clone())))
            .collect();
        if !lignes.is_empty() {
            credits.push((*track_id, lignes));
        }
    }
    Apports { champs, credits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::traits::StreamBooklet;

    fn candidat(id: &str, artiste: &str, titre: &str, n: u32) -> CandidatAlbum {
        CandidatAlbum {
            id: id.into(),
            titre: titre.into(),
            artiste: artiste.into(),
            nombre_pistes: n,
            upc: None,
        }
    }

    fn local(artiste: &str, titre: &str, n: u32) -> AlbumLocal {
        AlbumLocal {
            titre: titre.into(),
            artiste: artiste.into(),
            nombre_pistes: n,
            est_compilation: false,
        }
    }

    #[test]
    fn un_seul_album_meme_artiste_meme_titre_meme_nombre_est_sur() {
        let c = [
            candidat("a", "The Factices", "Disque Factice", 10),
            candidat("b", "Autre", "Disque Factice", 10),
        ];
        assert_eq!(
            juger_par_titre(&local("Factices", "Disque factice", 10), &c),
            Jugement::Sur("a".into())
        );
    }

    #[test]
    fn deux_editions_ou_un_autre_nombre_sont_douteux() {
        let c = [
            candidat("a", "Groupe", "Disque", 10),
            candidat("b", "Groupe", "Disque", 10),
        ];
        assert!(matches!(
            juger_par_titre(&local("Groupe", "Disque", 10), &c),
            Jugement::Douteux {
                motif: "plusieurs_editions",
                ..
            }
        ));
        let c = [candidat("a", "Groupe", "Disque", 12)];
        assert!(matches!(
            juger_par_titre(&local("Groupe", "Disque", 10), &c),
            Jugement::Douteux {
                motif: "nombre_de_pistes",
                ..
            }
        ));
        let c = [candidat("a", "Groupe", "Disque (Deluxe Edition)", 10)];
        assert!(matches!(
            juger_par_titre(&local("Groupe", "Disque", 10), &c),
            Jugement::Douteux {
                motif: "titre_approche",
                ..
            }
        ));
        let mut l = local("Various Artists", "Disque", 10);
        l.est_compilation = true;
        let c = [candidat("a", "Various Artists", "Disque", 10)];
        assert!(matches!(
            juger_par_titre(&l, &c),
            Jugement::Douteux {
                motif: "compilation",
                ..
            }
        ));
        assert_eq!(
            juger_par_titre(
                &local("Groupe", "Disque", 10),
                &[candidat("z", "Autre", "Rien", 3)]
            ),
            Jugement::Aucun
        );
    }

    fn piste_s(disque: i64, numero: i64, duree_ms: i64, isrc: Option<&str>) -> PisteDeService {
        PisteDeService {
            id: format!("{disque}-{numero}"),
            disque,
            numero,
            duree_ms,
            isrc: isrc.map(str::to_string),
            titre: String::new(),
            credits: vec![],
        }
    }

    #[test]
    fn les_pistes_se_rapprochent_par_place_et_duree_sinon_par_isrc() {
        let service = [
            piste_s(1, 1, 200_000, Some("XXA0D2600001")),
            piste_s(1, 2, 300_000, Some("XXA0D2600002")),
        ];
        let locales = [
            PisteLocale {
                id: 10,
                disque: 1,
                numero: 1,
                duree_ms: 202_000,
                isrc: None,
            },
            // Durée trop éloignée à sa place, retrouvée par l'ISRC.
            PisteLocale {
                id: 11,
                disque: 1,
                numero: 2,
                duree_ms: 250_000,
                isrc: Some("xxa0d2600002".into()),
            },
            PisteLocale {
                id: 12,
                disque: 1,
                numero: 3,
                duree_ms: 100_000,
                isrc: None,
            },
        ];
        assert_eq!(
            rapprocher_pistes(&locales, &service),
            vec![(10, 0), (11, 1)]
        );
        assert_eq!(
            statut_apres_pistes(Methode::Titre, 3, 2),
            Statut::AConfirmer
        );
        assert_eq!(statut_apres_pistes(Methode::Upc, 3, 2), Statut::Applique);
    }

    #[test]
    fn la_garde_ne_reprend_rien_de_present() {
        let fiche = FicheAlbumService {
            id: "a".into(),
            label: Some("Label Qobuz".into()),
            date_de_sortie: Some("2015-03-02".into()),
            livrets: vec![StreamBooklet {
                name: "Livret".into(),
                url: "https://static.qobuz.com/a.pdf".into(),
            }],
            pochette_hd: Some(("https://static.qobuz.com/a_max.jpg".into(), 600)),
            ..Default::default()
        };
        let present = EtatLocalAlbum {
            label: Some("Label local".into()),
            annee: Some(1959),
            livret: true,
            pochette: PochetteLocale::Cote(1400),
            ..Default::default()
        };
        assert!(calculer(&present, &[], &fiche, &[]).champs.is_empty());
        let vide = EtatLocalAlbum {
            pochette: PochetteLocale::Cote(300),
            ..Default::default()
        };
        let champs: Vec<&str> = calculer(&vide, &[], &fiche, &[])
            .champs
            .iter()
            .map(|a| a.champ)
            .collect();
        assert_eq!(
            champs,
            vec!["label", "date_de_sortie", "livrets", "pochette_hd"]
        );
    }
}
