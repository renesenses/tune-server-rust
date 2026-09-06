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
//! ne classe pas des morceaux differents, et BIB-B3 la croise avec la duree,
//! le titre et l'artiste avant de nommer un doublon.
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
/// Calibrée sur les témoins : 0 pour deux encodages sans perte, quelques
/// centièmes pour une simulation d'encodage avec perte, au-dessus de 0,15
/// pour deux signaux différents.
pub const SEUIL_MEME_CONTENU: f64 = 0.06;
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
    let decode = decode_to_pcm(
        chemin,
        Some(TAUX),
        Some(1),
        0.0,
        FENETRE_S + MARGE_SILENCE_S,
    )?;
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

/// Regroupe des pistes par contenu : les paires dont les durées utiles sont à
/// [`TOLERANCE_DUREE_TRAMES`] près, qui passent le préfiltre grossier puis
/// [`meme_contenu`], sont réunies (union-find). Rend les groupes d'au moins
/// deux identifiants, les plus grands d'abord, identifiants croissants.
pub fn grouper_par_contenu(empreintes: &[(i64, Empreinte)]) -> Vec<Vec<i64>> {
    fn racine(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut ordre: Vec<usize> = (0..empreintes.len()).collect();
    ordre.sort_by_key(|&i| empreintes[i].1.trames.len());
    let mut parent: Vec<usize> = (0..empreintes.len()).collect();
    for (k, &i) in ordre.iter().enumerate() {
        let li = empreintes[i].1.trames.len();
        for &j in &ordre[k + 1..] {
            if empreintes[j].1.trames.len() > li + TOLERANCE_DUREE_TRAMES {
                break;
            }
            let (a, b) = (&empreintes[i].1, &empreintes[j].1);
            if distance_grossiere(a, b).is_some_and(|d| d <= SEUIL_GROSSIER) && meme_contenu(a, b) {
                let (ra, rb) = (racine(&mut parent, i), racine(&mut parent, j));
                if ra != rb {
                    parent[rb] = ra;
                }
            }
        }
    }
    let mut groupes: std::collections::HashMap<usize, Vec<i64>> = std::collections::HashMap::new();
    for (i, (id, _)) in empreintes.iter().enumerate() {
        let r = racine(&mut parent, i);
        groupes.entry(r).or_default().push(*id);
    }
    let mut sortie: Vec<Vec<i64>> = groupes
        .into_values()
        .filter(|g| g.len() >= 2)
        .map(|mut g| {
            g.sort_unstable();
            g
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
}
