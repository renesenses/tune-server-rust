//! LA règle « cet album est-il une compilation ? » — un seul endroit.
//!
//! Arbitrage de Bertrand du 25/09/2026. Le filtre « Compilations » de la
//! Bibliothèque mêlait de vraies compilations (« Jazz in Paris », « Blue Note
//! plays The Beatles », « Naim, the Sampler ») et des albums d'UN seul
//! artiste dont les fichiers portaient `COMPILATION=1` : « Here & Gone »
//! (David Sanborn), « A Love Supreme, Disc 1 » (John Coltrane — le Disc 2,
//! sans la balise, n'en était pas une), « Early Works » (Laurent Garnier)…
//!
//! # La règle
//!
//! Un album est une compilation si :
//!
//! - **(a)** son artiste d'album est « Various Artists » ou un équivalent
//!   reconnu ([`est_artistes_divers`]) ;
//! - **(b)** OU ses pistes ont au moins **deux artistes principaux
//!   distincts**. Un crédit « X feat. Y », « X & Y », « X; Y » est découpé
//!   par l'idiome du dépôt ([`crate::metadata::artist_split`]) : l'album est
//!   d'un seul artiste dès qu'UN nom court à travers TOUTES ses pistes — les
//!   invités d'une piste ne font donc pas un second artiste principal. Un
//!   artiste d'album nommé et unique (toutes ses graphies partageant un nom,
//!   « Fritz Reiner » / « Chicago Symphony Orchestra, Fritz Reiner ») dit à
//!   lui seul « l'album de cet artiste » : les interprètes qui varient d'une
//!   piste à l'autre (un disque classique, un chef et ses solistes) ne le
//!   changent pas en compilation — c'était déjà le cas avant cette règle —
//!   sauf si le fichier porte aussi `COMPILATION=1` : des artistes
//!   principaux variés ET la balise, c'est une compilation (un mix publié
//!   sous le nom de son compilateur) ;
//! - **(c)** OU MusicBrainz dit « Compilation », sauf si c'est contredit par
//!   un artiste d'album unique égal à l'artiste de toutes les pistes (un
//!   « Greatest Hits » d'un seul groupe reste dans sa discographie).
//!   ⚠️ Aucune source ne fournit aujourd'hui ce type secondaire :
//!   `albums.release_type` (#4777) ne garde que le type PRIMAIRE
//!   (album / EP / single). Les appelants passent donc `false` ; l'entrée
//!   existe pour que le jour où la donnée arrive, elle passe par ICI.
//!
//! La balise `COMPILATION=1` (`TCMP`, `cpil`, `ITUNESCOMPILATION`) **seule ne
//! suffit PLUS**. La balise à FAUX (`COMPILATION=0`) garde, elle, son sens
//! d'arbitrage C1 (14/09/2026) : le fichier dit explicitement « pas une
//! compilation », rien ne le renverse (le coffret de Pierre M, #3855).
//!
//! Cas non réglable par la règle, et qu'elle n'aggrave pas : un album d'un
//! seul artiste balisé `ALBUMARTIST=Various Artists` (« 10,000 Hz Legend »
//! d'Air, à cause de ses invités) reste une compilation par (a).
//!
//! # Qui l'applique
//!
//! Le scan par lots (`scan_import`), le surveillant de fichiers
//! (`auto_scan`), la passe de réparation qui relit les fichiers
//! (`reparer_compilations`) et la passe de recalcul sur la base
//! ([`crate::db::album_repo::AlbumRepo::recalculer_les_compilations`]). Aucun
//! d'eux ne réécrit la règle : ils rassemblent des [`IndicesCompilation`] et
//! appellent [`IndicesCompilation::juger`].

use std::collections::BTreeSet;

use crate::db::engine::fold_diacritics;
use crate::metadata::artist_split::split_artist_credit;

/// Les graphies d'« artistes divers » reconnues, repliées (minuscules, sans
/// accents). Réunit le vocabulaire du scan (`various artists`, `various`,
/// `va`, `compilations`), celui de la page artiste (`divers`, `artistes
/// divers`, `compilation`) et les équivalents localisés courants des
/// étiqueteurs (MusicBrainz Picard, iTunes, Mp3tag).
const ARTISTES_DIVERS: &[&str] = &[
    "various artists",
    "various artist",
    "various",
    "va",
    "v.a.",
    "v.a",
    "v/a",
    "compilations",
    "compilation",
    "artistes divers",
    "artistes varies",
    "divers",
    "divers artistes",
    "verschiedene interpreten",
    "verschiedene kunstler",
    "varios artistas",
    "artisti vari",
    "artisti varii",
    "diversi artisti",
    "vari artisti",
    "varios interpretes",
    "diverse artiesten",
];

/// Vrai pour un nom d'artiste (d'album) qui désigne « des artistes divers ».
///
/// Casse et accents ignorés : « Artistes variés » et « ARTISTES VARIES »
/// valent la même chose.
pub fn est_artistes_divers(nom: &str) -> bool {
    let cle = fold_diacritics(nom.trim()).to_lowercase();
    !cle.is_empty() && ARTISTES_DIVERS.contains(&cle.as_str())
}

/// Les noms d'un crédit, repliés — la clé de comparaison de la règle.
fn noms(credit: &str) -> BTreeSet<String> {
    split_artist_credit(credit, &[], true)
        .iter()
        .map(|n| fold_diacritics(n.trim()).to_lowercase())
        .filter(|n| !n.is_empty())
        .collect()
}

/// L'intersection d'une famille d'ensembles — les noms présents PARTOUT.
/// `None` quand la famille est vide.
fn communs<'a>(
    familles: impl IntoIterator<Item = &'a BTreeSet<String>>,
) -> Option<BTreeSet<String>> {
    let mut acc: Option<BTreeSet<String>> = None;
    for f in familles {
        acc = Some(match acc {
            None => f.clone(),
            Some(a) => a.intersection(f).cloned().collect(),
        });
    }
    acc
}

/// La graphie d'artiste d'album qui vaut pour TOUT un album d'un seul
/// artiste dont les pistes l'écrivent de plusieurs façons (#3855 : « Fritz
/// Reiner » et « Chicago Symphony Orchestra, Fritz Reiner » dans un même
/// disque).
///
/// Avant le 25/09/2026, ces deux graphies faisaient de l'album une
/// compilation, et c'est « Various Artists » qui le tenait en UNE ligne. LA
/// règle n'en fait plus une compilation : sans graphie commune, chaque
/// graphie ouvrirait sa propre ligne album. Rendue seulement quand toutes les
/// graphies partagent un nom ; c'est la plus complète (le plus de noms), puis
/// la première dans l'ordre alphabétique — un choix qui ne dépend ni de
/// l'ordre des fichiers ni du découpage en lots.
pub fn graphie_de_reference<'a>(valeurs: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut graphies: Vec<(&str, BTreeSet<String>)> = Vec::new();
    for v in valeurs {
        let v = v.trim();
        if v.is_empty() || est_artistes_divers(v) {
            continue;
        }
        if graphies.iter().any(|(g, _)| g.eq_ignore_ascii_case(v)) {
            continue;
        }
        graphies.push((v, noms(v)));
    }
    if graphies.len() < 2 {
        return None;
    }
    let commun = communs(graphies.iter().map(|(_, n)| n))?;
    if commun.is_empty() {
        return None;
    }
    graphies
        .into_iter()
        .min_by(|(ga, na), (gb, nb)| {
            nb.len()
                .cmp(&na.len())
                .then_with(|| ga.to_lowercase().cmp(&gb.to_lowercase()))
        })
        .map(|(g, _)| g.to_string())
}

/// Pourquoi la règle a tranché — ce que le journal nomme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotifCompilation {
    /// (a) — l'artiste d'album est « Various Artists » ou un équivalent.
    ArtisteDAlbumDivers,
    /// (b) — au moins deux artistes principaux distincts sur les pistes.
    PlusieursArtistes,
    /// (c) — MusicBrainz dit « Compilation », et rien ne le contredit.
    MusicBrainz,
    /// Le fichier porte `COMPILATION=0` : C1, il fait foi dans ce sens.
    BaliseNon,
    /// Un seul artiste principal. Si la balise disait `COMPILATION=1`, elle
    /// est écartée : seule, elle ne suffit plus.
    UnSeulArtiste,
    /// Rien ne permet de dire que c'est une compilation.
    SansIndice,
}

impl MotifCompilation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArtisteDAlbumDivers => "artiste_d_album_divers",
            Self::PlusieursArtistes => "plusieurs_artistes_principaux",
            Self::MusicBrainz => "musicbrainz",
            Self::BaliseNon => "balise_non",
            Self::UnSeulArtiste => "un_seul_artiste",
            Self::SansIndice => "sans_indice",
        }
    }
}

/// Le verdict de la règle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jugement {
    pub compilation: bool,
    pub motif: MotifCompilation,
    /// La balise `COMPILATION=1` était posée et la règle l'a écartée — à
    /// journaliser : c'est exactement le cas « Here & Gone ».
    pub balise_ecartee: bool,
}

/// Ce que l'on sait d'un album (ou d'un dossier) pour le juger.
///
/// Rempli piste par piste avec [`Self::ajouter_piste`] ; les valeurs sont
/// dédoublonnées, le coût suit les valeurs DISTINCTES et non les fichiers
/// (un coffret de 63 CD d'un même chef tient en quelques entrées).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndicesCompilation {
    /// Une piste porte un artiste d'album « divers ».
    artiste_d_album_divers: bool,
    /// Les artistes d'album NOMMÉS (hors « divers »), en ensembles de noms.
    artistes_d_album: BTreeSet<BTreeSet<String>>,
    /// Le crédit d'artiste de chaque piste, en ensembles de noms. À défaut de
    /// crédit, l'artiste d'album de la piste en tient lieu.
    credits: BTreeSet<BTreeSet<String>>,
    balise_oui: bool,
    balise_non: bool,
    musicbrainz_compilation: bool,
}

impl IndicesCompilation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verse une piste : son artiste d'album (balise `ALBUMARTIST`, ou ce
    /// que la base en garde) et son artiste (balise `ARTIST`, ou l'artiste
    /// résolu de la piste).
    ///
    /// ⚠️ N'y verser que des valeurs venues des BALISES (ou de la base qui
    /// les a gardées). Un artiste déduit du nom de dossier fabriquerait un
    /// faux second artiste (#3232) : c'est à l'appelant de l'écarter.
    pub fn ajouter_piste(&mut self, artiste_d_album: Option<&str>, artiste: Option<&str>) {
        let aa = artiste_d_album.map(str::trim).filter(|s| !s.is_empty());
        let mut aa_nomme = None;
        if let Some(aa) = aa {
            if est_artistes_divers(aa) {
                self.artiste_d_album_divers = true;
            } else {
                let n = noms(aa);
                if !n.is_empty() {
                    self.artistes_d_album.insert(n.clone());
                    aa_nomme = Some(n);
                }
            }
        }
        let credit = artiste
            .map(str::trim)
            .filter(|s| !s.is_empty() && !est_artistes_divers(s))
            .map(noms)
            .filter(|n| !n.is_empty())
            .or(aa_nomme);
        if let Some(c) = credit {
            self.credits.insert(c);
        }
    }

    /// Verse la balise « compilation » d'un fichier, en trois états.
    pub fn balise(&mut self, tag: Option<bool>) {
        match tag {
            Some(true) => self.balise_oui = true,
            Some(false) => self.balise_non = true,
            None => {}
        }
    }

    /// MusicBrainz classe le groupe de sortie en « Compilation ».
    pub fn musicbrainz_compilation(&mut self, oui: bool) {
        self.musicbrainz_compilation |= oui;
    }

    /// Réunit les indices d'un autre ensemble (un dossier = ses albums).
    pub fn fusionner(&mut self, autre: &IndicesCompilation) {
        self.artiste_d_album_divers |= autre.artiste_d_album_divers;
        self.artistes_d_album
            .extend(autre.artistes_d_album.iter().cloned());
        self.credits.extend(autre.credits.iter().cloned());
        self.balise_oui |= autre.balise_oui;
        self.balise_non |= autre.balise_non;
        self.musicbrainz_compilation |= autre.musicbrainz_compilation;
    }

    /// Les balises, telles que versées : `Some(true)` dès qu'un fichier dit
    /// oui (un vrai l'emporte sur un faux, comme avant), `Some(false)` si
    /// seuls des « non » ont été lus.
    pub fn balise_lue(&self) -> Option<bool> {
        match (self.balise_oui, self.balise_non) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }

    /// LA règle. Voir la documentation du module.
    pub fn juger(&self) -> Jugement {
        let balise_oui = self.balise_lue() == Some(true);
        let verdict = |compilation: bool, motif: MotifCompilation| Jugement {
            compilation,
            motif,
            balise_ecartee: balise_oui && !compilation,
        };
        // C1 — le fichier dit NON : il fait foi dans ce sens.
        if self.balise_lue() == Some(false) {
            return verdict(false, MotifCompilation::BaliseNon);
        }
        // (a)
        if self.artiste_d_album_divers {
            return verdict(true, MotifCompilation::ArtisteDAlbumDivers);
        }
        // Un nom qui court à travers TOUTES les pistes, et à travers toutes
        // les graphies de l'artiste d'album.
        let commun_pistes = communs(&self.credits);
        let commun_album = communs(&self.artistes_d_album);
        let artiste_d_album_unique = commun_album.as_ref().is_some_and(|c| !c.is_empty());
        let pistes_d_un_seul_artiste = commun_pistes.as_ref().is_some_and(|c| !c.is_empty());
        // (b) — un artiste d'album nommé et unique couvre des interprètes
        // qui varient (le classique), SAUF si le fichier se dit compilation :
        // la balise ne suffit plus seule, mais avec des artistes principaux
        // variés elle dit ce qu'elle a toujours dit (une compilation mixée
        // sous le nom de son compilateur).
        if !self.credits.is_empty()
            && !pistes_d_un_seul_artiste
            && (!artiste_d_album_unique || balise_oui)
        {
            return verdict(true, MotifCompilation::PlusieursArtistes);
        }
        // (c) — contredit par un artiste d'album unique (ou absent) égal à
        // l'artiste de toutes les pistes.
        let un_seul_artiste = pistes_d_un_seul_artiste
            && match (&commun_album, &commun_pistes) {
                (None, _) => true,
                (Some(a), Some(p)) => a.intersection(p).next().is_some(),
                (Some(_), None) => false,
            };
        if self.musicbrainz_compilation && !un_seul_artiste {
            return verdict(true, MotifCompilation::MusicBrainz);
        }
        if pistes_d_un_seul_artiste || artiste_d_album_unique {
            verdict(false, MotifCompilation::UnSeulArtiste)
        } else {
            verdict(false, MotifCompilation::SansIndice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn juger(pistes: &[(Option<&str>, Option<&str>)], tag: Option<bool>) -> Jugement {
        let mut i = IndicesCompilation::new();
        for (aa, a) in pistes {
            i.ajouter_piste(*aa, *a);
            i.balise(tag);
        }
        i.juger()
    }

    /// « Here & Gone » (David Sanborn, .18 id 10831) : un seul artiste, la
    /// balise `COMPILATION=1`. La balise seule ne suffit plus.
    #[test]
    fn un_seul_artiste_et_la_balise_n_est_pas_une_compilation() {
        let j = juger(
            &[
                (Some("David Sanborn"), Some("David Sanborn")),
                (
                    Some("David Sanborn"),
                    Some("David Sanborn feat. Eric Clapton"),
                ),
                (Some("David Sanborn"), Some("David Sanborn & Joss Stone")),
            ],
            Some(true),
        );
        assert!(!j.compilation, "{j:?}");
        assert_eq!(j.motif, MotifCompilation::UnSeulArtiste);
        assert!(
            j.balise_ecartee,
            "le journal doit dire que la balise a été écartée"
        );
    }

    /// Les deux disques d'« A Love Supreme » : même verdict, balise ou non.
    #[test]
    fn deux_disques_d_un_meme_coffret_sont_coherents() {
        let disque = |tag| {
            juger(
                &[(None, Some("John Coltrane")), (None, Some("John Coltrane"))],
                tag,
            )
            .compilation
        };
        assert_eq!(disque(Some(true)), disque(None));
        assert!(!disque(Some(true)));
    }

    /// (a) — « Jazz in Paris », « Naim, the Sampler » : artiste d'album
    /// « Various Artists », avec ou sans balise, graphies localisées comprises.
    #[test]
    fn artiste_d_album_divers_est_une_compilation() {
        for va in [
            "Various Artists",
            "VA",
            "Artistes divers",
            "Verschiedene Interpreten",
            "Varios artistas",
            "Artisti vari",
            "V.A.",
        ] {
            let j = juger(
                &[
                    (Some(va), Some("Django Reinhardt")),
                    (Some(va), Some("Stéphane Grappelli")),
                ],
                None,
            );
            assert!(j.compilation, "{va}: {j:?}");
            assert_eq!(j.motif, MotifCompilation::ArtisteDAlbumDivers);
        }
        // Même un seul artiste de piste : (a) suffit (« 10,000 Hz Legend »).
        assert!(juger(&[(Some("Various Artists"), Some("Air"))], None).compilation);
    }

    /// (b) — artistes variés, SANS balise ni artiste d'album.
    #[test]
    fn artistes_varies_sans_balise_est_une_compilation() {
        let j = juger(
            &[
                (None, Some("Barbara")),
                (None, Some("Jacques Brel")),
                (None, Some("Georges Brassens")),
            ],
            None,
        );
        assert!(j.compilation);
        assert_eq!(j.motif, MotifCompilation::PlusieursArtistes);
        // Compilation faite main : chaque piste son propre artiste d'album.
        let j = juger(
            &[
                (Some("Aretha Franklin"), Some("Aretha Franklin")),
                (Some("Otis Redding"), Some("Otis Redding")),
            ],
            None,
        );
        assert!(j.compilation);
    }

    /// Les invités ne font pas un second artiste principal.
    #[test]
    fn les_invites_ne_comptent_pas() {
        let j = juger(
            &[
                (None, Some("Santana")),
                (None, Some("Santana feat. Rob Thomas")),
                (None, Some("Santana ft. Everlast")),
            ],
            None,
        );
        assert!(!j.compilation, "{j:?}");
    }

    /// #3855 — deux graphies d'un même chef, un seul artiste de piste : pas
    /// une compilation. Et un chef avec des solistes différents non plus.
    #[test]
    fn un_chef_et_ses_graphies_n_est_pas_une_compilation() {
        let j = juger(
            &[
                (Some("Fritz Reiner"), Some("Fritz Reiner")),
                (
                    Some("Chicago Symphony Orchestra, Fritz Reiner"),
                    Some("Fritz Reiner"),
                ),
            ],
            None,
        );
        assert!(!j.compilation, "{j:?}");
        let solistes = [
            (Some("Fritz Reiner"), Some("Chicago Symphony Orchestra")),
            (Some("Fritz Reiner"), Some("Vienna Philharmonic")),
        ];
        let j = juger(&solistes, None);
        assert!(!j.compilation, "artiste d'album unique : {j:?}");
        // Les mêmes, balisés compilation : artistes variés ET balise.
        let j = juger(&solistes, Some(true));
        assert!(j.compilation, "{j:?}");
        assert_eq!(j.motif, MotifCompilation::PlusieursArtistes);
    }

    /// C1 — `COMPILATION=0` fait foi, même devant des artistes variés.
    #[test]
    fn la_balise_non_fait_foi() {
        let j = juger(&[(None, Some("A")), (None, Some("B"))], Some(false));
        assert!(!j.compilation);
        assert_eq!(j.motif, MotifCompilation::BaliseNon);
    }

    /// (c) — MusicBrainz « Compilation », contredit ou non.
    #[test]
    fn musicbrainz_compilation_sauf_artiste_unique() {
        let mut i = IndicesCompilation::new();
        i.ajouter_piste(Some("Queen"), Some("Queen"));
        i.ajouter_piste(Some("Queen"), Some("Queen"));
        i.musicbrainz_compilation(true);
        assert!(
            !i.juger().compilation,
            "un Greatest Hits de Queen reste à Queen"
        );

        let mut i = IndicesCompilation::new();
        i.ajouter_piste(Some("Label Maison"), Some("Artiste Un"));
        i.ajouter_piste(Some("Label Maison"), Some("Artiste Un"));
        i.musicbrainz_compilation(true);
        let j = i.juger();
        assert!(j.compilation, "{j:?}");
        assert_eq!(j.motif, MotifCompilation::MusicBrainz);
    }

    /// #3855 — une graphie de référence, et seulement entre graphies d'un
    /// même artiste.
    #[test]
    fn la_graphie_de_reference_est_la_plus_complete() {
        let long = "Chicago Symphony Orchestra, Fritz Reiner";
        assert_eq!(
            graphie_de_reference(["Fritz Reiner", long, "fritz reiner"]).as_deref(),
            Some(long)
        );
        assert_eq!(
            graphie_de_reference(["Fritz Reiner"]),
            None,
            "une seule graphie"
        );
        assert_eq!(
            graphie_de_reference(["Aretha Franklin", "Otis Redding"]),
            None
        );
    }

    /// Rien du tout : pas une compilation.
    #[test]
    fn sans_rien_pas_de_compilation() {
        let j = IndicesCompilation::new().juger();
        assert!(!j.compilation);
        assert_eq!(j.motif, MotifCompilation::SansIndice);
        // La balise seule, sans aucun artiste : elle ne suffit pas non plus.
        let mut i = IndicesCompilation::new();
        i.balise(Some(true));
        assert!(!i.juger().compilation);
    }
}
