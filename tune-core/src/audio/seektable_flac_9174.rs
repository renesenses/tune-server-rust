//! #9174 — une SEEKTABLE pour le FLAC servi à un navigateur tiré à distance.
//!
//! Le FLAC que Tune fabrique (remux DASH, encodeur natif) n'a pas de
//! SEEKTABLE : un `Range` au milieu du fichier ne tombe pas sur une trame et
//! le `<audio>` cale (0.9.42). Ce module pose un point toutes les 10 s.
//!
//! Deux chemins, sans recopie du fichier en mémoire :
//!
//! - le remux connaît chaque trame (un échantillon mp4 = une trame FLAC) : il
//!   nourrit un [`Indexeur`] trame après trame ([`Indexeur::trame_brute`]) ;
//! - le ré-encodage rend un tampon entier : [`ecrire_flac_indexe`] y retrouve
//!   les trames (synchro, CRC-8 de l'en-tête, CRC-16 de la trame entière),
//!   puis écrit l'en-tête neuf et recopie les trames en flux dans le fichier.
//!
//! Un FLAC que l'index ne sait pas lire part tel quel : l'index aide le saut,
//! il ne doit jamais faire échouer la piste.

use std::io::Write;

use super::depart_natif::{SignatureTrame, StreamInfoFlac, en_tete_de_trame, lire_streaminfo};

/// Un point d'index toutes les 10 s, comme `flac` par défaut.
const INTERVALLE_S: u64 = 10;

/// Les points d'une SEEKTABLE, posés trame après trame, dans l'ordre.
pub struct Indexeur {
    info: StreamInfoFlac,
    intervalle: u64,
    prochain: u64,
    /// Octets de trames déjà vus : le décalage de la trame suivante.
    octets: u64,
    /// Premier échantillon attendu de la trame suivante.
    attendu: u64,
    signature: Option<SignatureTrame>,
    points: Vec<u8>,
}

impl Indexeur {
    /// D'après le corps de STREAMINFO (34 octets). `None` s'il est illisible.
    pub fn depuis_streaminfo(streaminfo: &[u8]) -> Option<Self> {
        let info = lire_streaminfo(streaminfo)?;
        if info.cadence == 0 {
            return None;
        }
        Some(Self {
            info,
            intervalle: INTERVALLE_S * info.cadence as u64,
            prochain: 0,
            octets: 0,
            attendu: 0,
            signature: None,
            points: Vec::new(),
        })
    }

    fn noter(&mut self, echantillon: u64, bloc: u64, octets: u64) {
        if echantillon >= self.prochain {
            self.points.extend_from_slice(&echantillon.to_be_bytes());
            self.points.extend_from_slice(&self.octets.to_be_bytes());
            self.points
                .extend_from_slice(&(bloc.min(u16::MAX as u64) as u16).to_be_bytes());
            self.prochain = (echantillon / self.intervalle + 1) * self.intervalle;
        }
        self.octets += octets;
        self.attendu = echantillon + bloc;
    }

    /// Une trame entière, bornes connues (un paquet du remux, une trame
    /// refermée par son CRC-16). Son premier échantillon est la somme des
    /// blocs qui la précèdent : le numéro de l'en-tête, qui en blocs fixes
    /// compte les TRAMES, n'est pas relu. Faux quand l'en-tête ne se lit pas
    /// ou change de signature : l'index est alors abandonné, pas la piste.
    pub fn trame_brute(&mut self, trame: &[u8]) -> bool {
        let Some(e) = en_tete_de_trame(trame, &self.info) else {
            return false;
        };
        if self.signature.is_some_and(|s| s != e.signature) {
            return false;
        }
        self.signature = Some(e.signature);
        self.noter(self.attendu, e.bloc, trame.len() as u64);
        true
    }

    /// Le corps du bloc SEEKTABLE.
    pub fn table(&self) -> &[u8] {
        &self.points
    }

    /// Un point toutes les `echantillons` (les témoins, sur des FLAC courts).
    #[cfg(test)]
    pub(crate) fn avec_intervalle(mut self, echantillons: u64) -> Self {
        self.intervalle = echantillons.max(1);
        self
    }
}

/// L'en-tête FLAC : `fLaC`, STREAMINFO, la SEEKTABLE, puis les autres blocs
/// (une SEEKTABLE déjà présente est retirée), le dernier marqué comme tel.
pub fn en_tete_avec_seektable(blocs: &[(u8, &[u8])], table: &[u8]) -> Vec<u8> {
    let mut gardes: Vec<(u8, &[u8])> = Vec::with_capacity(blocs.len() + 1);
    if let Some(&premier) = blocs.first() {
        gardes.push(premier);
    }
    gardes.push((3, table));
    gardes.extend(blocs.iter().skip(1).copied().filter(|(k, _)| *k != 3));
    let mut out = b"fLaC".to_vec();
    let n = gardes.len();
    for (k, (kind, corps)) in gardes.into_iter().enumerate() {
        let drapeau = if k + 1 == n { 0x80 } else { 0 };
        out.push(drapeau | kind);
        out.extend_from_slice(&(corps.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(corps);
    }
    out
}

/// Les blocs de métadonnées d'un FLAC en mémoire et l'octet de la première
/// trame.
pub(crate) fn blocs_de_metadonnees(flac: &[u8]) -> Option<(Vec<(u8, &[u8])>, usize)> {
    if flac.get(..4)? != b"fLaC" {
        return None;
    }
    let mut blocs = Vec::new();
    let mut pos = 4usize;
    loop {
        let h = flac.get(pos..pos + 4)?;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        blocs.push((h[0] & 0x7F, flac.get(pos + 4..pos + 4 + len)?));
        pos += 4 + len;
        if h[0] & 0x80 != 0 {
            return Some((blocs, pos));
        }
    }
}

const fn table_crc16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut b = 0;
        while b < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            b += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// CRC-16 des trames FLAC (polynôme 0x8005, valeur initiale 0).
static CRC16: [u16; 256] = table_crc16();

fn crc16_suite(crc: u16, octet: u8) -> u16 {
    (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ octet) as usize]
}

/// Les trames d'un FLAC en mémoire, nourries à l'indexeur. Une trame n'est
/// retenue que si son en-tête est valide (synchro, CRC-8, même signature) ET
/// si le CRC-16 qui la clôt couvre exactement ses octets : un faux code de
/// synchro dans l'audio ne coupe pas une trame en deux. `false` si la
/// première trame n'est pas juste après les métadonnées ou si une trame ne
/// se referme pas.
pub(crate) fn indexer_les_trames(flac: &[u8], debut: usize, indexeur: &mut Indexeur) -> bool {
    let fin = flac.len();
    let mut cur = debut;
    if en_tete_de_trame(flac.get(cur..).unwrap_or(&[]), &indexeur.info).is_none() {
        return false;
    }
    // CRC-16 de flac[cur..k).
    let mut crc = 0u16;
    let mut k = cur;
    let mut j = cur + 2;
    while j + 1 < fin {
        if flac[j] == 0xFF && (flac[j + 1] & 0xFE) == 0xF8 && j >= cur + 6 {
            while k < j - 2 {
                crc = crc16_suite(crc, flac[k]);
                k += 1;
            }
            if crc == u16::from_be_bytes([flac[j - 2], flac[j - 1]])
                && en_tete_de_trame(&flac[j..], &indexeur.info).is_some()
            {
                if !indexeur.trame_brute(&flac[cur..j]) {
                    return false;
                }
                cur = j;
                crc = 0;
                k = cur;
                j = cur + 2;
                continue;
            }
        }
        j += 1;
    }
    // La dernière trame court jusqu'à la fin.
    if fin < cur + 6 {
        return false;
    }
    while k < fin - 2 {
        crc = crc16_suite(crc, flac[k]);
        k += 1;
    }
    crc == u16::from_be_bytes([flac[fin - 2], flac[fin - 1]]) && indexeur.trame_brute(&flac[cur..])
}

/// #9174 — écrit `flac` dans `chemin` avec une SEEKTABLE : l'en-tête neuf,
/// puis les trames recopiées telles quelles depuis le tampon (aucune seconde
/// copie en mémoire). Un FLAC que l'index ne sait pas lire est écrit tel
/// quel. Rend la taille écrite.
pub fn ecrire_flac_indexe(chemin: &str, flac: &[u8]) -> Result<u64, String> {
    let indexe = blocs_de_metadonnees(flac).and_then(|(blocs, debut)| {
        let (kind0, si) = *blocs.first()?;
        if kind0 != 0 {
            return None;
        }
        let mut indexeur = Indexeur::depuis_streaminfo(si)?;
        indexer_les_trames(flac, debut, &mut indexeur)
            .then(|| (en_tete_avec_seektable(&blocs, indexeur.table()), debut))
    });
    let fichier = std::fs::File::create(chemin).map_err(|e| format!("write temp file: {e}"))?;
    let mut w = std::io::BufWriter::new(fichier);
    let taille = match indexe {
        Some((en_tete, debut)) => {
            w.write_all(&en_tete)
                .and_then(|_| w.write_all(&flac[debut..]))
                .map_err(|e| format!("write temp file: {e}"))?;
            (en_tete.len() + flac.len() - debut) as u64
        }
        None => {
            tracing::warn!("flac_seektable_illisible_servi_sans_index");
            w.write_all(flac)
                .map_err(|e| format!("write temp file: {e}"))?;
            flac.len() as u64
        }
    };
    w.flush().map_err(|e| format!("write temp file: {e}"))?;
    Ok(taille)
}
