//! BIB-B2 — l'EMPREINTE du contenu audio décodé.
//!
//! `tracks.audio_hash` (`scanner/hasher.rs`) est un hachage d'octets du
//! CONTENEUR : il ne reconnaît que la copie exacte. Deux encodages d'un même
//! master — le rip FLAC et sa copie AAC, l'AIFF et son ALAC, 44,1/16 et 96/24
//! du même signal — n'ont aucun octet en commun et passent pour deux morceaux.
//! L'empreinte d'ici se calcule sur le SON, une fois décodé en mono à 11 025 Hz
//! (le contrat #2230 de `decode_to_pcm` garantit la cadence et le nombre de
//! canaux rendus), et se compare avec une tolérance : un encodeur avec perte
//! déplace un peu le signal (délai d'encodage, bruit de quantification), il ne
//! le change pas.
//!
//! Granularite : deux contenus de meme enveloppe et de hauteur dominante
//! voisine (un sinus pur de 440 Hz, un melange 220/330/440) se ressemblent
//! pour elle. Elle reconnait un MEME enregistrement sous deux encodages ; elle
//! ne classe pas des morceaux differents. Avant de nommer un doublon,
//! [`grouper_par_contenu_avec_durees_et_titres`] la croise avec la DUREE
//! reelle (a une seconde pres, #5455) et le TITRE (#5976 : titres normalises
//! egaux, memes mentions de version — « Titre » et « Titre (Instrumental) »
//! ne sont pas le meme enregistrement). L'artiste n'est PAS compare.
//!
//! Ce module est pur : aucune base, aucun réseau, aucune dépendance nouvelle,
//! présent dans le binaire par défaut (Tune OS sur Raspberry Pi n'a ni la
//! feature `audio-embedding`, ni `fpcalc`). Le stockage (`tracks.audio_fingerprint`),
//! le calcul dans la passe ReplayGain et l'exposition dans les doublons sont
//! la phase suivante ; la porte unique des critères est BIB-B3.
//!
//! ## La forme
//!
//! Après retrait du silence de tête, les 60 premières secondes utiles sont
//! découpées en trames de 100 ms. Chaque trame donne deux octets : l'énergie
//! (RMS en dB, rapportée au maximum de la fenêtre — donc insensible au niveau
//! global, une copie normalisée reste la même) et le taux de passages par zéro
//! (qui distingue deux sinus purs de même énergie). La comparaison cherche le
//! meilleur alignement à ±3 trames près (le délai d'un encodeur MP3 vaut ~25 ms,
//! celui d'un AAC ~50 ms) et rend une distance dans [0, 1].

use crate::audio::decode::decode_to_pcm;

/// Version de l'algorithme, préfixe de la forme sérialisée. Changer la forme,
/// c'est changer la version : deux versions ne se comparent jamais.
pub const VERSION: &str = "env100ms-v1";
/// Cadence de décodage. Mono, 11 025 Hz : assez pour l'enveloppe et le taux
/// de passages par zéro, dix fois moins de travail qu'à 44,1 kHz stéréo.
pub const TAUX: u32 = 11_025;
/// Longueur d'une trame, en échantillons (100 ms à 11 025 Hz).
pub const TRAME: usize = 1_102;
/// Fenêtre utile analysée, en secondes.
pub const FENETRE_S: f64 = 60.0;
/// Marge décodée en plus, pour absorber le silence de tête (jusqu'à 30 s).
const MARGE_SILENCE_S: f64 = 30.0;
/// Sous ce niveau d'énergie par trame (dBFS), une trame est du silence de tête.
/// Par trame et non par échantillon : le bruit de quantification d'un encodeur
/// ou un dither ne doivent pas faire commencer la fenêtre plus tôt.
const SEUIL_SILENCE_DB: f64 = -45.0;
/// Plancher de l'énergie relative, en dB.
const PLANCHER_DB: f64 = -80.0;
/// Décalage maximal essayé à la comparaison, en trames.
pub const DECALAGE_MAX: usize = 3;
/// Distance en deçà de laquelle deux empreintes désignent le même contenu.
///
/// Calibrée sur 556 fichiers réels du .18 (06/09/2026 : 153 689 paires,
/// originaux et enregistrements des mêmes morceaux, plusieurs éditions) :
/// les paires au même titre sont à 88 % sous 0,05 (351 sur 400) ; les paires
/// de morceaux DIFFÉRENTS commencent à 0,050 (8 entre 0,050 et 0,0525, 27,
/// 63, 134, 223… par pas de 0,0025 ensuite) et une seule descend sous 0,05
/// (« All Blues » / « A.T.F.W. », 0,0445). À 0,06, l'ancien seuil, 232 paires
/// étrangères passaient, et la transitivité en faisait un amas de cent
/// pistes. Entre 0,05 et 0,06 vivent surtout des rééditions d'un même
/// enregistrement mêlées à des étrangers : la zone grise n'est pas tranchée
/// ici, elle reste hors du « même contenu ».
pub const SEUIL_MEME_CONTENU: f64 = 0.05;
/// Part minimale de la plus courte empreinte qui doit se recouvrir.
const RECOUVREMENT_MIN: f64 = 0.8;

/// Une empreinte : la version et ses trames `(energie, passages_par_zero)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Empreinte {
    pub version: String,
    pub trames: Vec<[u8; 2]>,
}

impl Empreinte {
    /// Durée utile couverte, en secondes.
    pub fn duree_s(&self) -> f64 {
        self.trames.len() as f64 * TRAME as f64 / TAUX as f64
    }

    /// `env100ms-v1:<hex>`, deux octets par trame.
    pub fn serialiser(&self) -> String {
        let mut s = String::with_capacity(self.version.len() + 1 + self.trames.len() * 4);
        s.push_str(&self.version);
        s.push(':');
        for [e, z] in &self.trames {
            s.push_str(&format!("{e:02x}{z:02x}"));
        }
        s
    }

    /// L'inverse de [`Self::serialiser`] ; `None` si la forme n'est pas lisible.
    pub fn deserialiser(texte: &str) -> Option<Self> {
        let (version, hex) = texte.split_once(':')?;
        if version.is_empty() || hex.len() % 4 != 0 {
            return None;
        }
        let octets: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        let octets = octets?;
        let trames = octets
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| [c[0], c[1]])
            .collect();
        Some(Self {
            version: version.to_string(),
            trames,
        })
    }
}

/// L'empreinte d'un fichier de la bibliothèque, décodé par le décodeur commun.
/// `Err` si le fichier ne se décode pas ; `Ok(None)` s'il ne contient que du
/// silence.
pub fn empreinte_du_fichier(chemin: &str) -> Result<Option<Empreinte>, String> {
    let decode = decode_to_pcm(chemin, Some(TAUX), Some(1), 0.0, FENETRE_DECODEE_S)?;
    empreinte_d_un_decodage_adapte(decode)
}

/// La fenêtre de tête que décode l'empreinte, marge de silence comprise.
pub(crate) const FENETRE_DECODEE_S: f64 = FENETRE_S + MARGE_SILENCE_S;

/// L'empreinte tirée d'un décodage NATIF de la fenêtre de tête
/// (`decode::decode_natif(chemin, Some(TAUX), Some(1), 0.0, FENETRE_DECODEE_S)`).
///
/// #5519 — [`empreinte_du_fichier`] vaut exactement ceci appliqué à ce
/// décodage-là : `decode_to_pcm` n'est que `adapter_pcm ∘ decode_natif`. La
/// passe ReplayGain l'appelle sur le décodage qu'elle fait déjà, au lieu de
/// relire le fichier.
pub(crate) fn empreinte_d_un_decodage_natif(
    natif: crate::audio::decode::DecodedAudio,
) -> Result<Option<Empreinte>, String> {
    let decode = crate::audio::decode::adapter_pcm(natif, Some(TAUX), Some(1))?;
    empreinte_d_un_decodage_adapte(decode)
}

fn empreinte_d_un_decodage_adapte(
    decode: crate::audio::decode::DecodedAudio,
) -> Result<Option<Empreinte>, String> {
    if decode.channels != 1 || decode.sample_rate != TAUX {
        return Err(format!(
            "decodeur hors contrat : {} canaux a {} Hz",
            decode.channels, decode.sample_rate
        ));
    }
    Ok(empreinte_des_echantillons(
        &decode.samples_i32,
        decode.bit_depth,
    ))
}

/// L'empreinte d'échantillons mono à [`TAUX`], `bit_depth` bits (16, 24 ou 32).
/// `None` si la fenêtre utile est vide.
pub fn empreinte_des_echantillons(echantillons: &[i32], bit_depth: u16) -> Option<Empreinte> {
    let pleine_echelle: i64 = 1i64 << (bit_depth.clamp(8, 32) - 1);
    let rms_db = |trame: &[i32]| -> f64 {
        let somme: f64 = trame
            .iter()
            .map(|&s| {
                let x = s as f64 / pleine_echelle as f64;
                x * x
            })
            .sum();
        let rms = (somme / trame.len().max(1) as f64).sqrt();
        if rms > 0.0 {
            (20.0 * rms.log10()).max(PLANCHER_DB)
        } else {
            PLANCHER_DB
        }
    };
    let premiere_trame_utile = echantillons
        .as_chunks::<TRAME>()
        .0
        .iter()
        .position(|t| rms_db(t) > SEUIL_SILENCE_DB)?;
    let debut = premiere_trame_utile * TRAME;
    let fin = echantillons
        .len()
        .min(debut + (FENETRE_S * TAUX as f64) as usize);
    let utile = &echantillons[debut..fin];
    if utile.len() < TRAME {
        return None;
    }
    let mut energies_db: Vec<f64> = Vec::with_capacity(utile.len() / TRAME);
    let mut passages: Vec<u8> = Vec::with_capacity(utile.len() / TRAME);
    for trame in utile.as_chunks::<TRAME>().0 {
        energies_db.push(rms_db(trame));
        let croisements = trame
            .windows(2)
            .filter(|w| (w[0] < 0) != (w[1] < 0))
            .count();
        // Le taux réel dépasse rarement 0,5 en musique : on étale [0, 0,5] sur
        // l'octet entier pour que deux hauteurs se distinguent nettement.
        let taux = croisements as f64 / trame.len() as f64;
        passages.push((taux * 510.0).round().min(255.0) as u8);
    }
    let maximum = energies_db.iter().cloned().fold(PLANCHER_DB, f64::max);
    let trames = energies_db
        .iter()
        .zip(passages)
        .map(|(&db, z)| {
            let relative = (db - maximum).max(PLANCHER_DB);
            let e = ((relative - PLANCHER_DB) / -PLANCHER_DB * 255.0).round() as u8;
            [e, z]
        })
        .collect();
    Some(Empreinte {
        version: VERSION.to_string(),
        trames,
    })
}

/// La distance entre deux empreintes, dans [0, 1] : moyenne des écarts absolus
/// des deux composantes sur le meilleur alignement à ±[`DECALAGE_MAX`] trames.
/// `None` si les versions diffèrent ou si le recouvrement est insuffisant.
pub fn distance(a: &Empreinte, b: &Empreinte) -> Option<f64> {
    if a.version != b.version || a.trames.is_empty() || b.trames.is_empty() {
        return None;
    }
    let plus_courte = a.trames.len().min(b.trames.len());
    let mut meilleure: Option<f64> = None;
    for decalage in -(DECALAGE_MAX as isize)..=(DECALAGE_MAX as isize) {
        let (ia, ib) = if decalage >= 0 {
            (decalage as usize, 0usize)
        } else {
            (0usize, (-decalage) as usize)
        };
        let n = a
            .trames
            .len()
            .saturating_sub(ia)
            .min(b.trames.len().saturating_sub(ib));
        if (n as f64) < (plus_courte as f64 * RECOUVREMENT_MIN) {
            continue;
        }
        let somme: f64 = (0..n)
            .map(|i| {
                let ta = a.trames[ia + i];
                let tb = b.trames[ib + i];
                ((ta[0] as f64 - tb[0] as f64).abs() + (ta[1] as f64 - tb[1] as f64).abs()) / 510.0
            })
            .sum();
        let d = somme / n as f64;
        meilleure = Some(meilleure.map_or(d, |m: f64| m.min(d)));
    }
    meilleure
}

/// Deux empreintes désignent-elles le même enregistrement ?
pub fn meme_contenu(a: &Empreinte, b: &Empreinte) -> bool {
    distance(a, b).is_some_and(|d| d <= SEUIL_MEME_CONTENU)
}

/// Une trame sur [`PAS_GROSSIER`] pour la comparaison grossière.
pub const PAS_GROSSIER: usize = 8;
/// Au-delà, la comparaison grossière écarte la paire sans aller plus loin.
pub const SEUIL_GROSSIER: f64 = 0.18;
/// Deux enregistrements identiques n'ont pas des durées utiles qui diffèrent
/// de plus d'une seconde (rembourrage d'encodeur, fondu coupé).
pub const TOLERANCE_DUREE_TRAMES: usize = 10;

/// Distance grossière : sans décalage, une trame sur [`PAS_GROSSIER`]. Un
/// préfiltre bon marché pour écarter ce qui n'a rien à voir avant la
/// comparaison alignée. `None` si les versions diffèrent.
pub fn distance_grossiere(a: &Empreinte, b: &Empreinte) -> Option<f64> {
    if a.version != b.version {
        return None;
    }
    let n = a.trames.len().min(b.trames.len());
    if n == 0 {
        return None;
    }
    let mut somme = 0.0;
    let mut compte = 0usize;
    for i in (0..n).step_by(PAS_GROSSIER) {
        let (ta, tb) = (a.trames[i], b.trames[i]);
        somme +=
            ((ta[0] as f64 - tb[0] as f64).abs() + (ta[1] as f64 - tb[1] as f64).abs()) / 510.0;
        compte += 1;
    }
    Some(somme / compte as f64)
}

/// Trames par bloc du minorant de [`peut_etre_meme_contenu`].
const TRAMES_PAR_BLOC: usize = 10;

/// Les sommes cumulées des deux octets d'une empreinte : la somme de
/// n'importe quelle plage de trames en deux soustractions.
struct Cumuls {
    energie: Vec<i64>,
    passages: Vec<i64>,
}

impl Cumuls {
    fn de(e: &Empreinte) -> Self {
        let mut energie = Vec::with_capacity(e.trames.len() + 1);
        let mut passages = Vec::with_capacity(e.trames.len() + 1);
        let (mut se, mut sz) = (0i64, 0i64);
        energie.push(0);
        passages.push(0);
        for [t_e, t_z] in &e.trames {
            se += *t_e as i64;
            sz += *t_z as i64;
            energie.push(se);
            passages.push(sz);
        }
        Self { energie, passages }
    }
    fn longueur(&self) -> usize {
        self.energie.len() - 1
    }
}

/// Un MINORANT de [`distance`], sans parcourir les trames — #5455.
///
/// Pour chaque décalage que [`distance`] essaie (mêmes bornes, même
/// recouvrement minimal), la somme des écarts absolus trame à trame est au
/// moins la somme, bloc par bloc de [`TRAMES_PAR_BLOC`] trames, des écarts
/// absolus des SOMMES du bloc (inégalité triangulaire). Si ce minorant dépasse
/// [`SEUIL_MEME_CONTENU`] pour tous les décalages, [`meme_contenu`] est faux :
/// `false` est alors une certitude, `true` seulement une possibilité.
/// Environ cinq fois moins d'opérations que [`distance`].
fn peut_etre_meme_contenu(a: &Cumuls, b: &Cumuls) -> bool {
    let (la, lb) = (a.longueur(), b.longueur());
    if la == 0 || lb == 0 {
        return false;
    }
    let plus_courte = la.min(lb);
    // Une marge contre l'arrondi : le minorant est exact (entiers), la
    // distance est une somme de flottants.
    let seuil = SEUIL_MEME_CONTENU + 1e-9;
    for decalage in -(DECALAGE_MAX as isize)..=(DECALAGE_MAX as isize) {
        let (ia, ib) = if decalage >= 0 {
            (decalage as usize, 0usize)
        } else {
            (0usize, (-decalage) as usize)
        };
        let n = la.saturating_sub(ia).min(lb.saturating_sub(ib));
        if (n as f64) < (plus_courte as f64 * RECOUVREMENT_MIN) {
            continue;
        }
        let mut somme = 0i64;
        let mut debut = 0usize;
        while debut < n {
            let fin = (debut + TRAMES_PAR_BLOC).min(n);
            let bloc = |c: &[i64], o: usize| c[o + fin] - c[o + debut];
            somme += (bloc(&a.energie, ia) - bloc(&b.energie, ib)).abs()
                + (bloc(&a.passages, ia) - bloc(&b.passages, ib)).abs();
            debut = fin;
        }
        if somme as f64 / 510.0 / n as f64 <= seuil {
            return true;
        }
    }
    false
}

/// La tolérance sur la DURÉE RÉELLE des deux pistes, en millisecondes : la
/// même seconde que [`TOLERANCE_DUREE_TRAMES`] (dix trames de 100 ms).
///
/// 🔴 #5455 — la tolérance en trames ne borne RIEN au-delà d'une minute :
/// l'empreinte s'arrête à [`FENETRE_S`], et toute piste de plus d'une minute
/// et demie a exactement 600 trames. « Durées à une seconde près » ne triait
/// donc plus rien, et le regroupement comparait chaque piste à toutes les
/// autres — mesuré le 29/09/2026 sur Shrek : 347 s pour 2 000 pistes
/// empreintées, un temps qui quadruple quand la bibliothèque double.
pub const TOLERANCE_DUREE_MS: i64 = 1_000;

/// Regroupe des pistes par contenu : les paires dont les durées utiles sont à
/// [`TOLERANCE_DUREE_TRAMES`] près, qui passent le préfiltre grossier puis
/// [`meme_contenu`], sont réunies (union-find). Rend les groupes d'au moins
/// deux identifiants, les plus grands d'abord, identifiants croissants.
///
/// Sans durée réelle : voir [`grouper_par_contenu_avec_durees`].
pub fn grouper_par_contenu(empreintes: &[(i64, Empreinte)]) -> Vec<Vec<i64>> {
    grouper_par_contenu_avec_durees(empreintes, &[])
}

/// [`grouper_par_contenu`], où deux pistes dont les durées RÉELLES
/// (`durees_ms[i]`, en millisecondes) sont connues ne se comparent que si
/// elles sont à [`TOLERANCE_DUREE_MS`] près — #5455. Une durée absente, nulle
/// ou négative ne borne rien : la piste se compare comme avant.
///
/// Le résultat est EXACTEMENT celui de la double boucle d'origine, prédicat
/// de durée ajouté : les paires « même contenu » sont d'abord cherchées dans un
/// index (longueur d'empreinte, puis durée), puis rejouées dans l'ordre même
/// de la double boucle — la liaison complète en dépend.
pub fn grouper_par_contenu_avec_durees(
    empreintes: &[(i64, Empreinte)],
    durees_ms: &[Option<i64>],
) -> Vec<Vec<i64>> {
    grouper_par_contenu_avec_durees_et_titres(empreintes, durees_ms, &[])
}

/// Les mentions de VERSION qu'un titre peut porter : deux pistes dont l'une
/// en porte une que l'autre n'a pas ne sont pas le même enregistrement, quoi
/// qu'en dise l'empreinte (#5976). Comparées mot à mot, sur le titre
/// normalisé par [`titre_normalise`] (minuscules, sans accents).
pub const MENTIONS_DE_VERSION: &[&str] = &[
    "instrumental",
    "instrumentale",
    "instru",
    "karaoke",
    "live",
    "remix",
    "remixed",
    "rmx",
    "mix",
    "demo",
    "acoustic",
    "acoustique",
    "unplugged",
    "edit",
    "remaster",
    "remastered",
    "remasterise",
    "remasterisee",
    "mono",
    "stereo",
    "version",
    "extended",
    "orchestral",
    "acapella",
    "cappella",
    "alternate",
    "outtake",
    "rehearsal",
    "dub",
];

/// Le titre tel que le compare le faisceau « même enregistrement » : sans
/// accents, en minuscules, toute ponctuation ramenée à une espace, espaces
/// réduites. « Nightfall » et « NIGHTFALL ! » sont le même titre ;
/// « Nightfall » et « Nightfall (Instrumental) » non.
pub fn titre_normalise(titre: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let replie: String = titre
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    replie.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Les mentions de version ([`MENTIONS_DE_VERSION`]) d'un titre normalisé,
/// triées et sans doublon.
fn mentions_de_version(normalise: &str) -> Vec<&'static str> {
    let mut m: Vec<&'static str> = normalise
        .split(' ')
        .filter_map(|mot| MENTIONS_DE_VERSION.iter().copied().find(|v| *v == mot))
        .collect();
    m.sort_unstable();
    m.dedup();
    m
}

/// #5976 — deux titres peuvent-ils désigner le même enregistrement ? Non si
/// l'un porte une mention de version que l'autre n'a pas, ni si, connus tous
/// les deux, leurs formes normalisées diffèrent. Un titre absent ou vide ne
/// borne que par les mentions.
pub fn titres_compatibles(a: Option<&str>, b: Option<&str>) -> bool {
    let na = a.map(titre_normalise).unwrap_or_default();
    let nb = b.map(titre_normalise).unwrap_or_default();
    if mentions_de_version(&na) != mentions_de_version(&nb) {
        return false;
    }
    na.is_empty() || nb.is_empty() || na == nb
}

/// [`grouper_par_contenu_avec_durees`], où deux pistes ne se comparent en
/// outre que si leurs titres (`titres[i]`) sont [`titres_compatibles`] —
/// #5976 : sans lui, « Nightfall » et « Nightfall (Instrumental) », même
/// mixage sans la voix, même durée à la seconde, même première minute,
/// passaient pour le même enregistrement. Un tableau de titres vide ou trop
/// court ne borne rien au-delà de sa longueur.
pub fn grouper_par_contenu_avec_durees_et_titres(
    empreintes: &[(i64, Empreinte)],
    durees_ms: &[Option<i64>],
    titres: &[Option<String>],
) -> Vec<Vec<i64>> {
    use std::collections::BTreeMap;
    let longueur = |i: usize| empreintes[i].1.trames.len();
    let duree = |i: usize| durees_ms.get(i).copied().flatten().filter(|d| *d > 0);
    let titre = |i: usize| titres.get(i).map(|t| t.as_deref());
    let cumuls: Vec<Cumuls> = empreintes.iter().map(|(_, e)| Cumuls::de(e)).collect();
    // LIAISON COMPLÈTE, pas transitive : une piste n'entre dans un groupe que
    // si elle est « même contenu » avec CHACUN de ses membres, et deux groupes
    // ne fusionnent jamais. L'union-find d'avant chaînait les arêtes : sur le
    // banc réel, une seule paire douteuse suffisait à souder Coltrane,
    // Gainsbourg et Nougaro dans un groupe de cent pistes.
    let meme = |i: usize, j: usize| {
        if let (Some(a), Some(b)) = (titre(i), titre(j))
            && !titres_compatibles(a, b)
        {
            return false;
        }
        if let (Some(a), Some(b)) = (duree(i), duree(j))
            && (a - b).abs() > TOLERANCE_DUREE_MS
        {
            return false;
        }
        let (a, b) = (&empreintes[i].1, &empreintes[j].1);
        distance_grossiere(a, b).is_some_and(|d| d <= SEUIL_GROSSIER)
            && peut_etre_meme_contenu(&cumuls[i], &cumuls[j])
            && meme_contenu(a, b)
    };
    let mut ordre: Vec<usize> = (0..empreintes.len()).collect();
    ordre.sort_by_key(|&i| longueur(i));
    let mut rang = vec![0usize; empreintes.len()];
    for (k, &i) in ordre.iter().enumerate() {
        rang[i] = k;
    }
    // L'index : par longueur d'empreinte, les pistes de durée connue triées
    // par durée, et celles sans durée à part.
    type Case = (Vec<(i64, usize)>, Vec<usize>);
    let mut par_longueur: BTreeMap<usize, Case> = BTreeMap::new();
    for &i in &ordre {
        let case = par_longueur.entry(longueur(i)).or_default();
        match duree(i) {
            Some(d) => case.0.push((d, i)),
            None => case.1.push(i),
        }
    }
    for case in par_longueur.values_mut() {
        case.0.sort_unstable();
    }
    // Les paires « même contenu » que la double boucle d'origine aurait
    // rencontrées : `j` après `i` dans `ordre`, longueur à la tolérance près.
    let mut paires: Vec<(usize, usize)> = Vec::new();
    for &i in &ordre {
        let li = longueur(i);
        for (connues, sans_duree) in par_longueur
            .range(li..=li + TOLERANCE_DUREE_TRAMES)
            .map(|(_, c)| c)
        {
            let mut examiner = |j: usize| {
                if rang[j] > rang[i] && meme(i, j) {
                    paires.push((rang[i], rang[j]));
                }
            };
            match duree(i) {
                Some(d) => {
                    let debut = connues.partition_point(|&(dj, _)| dj < d - TOLERANCE_DUREE_MS);
                    for &(dj, j) in &connues[debut..] {
                        if dj > d + TOLERANCE_DUREE_MS {
                            break;
                        }
                        examiner(j);
                    }
                }
                None => connues.iter().for_each(|&(_, j)| examiner(j)),
            }
            sans_duree.iter().for_each(|&j| examiner(j));
        }
    }
    // Rejouées dans l'ordre de la double boucle : `i` croissant, puis `j`.
    paires.sort_unstable();
    let mut groupe_de: Vec<Option<usize>> = vec![None; empreintes.len()];
    let mut groupes: Vec<Vec<usize>> = Vec::new();
    for (ri, rj) in paires {
        let (i, j) = (ordre[ri], ordre[rj]);
        match (groupe_de[i], groupe_de[j]) {
            (None, None) => {
                groupes.push(vec![i, j]);
                groupe_de[i] = Some(groupes.len() - 1);
                groupe_de[j] = Some(groupes.len() - 1);
            }
            (Some(g), None) => {
                if groupes[g].iter().all(|&m| m == i || meme(m, j)) {
                    groupes[g].push(j);
                    groupe_de[j] = Some(g);
                }
            }
            (None, Some(g)) => {
                if groupes[g].iter().all(|&m| m == j || meme(m, i)) {
                    groupes[g].push(i);
                    groupe_de[i] = Some(g);
                }
            }
            (Some(_), Some(_)) => {}
        }
    }
    let mut sortie: Vec<Vec<i64>> = groupes
        .into_iter()
        .filter(|g| g.len() >= 2)
        .map(|g| {
            let mut ids: Vec<i64> = g.into_iter().map(|i| empreintes[i].0).collect();
            ids.sort_unstable();
            ids
        })
        .collect();
    sortie.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(nom: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(nom)
            .to_string_lossy()
            .to_string()
    }

    /// Un signal mono 16 bits à [`TAUX`] : somme de sinus, avec un silence de tête.
    fn signal(frequences: &[f64], secondes: f64, silence_s: f64, amplitude: f64) -> Vec<i32> {
        let n = (secondes * TAUX as f64) as usize;
        let mut v: Vec<i32> = vec![0; (silence_s * TAUX as f64) as usize];
        for i in 0..n {
            let t = i as f64 / TAUX as f64;
            let enveloppe = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
            let x: f64 = frequences
                .iter()
                .map(|f| (2.0 * std::f64::consts::PI * f * t).sin())
                .sum::<f64>()
                / frequences.len() as f64;
            v.push((x * enveloppe * amplitude * 32_767.0) as i32);
        }
        v
    }

    /// Simule un encodeur avec perte : délai d'encodage et bruit de quantification.
    fn avec_perte(source: &[i32], delai: usize, bruit: i32) -> Vec<i32> {
        let mut graine: u32 = 0x9E37_79B9;
        let mut v: Vec<i32> = vec![0; delai];
        for &s in source {
            graine ^= graine << 13;
            graine ^= graine >> 17;
            graine ^= graine << 5;
            let b = (graine % (2 * bruit as u32 + 1)) as i32 - bruit;
            v.push(s + b);
        }
        v
    }

    #[test]
    fn le_meme_signal_en_deux_encodages_sans_perte_a_la_meme_empreinte() {
        let wav = empreinte_du_fichier(&fixture("ape/sine_16s_c3000.wav"))
            .unwrap()
            .expect("le sinus n'est pas du silence");
        let ape = empreinte_du_fichier(&fixture("ape/sine_16s_c3000.ape"))
            .unwrap()
            .expect("le sinus n'est pas du silence");
        assert_eq!(wav.version, VERSION);
        assert!(
            wav.duree_s() > 0.9,
            "un sinus d'une seconde : {} s",
            wav.duree_s()
        );
        let d = distance(&wav, &ape).unwrap();
        assert!(d < 0.01, "WAV et APE du même sinus : distance {d}");
        assert!(meme_contenu(&wav, &ape));
    }

    #[test]
    fn un_encodage_avec_perte_reste_le_meme_contenu_et_un_autre_morceau_non() {
        let original = signal(&[220.0, 330.0, 440.0], 30.0, 1.5, 0.8);
        let a = empreinte_des_echantillons(&original, 16).unwrap();
        // Délai AAC (~50 ms) + bruit de quantification bien au-dessus du LSB.
        let copie = avec_perte(&original, 551, 48);
        let b = empreinte_des_echantillons(&copie, 16).unwrap();
        let d_copie = distance(&a, &b).unwrap();
        assert!(
            d_copie <= SEUIL_MEME_CONTENU,
            "copie avec perte : distance {d_copie}"
        );
        assert!(meme_contenu(&a, &b));
        // Copie normalisée (niveau divisé par deux) : l'énergie est relative.
        let attenuee: Vec<i32> = original.iter().map(|s| s / 2).collect();
        let c = empreinte_des_echantillons(&attenuee, 16).unwrap();
        assert!(meme_contenu(&a, &c), "le niveau global ne compte pas");
        // Un autre morceau : mêmes durées, autres hauteurs.
        let autre = signal(&[1_000.0, 1_500.0], 30.0, 1.5, 0.8);
        let z = empreinte_des_echantillons(&autre, 16).unwrap();
        let d_autre = distance(&a, &z).unwrap();
        assert!(
            d_autre > 0.15,
            "deux morceaux différents : distance {d_autre}"
        );
        assert!(!meme_contenu(&a, &z));
    }

    #[test]
    fn deux_sinus_purs_de_meme_energie_ne_se_confondent_pas() {
        let la = empreinte_des_echantillons(&signal(&[440.0], 10.0, 0.0, 0.8), 16).unwrap();
        let mi = empreinte_des_echantillons(&signal(&[1_000.0], 10.0, 0.0, 0.8), 16).unwrap();
        assert!(
            !meme_contenu(&la, &mi),
            "l'enveloppe seule ne suffirait pas ; les passages par zéro tranchent"
        );
    }

    #[test]
    fn la_forme_serialisee_se_relit_et_le_silence_ne_donne_rien() {
        let e = empreinte_des_echantillons(&signal(&[440.0], 3.0, 0.5, 0.5), 16).unwrap();
        let texte = e.serialiser();
        assert!(texte.starts_with("env100ms-v1:"));
        assert_eq!(Empreinte::deserialiser(&texte).as_ref(), Some(&e));
        assert_eq!(Empreinte::deserialiser("n-importe-quoi"), None);
        assert_eq!(empreinte_des_echantillons(&vec![0i32; 44_100], 16), None);
        assert_eq!(empreinte_des_echantillons(&[], 16), None);
        let autre_version = Empreinte {
            version: "autre".into(),
            trames: e.trames.clone(),
        };
        assert_eq!(
            distance(&e, &autre_version),
            None,
            "deux versions ne se comparent pas"
        );
    }

    #[test]
    fn la_fenetre_est_bornee_a_soixante_secondes_apres_le_silence_de_tete() {
        let long = signal(&[440.0, 660.0], 80.0, 10.0, 0.8);
        let e = empreinte_des_echantillons(&long, 16).unwrap();
        assert!((e.duree_s() - FENETRE_S).abs() < 0.2, "{} s", e.duree_s());
        let court = signal(&[440.0, 660.0], 20.0, 10.0, 0.8);
        let c = empreinte_des_echantillons(&court, 16).unwrap();
        assert!((c.duree_s() - 20.0).abs() < 0.2, "{} s", c.duree_s());
        // Les 20 premières secondes utiles sont les mêmes : même contenu.
        assert!(
            meme_contenu(&e, &c),
            "un extrait tronqué du même morceau se reconnaît si le recouvrement suffit… "
        );
    }

    #[test]
    fn grouper_par_contenu_reunit_les_copies_et_separe_les_morceaux() {
        let a = empreinte_des_echantillons(&signal(&[220.0, 330.0, 440.0], 30.0, 1.0, 0.8), 16)
            .unwrap();
        let a_perte = empreinte_des_echantillons(
            &avec_perte(&signal(&[220.0, 330.0, 440.0], 30.0, 1.0, 0.8), 551, 48),
            16,
        )
        .unwrap();
        let a_attenue = empreinte_des_echantillons(
            &signal(&[220.0, 330.0, 440.0], 30.0, 1.0, 0.8)
                .iter()
                .map(|s| s / 2)
                .collect::<Vec<_>>(),
            16,
        )
        .unwrap();
        let b =
            empreinte_des_echantillons(&signal(&[1_000.0, 1_500.0], 30.0, 1.0, 0.8), 16).unwrap();
        // Un contenu franchement different : l'empreinte (enveloppe + passages par zero)
        // ne separe PAS un sinus pur de 440 Hz d'un melange domine par 440 Hz —
        // c'est sa granularite, dite dans le doc du module.
        let c =
            empreinte_des_echantillons(&signal(&[3_000.0, 4_200.0], 30.0, 0.0, 0.8), 16).unwrap();
        let d_grossiere = distance_grossiere(&a, &a_perte).unwrap();
        assert!(d_grossiere <= SEUIL_GROSSIER, "préfiltre : {d_grossiere}");
        assert!(distance_grossiere(&a, &b).unwrap() > SEUIL_GROSSIER);
        let groupes = grouper_par_contenu(&[(7, b), (3, a_perte), (9, c), (1, a), (5, a_attenue)]);
        assert_eq!(
            groupes,
            vec![vec![1, 3, 5]],
            "les trois copies ensemble, les deux autres seuls"
        );
        assert!(grouper_par_contenu(&[]).is_empty());
    }

    /// Banc réel du 06/09/2026 : A ≈ B et B ≈ C ne font pas A ≈ C. Avec des
    /// arêtes transitives, trois pistes dont les extrêmes sont étrangères
    /// finissaient dans le même groupe — et de proche en proche, cent.
    #[test]
    fn le_regroupement_ne_chaine_pas_deux_voisins_dont_les_extremes_different() {
        let plate = |niveau: u8| Empreinte {
            version: VERSION.to_string(),
            trames: vec![[niveau, niveau]; 600],
        };
        // Pas de 12 sur les deux octets : 24 / 510 = 0,047, sous le seuil ;
        // deux pas : 0,094, au-dessus.
        let a = plate(100);
        let b = plate(112);
        let c = plate(124);
        assert!(meme_contenu(&a, &b) && meme_contenu(&b, &c) && !meme_contenu(&a, &c));
        let groupes = grouper_par_contenu(&[(1, a), (2, b), (3, c)]);
        assert_eq!(groupes, vec![vec![1, 2]], "{groupes:?}");
    }

    /// #5455 — l'ancienne double boucle, prédicat de durée ajouté : la
    /// RÉFÉRENCE que l'index doit reproduire exactement.
    fn reference_double_boucle(
        empreintes: &[(i64, Empreinte)],
        durees_ms: &[Option<i64>],
    ) -> Vec<Vec<i64>> {
        let duree = |i: usize| durees_ms.get(i).copied().flatten().filter(|d| *d > 0);
        // LIAISON COMPLÈTE, pas transitive : une piste n'entre dans un groupe que
        // si elle est « même contenu » avec CHACUN de ses membres, et deux groupes
        // ne fusionnent jamais. L'union-find d'avant chaînait les arêtes : sur le
        // banc réel, une seule paire douteuse suffisait à souder Coltrane,
        // Gainsbourg et Nougaro dans un groupe de cent pistes.
        let meme = |i: usize, j: usize| {
            if let (Some(a), Some(b)) = (duree(i), duree(j))
                && (a - b).abs() > TOLERANCE_DUREE_MS
            {
                return false;
            }
            let (a, b) = (&empreintes[i].1, &empreintes[j].1);
            distance_grossiere(a, b).is_some_and(|d| d <= SEUIL_GROSSIER) && meme_contenu(a, b)
        };
        let mut ordre: Vec<usize> = (0..empreintes.len()).collect();
        ordre.sort_by_key(|&i| empreintes[i].1.trames.len());
        let mut groupe_de: Vec<Option<usize>> = vec![None; empreintes.len()];
        let mut groupes: Vec<Vec<usize>> = Vec::new();
        for (k, &i) in ordre.iter().enumerate() {
            let li = empreintes[i].1.trames.len();
            for &j in &ordre[k + 1..] {
                if empreintes[j].1.trames.len() > li + TOLERANCE_DUREE_TRAMES {
                    break;
                }
                if !meme(i, j) {
                    continue;
                }
                match (groupe_de[i], groupe_de[j]) {
                    (None, None) => {
                        groupes.push(vec![i, j]);
                        groupe_de[i] = Some(groupes.len() - 1);
                        groupe_de[j] = Some(groupes.len() - 1);
                    }
                    (Some(g), None) => {
                        if groupes[g].iter().all(|&m| m == i || meme(m, j)) {
                            groupes[g].push(j);
                            groupe_de[j] = Some(g);
                        }
                    }
                    (None, Some(g)) => {
                        if groupes[g].iter().all(|&m| m == j || meme(m, i)) {
                            groupes[g].push(i);
                            groupe_de[i] = Some(g);
                        }
                    }
                    (Some(_), Some(_)) => {}
                }
            }
        }
        let mut sortie: Vec<Vec<i64>> = groupes
            .into_iter()
            .filter(|g| g.len() >= 2)
            .map(|g| {
                let mut ids: Vec<i64> = g.into_iter().map(|i| empreintes[i].0).collect();
                ids.sort_unstable();
                ids
            })
            .collect();
        sortie.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
        sortie
    }

    /// Des empreintes de 600 trames (toute piste de plus d'une minute et
    /// demie), en familles de variantes proches pour que la liaison complète
    /// ait des choix à faire, et des durées en partie absentes.
    fn banc_aleatoire(n: usize, graine: u64) -> (Vec<(i64, Empreinte)>, Vec<Option<i64>>) {
        let mut g = graine;
        let mut alea = move |borne: u64| {
            g = g
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (g >> 33) % borne
        };
        let mut empreintes = Vec::new();
        let mut durees = Vec::new();
        for k in 0..n {
            let famille = alea(12);
            let longueur = if alea(10) == 0 {
                590 + alea(20) as usize
            } else {
                600
            };
            let trames = (0..longueur)
                .map(|t| {
                    let base = 120 + (famille as usize * 7 + t / 50) % 40;
                    [
                        (base + alea(14) as usize) as u8,
                        (base + alea(14) as usize) as u8,
                    ]
                })
                .collect();
            empreintes.push((
                (k as i64) * 3 + 1,
                Empreinte {
                    version: VERSION.to_string(),
                    trames,
                },
            ));
            durees.push(match alea(8) {
                0 => None,
                1 => Some(0),
                _ => Some(200_000 + famille as i64 * 700 + alea(2_500) as i64),
            });
        }
        (empreintes, durees)
    }

    #[test]
    fn l_index_rend_exactement_ce_que_rendait_la_double_boucle() {
        for graine in [1_u64, 7, 5_455, 42_424] {
            let (empreintes, durees) = banc_aleatoire(160, graine);
            let attendu = reference_double_boucle(&empreintes, &durees);
            assert!(
                attendu.iter().any(|g| g.len() >= 3),
                "le banc doit exercer la liaison complète : {attendu:?}"
            );
            assert_eq!(
                grouper_par_contenu_avec_durees(&empreintes, &durees),
                attendu,
                "graine {graine}"
            );
            // Sans durées : le comportement d'avant #5455, à l'identique.
            assert_eq!(
                grouper_par_contenu(&empreintes),
                reference_double_boucle(&empreintes, &[]),
                "graine {graine}, sans durées"
            );
        }
    }

    #[test]
    fn le_minorant_ne_rejette_jamais_une_paire_que_distance_accepterait() {
        let (empreintes, _) = banc_aleatoire(120, 99);
        let cumuls: Vec<Cumuls> = empreintes.iter().map(|(_, e)| Cumuls::de(e)).collect();
        let mut rejets = 0;
        for i in 0..empreintes.len() {
            for j in 0..empreintes.len() {
                let possible = peut_etre_meme_contenu(&cumuls[i], &cumuls[j]);
                if let Some(d) = distance(&empreintes[i].1, &empreintes[j].1) {
                    let minorant_viole = !possible && d <= SEUIL_MEME_CONTENU;
                    assert!(!minorant_viole, "paire ({i}, {j}) : distance {d}");
                }
                rejets += usize::from(!possible);
            }
        }
        assert!(
            rejets > 0,
            "le minorant doit écarter des paires, sinon il ne sert à rien"
        );
    }

    #[test]
    fn deux_copies_du_meme_contenu_mais_de_durees_eloignees_ne_se_regroupent_plus() {
        let plate = Empreinte {
            version: VERSION.to_string(),
            trames: vec![[150, 150]; 600],
        };
        let e = [(1, plate.clone()), (2, plate.clone()), (3, plate)];
        assert_eq!(
            grouper_par_contenu_avec_durees(&e, &[Some(240_000), Some(240_900), Some(300_000)]),
            vec![vec![1, 2]],
            "à 0,9 s près : même contenu ; à une minute : deux morceaux"
        );
        assert_eq!(
            grouper_par_contenu_avec_durees(&e, &[Some(240_000), None, Some(300_000)]),
            vec![vec![1, 2]],
            "une durée inconnue ne borne rien, la liaison complète écarte le troisième"
        );
    }

    /// #5976 — Xandria, *Sacrificium* : « Nightfall » et « Nightfall
    /// (Instrumental) », même mixage sans la voix, même durée à la seconde,
    /// même première minute. L'empreinte les confond ; le titre les sépare.
    #[test]
    fn un_instrumental_de_meme_duree_et_meme_empreinte_n_est_pas_le_meme_enregistrement() {
        let plate = Empreinte {
            version: VERSION.to_string(),
            trames: vec![[150, 150]; 600],
        };
        let e = [(1, plate.clone()), (2, plate.clone()), (3, plate)];
        let durees = [Some(236_000), Some(236_000), Some(236_400)];
        assert_eq!(
            grouper_par_contenu_avec_durees(&e, &durees),
            vec![vec![1, 2, 3]],
            "sans les titres, les trois passent pour le même enregistrement"
        );
        let titres = [
            Some("Nightfall".to_string()),
            Some("Nightfall (Instrumental)".to_string()),
            Some("NIGHTFALL !".to_string()),
        ];
        assert_eq!(
            grouper_par_contenu_avec_durees_et_titres(&e, &durees, &titres),
            vec![vec![1, 3]],
            "l'instrumental reste seul ; la casse et la ponctuation ne comptent pas"
        );
    }

    #[test]
    fn titres_compatibles_refuse_une_mention_de_version_ou_un_autre_titre() {
        let ok = |a: &str, b: &str| titres_compatibles(Some(a), Some(b));
        assert!(ok("Nightfall", "Nightfall"));
        assert!(ok("Été indien", "ETE INDIEN"), "accents et casse repliés");
        assert!(
            ok("Don't Stop", "Don’t  Stop"),
            "apostrophes et espaces repliées"
        );
        for v in [
            "Nightfall (Instrumental)",
            "Nightfall - Live",
            "Nightfall [Remastered 2011]",
            "Nightfall (Radio Edit)",
            "Nightfall (Demo)",
            "Nightfall (Acoustic Version)",
            "Nightfall (Karaoke)",
            "Nightfall (Mono)",
            "Nightfall (Remix)",
        ] {
            assert!(!ok("Nightfall", v), "{v}");
            assert!(!ok(v, "Nightfall"), "{v} (symétrique)");
        }
        assert!(!ok("Nightfall", "Stardust"), "deux titres différents");
        assert!(
            ok("Live Forever", "Live Forever"),
            "« live » des deux côtés"
        );
        assert!(
            !ok("Song (Live)", "Song (Instrumental)"),
            "deux mentions différentes"
        );
        // Un titre inconnu ne borne que par les mentions.
        assert!(titres_compatibles(None, Some("Nightfall")));
        assert!(titres_compatibles(Some(""), Some("Nightfall")));
        assert!(!titres_compatibles(None, Some("Nightfall (Live)")));
    }
}
