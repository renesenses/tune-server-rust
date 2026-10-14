//! #6124 — les balises RÉELLEMENT présentes dans un fichier, lues brutes.
//!
//! Demande de forum-hifi.fr (fil 41831, message #792, PhilActiveAudio75) :
//! lire `ORIGINALRELEASEDATE` et `VERSION`, et montrer la liste des balises
//! que porte vraiment le fichier, pas seulement une liste standard.
//!
//! lofty range les balises dans un modèle générique (`ItemKey`) et ÉCARTE
//! celles qu'il ne connaît pas : `VERSION` et `ORIGINALRELEASEDATE` en Vorbis,
//! tout `TXXX` dont la description lui est inconnue, tout atome MP4 libre
//! (`----:com.apple.iTunes:…`) hors de sa table. Ce module relit le fichier
//! par ses types CONCRETS (`FlacFile`, `MpegFile`, `Mp4File`…), où rien n'est
//! écarté, et rend chaque balise sous sa clé d'origine.
//!
//! Deux usages :
//!
//! * la fiche d'une piste (`GET /library/tracks/{id}/tags`) : la liste
//!   complète, relue À LA DEMANDE, jamais stockée ;
//! * le scan : [`date_originale_parmi`] et [`version_parmi`] tirent de ces
//!   paires la date de sortie originale et la version (original, réédition,
//!   remaster…), rangées dans `track_metadata`.
//!
//! # Plafonds
//!
//! Les images ne sont JAMAIS lues (`read_cover_art(false)`, comme le scan :
//! voir `sonder_balises`), et les données binaires ne sont pas rendues. Une
//! valeur est coupée à [`PLAFOND_VALEUR`] caractères, la liste à
//! [`PLAFOND_BALISES`] entrées ; la réponse dit quand l'un ou l'autre a joué.

use std::path::Path;

use serde::Serialize;

/// Le nombre maximal de balises rendues pour un fichier.
pub const PLAFOND_BALISES: usize = 512;
/// La longueur maximale d'une valeur rendue, en caractères. Des paroles
/// complètes ou une note de pochette tiennent rarement au-delà ; une valeur
/// plus longue est coupée et marquée `truncated`.
pub const PLAFOND_VALEUR: usize = 2048;
/// La longueur maximale d'une `VERSION` rangée en base.
pub const PLAFOND_VERSION: usize = 200;

/// Une balise telle que le fichier la porte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaliseBrute {
    /// Le bloc qui la porte : `vorbis`, `id3v2`, `id3v1`, `ape`, `mp4`,
    /// `riff`.
    #[serde(rename = "block")]
    pub bloc: &'static str,
    /// La clé d'origine : `ORIGINALDATE`, `TDOR`, `TXXX:VERSION`,
    /// `----:com.apple.iTunes:ORIGINALDATE`, `©nam`…
    #[serde(rename = "key")]
    pub cle: String,
    #[serde(rename = "value")]
    pub valeur: String,
    /// La valeur a été coupée à [`PLAFOND_VALEUR`] caractères.
    #[serde(rename = "truncated", skip_serializing_if = "std::ops::Not::not")]
    pub tronquee: bool,
}

/// Les balises d'un fichier, dans l'ordre où il les porte.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BalisesDuFichier {
    /// Le conteneur reconnu : `FLAC`, `MPEG`, `MP4`, `DSF`…
    pub format: Option<String>,
    #[serde(rename = "tags")]
    pub balises: Vec<BaliseBrute>,
    /// La liste a été coupée à [`PLAFOND_BALISES`] entrées.
    #[serde(rename = "truncated")]
    pub liste_tronquee: bool,
}

impl BalisesDuFichier {
    fn pousser(&mut self, bloc: &'static str, cle: impl Into<String>, valeur: &str) {
        let cle = cle.into();
        let valeur = valeur.trim_end_matches('\0');
        if cle.is_empty() || valeur.trim().is_empty() {
            return;
        }
        if self.balises.len() >= PLAFOND_BALISES {
            self.liste_tronquee = true;
            return;
        }
        // Les trames ID3 à valeurs multiples séparent par NUL.
        let valeur = valeur.replace('\0', " ; ");
        let tronquee = valeur.chars().count() > PLAFOND_VALEUR;
        let valeur = if tronquee {
            valeur.chars().take(PLAFOND_VALEUR).collect()
        } else {
            valeur
        };
        self.balises.push(BaliseBrute {
            bloc,
            cle,
            valeur,
            tronquee,
        });
    }

    /// Les paires `(clé, valeur)`, pour [`date_originale_parmi`] et
    /// [`version_parmi`].
    pub fn paires(&self) -> Vec<(String, String)> {
        self.balises
            .iter()
            .map(|b| (b.cle.clone(), b.valeur.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Les clés de la date originale et de la version.
// ---------------------------------------------------------------------------

/// Les clés qui portent une DATE de sortie originale (jour ou année), dans
/// l'ordre de préférence : Picard écrit `ORIGINALDATE`, d'autres
/// `ORIGINALRELEASEDATE` ; `TDOR` est la trame ID3v2.4 (lofty y convertit la
/// `TORY` d'un ID3v2.3).
pub const CLES_DATE_ORIGINALE: [&str; 3] = ["ORIGINALDATE", "ORIGINALRELEASEDATE", "TDOR"];
/// Les clés qui ne portent qu'une ANNÉE originale : ne servent que faute de
/// date. `TORY` est la trame ID3v2.3, que lofty convertit d'ordinaire.
pub const CLES_ANNEE_ORIGINALE: [&str; 2] = ["ORIGINALYEAR", "TORY"];
/// La clé de la version : Vorbis `VERSION`, ID3 `TXXX:VERSION`, APE
/// `Version`, MP4 `----:com.apple.iTunes:VERSION`.
pub const CLE_VERSION: &str = "VERSION";

/// Le nom d'une clé, sans son préfixe de conteneur et en capitales :
/// `TXXX:originalyear` → `ORIGINALYEAR`, `----:com.apple.iTunes:VERSION` →
/// `VERSION`, `Version` (APE) → `VERSION`. Une trame ID3 nue (`TDOR`) reste
/// telle quelle.
pub fn nom_de_cle(cle: &str) -> String {
    let nu = if let Some(reste) = cle.strip_prefix("TXXX:") {
        reste
    } else if let Some(reste) = cle.strip_prefix("----:") {
        // `----:<moyen>:<nom>` : le nom suit le DERNIER deux-points.
        reste.rsplit(':').next().unwrap_or(reste)
    } else {
        cle
    };
    nu.trim().to_ascii_uppercase()
}

/// Une date de sortie utilisable : commence par quatre chiffres (l'année),
/// ni `0000`. Rend la valeur nettoyée, bornée à 32 caractères.
///
/// La garde est délibérée : la règle « année originale » des collections et
/// les tris lisent les quatre premiers caractères comme un nombre, et
/// PostgreSQL refuse un `CAST` d'un texte qui n'en est pas un.
pub fn date_valide(brut: &str) -> Option<String> {
    let v = brut.trim();
    let annee = v.get(..4)?;
    if !annee.bytes().all(|b| b.is_ascii_digit()) || annee == "0000" {
        return None;
    }
    Some(v.chars().take(32).collect())
}

/// La date de sortie originale parmi des paires `(clé, valeur)` : la
/// première date valide des [`CLES_DATE_ORIGINALE`], dans leur ordre, sinon
/// la première année valide des [`CLES_ANNEE_ORIGINALE`].
///
/// L'ordre compte : Picard écrit à la fois `ORIGINALDATE=1971-03-05` et
/// `ORIGINALYEAR=1971`. lofty range les deux sous la même clé générique et
/// rend la première qu'il croise ; ici, le jour gagne toujours.
pub fn date_originale_parmi(paires: &[(String, String)]) -> Option<String> {
    let premiere = |cles: &[&str]| {
        cles.iter().find_map(|cle| {
            paires
                .iter()
                .filter(|(k, _)| nom_de_cle(k) == *cle)
                .find_map(|(_, v)| date_valide(v))
        })
    };
    premiere(&CLES_DATE_ORIGINALE).or_else(|| premiere(&CLES_ANNEE_ORIGINALE))
}

/// La version (original, réédition, remaster…) parmi des paires
/// `(clé, valeur)` : la première `VERSION` non vide, bornée à
/// [`PLAFOND_VERSION`] caractères.
///
/// Une ligne, comme un titre : les caractères de contrôle deviennent une
/// espace (`sanitize_untrusted_single_line_text`).
pub fn version_parmi(paires: &[(String, String)]) -> Option<String> {
    paires.iter().find_map(|(k, v)| {
        if nom_de_cle(k) != CLE_VERSION {
            return None;
        }
        let (propre, _) = super::sanitize_untrusted_single_line_text(v, "version");
        let propre = propre.trim();
        (!propre.is_empty()).then(|| propre.chars().take(PLAFOND_VERSION).collect())
    })
}

// ---------------------------------------------------------------------------
// La lecture.
// ---------------------------------------------------------------------------

/// Les balises du fichier `path`, lues par son type concret.
///
/// Erreur quand le fichier est illisible ou d'un conteneur inconnu. Un
/// fichier sans aucune balise rend une liste vide, pas une erreur.
pub fn lire_balises_du_fichier(path: &Path) -> Result<BalisesDuFichier, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "dsf" {
        return Ok(lire_dsf(path));
    }
    match crate::audio::iso9660::ouvrir_si_virtuel(path) {
        Some(lecteur) => {
            let lecteur = lecteur.map_err(|e| e.to_string())?;
            lire_par_lofty(std::io::BufReader::new(lecteur))
        }
        None => {
            let f = crate::audio::iso9660::ouvrir_fichier(path).map_err(|e| e.to_string())?;
            lire_par_lofty(std::io::BufReader::new(f))
        }
    }
}

fn options() -> lofty::config::ParseOptions {
    use lofty::config::{ParseOptions, ParsingMode};
    ParseOptions::new()
        .parsing_mode(ParsingMode::Relaxed)
        .max_junk_bytes(1024 * 1024)
        // Jamais d'image : même règle que le scan (`sonder_balises`).
        .read_cover_art(false)
        // Les propriétés audio ne servent pas ici.
        .read_properties(false)
}

fn lire_par_lofty<R: std::io::Read + std::io::Seek>(
    mut lecteur: R,
) -> Result<BalisesDuFichier, String> {
    use lofty::file::{AudioFile, FileType};
    use std::io::SeekFrom;

    let type_de_fichier = lofty::probe::Probe::new(&mut lecteur)
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .file_type()
        .ok_or_else(|| "conteneur inconnu".to_string())?;
    lecteur
        .seek(SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    let mut sortie = BalisesDuFichier {
        format: Some(format!("{type_de_fichier:?}").to_uppercase()),
        ..Default::default()
    };
    let o = options();
    let e = |e: lofty::error::LoftyError| e.to_string();
    match type_de_fichier {
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.vorbis_comments() {
                vorbis(&mut sortie, t);
            }
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
        }
        FileType::Vorbis => {
            let f = lofty::ogg::VorbisFile::read_from(&mut lecteur, o).map_err(e)?;
            vorbis(&mut sortie, f.vorbis_comments());
        }
        FileType::Opus => {
            let f = lofty::ogg::OpusFile::read_from(&mut lecteur, o).map_err(e)?;
            vorbis(&mut sortie, f.vorbis_comments());
        }
        FileType::Speex => {
            let f = lofty::ogg::SpeexFile::read_from(&mut lecteur, o).map_err(e)?;
            vorbis(&mut sortie, f.vorbis_comments());
        }
        FileType::Mpeg => {
            let f = lofty::mpeg::MpegFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
            if let Some(t) = f.ape() {
                ape(&mut sortie, t);
            }
            if let Some(t) = f.id3v1() {
                id3v1(&mut sortie, t);
            }
        }
        FileType::Aac => {
            let f = lofty::aac::AacFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
            if let Some(t) = f.id3v1() {
                id3v1(&mut sortie, t);
            }
        }
        FileType::Mp4 => {
            let f = lofty::mp4::Mp4File::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.ilst() {
                mp4(&mut sortie, t);
            }
        }
        FileType::Ape => {
            let f = lofty::ape::ApeFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.ape() {
                ape(&mut sortie, t);
            }
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
            if let Some(t) = f.id3v1() {
                id3v1(&mut sortie, t);
            }
        }
        FileType::WavPack => {
            let f = lofty::wavpack::WavPackFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.ape() {
                ape(&mut sortie, t);
            }
            if let Some(t) = f.id3v1() {
                id3v1(&mut sortie, t);
            }
        }
        FileType::Mpc => {
            let f = lofty::musepack::MpcFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.ape() {
                ape(&mut sortie, t);
            }
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
            if let Some(t) = f.id3v1() {
                id3v1(&mut sortie, t);
            }
        }
        FileType::Wav => {
            let f = lofty::iff::wav::WavFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
            if let Some(t) = f.riff_info() {
                for (k, v) in t {
                    sortie.pousser("riff", k.clone(), v);
                }
            }
        }
        FileType::Aiff => {
            let f = lofty::iff::aiff::AiffFile::read_from(&mut lecteur, o).map_err(e)?;
            if let Some(t) = f.id3v2() {
                id3v2(&mut sortie, t);
            }
        }
        autre => return Err(format!("conteneur non pris en charge : {autre:?}")),
    }
    Ok(sortie)
}

fn vorbis(sortie: &mut BalisesDuFichier, tag: &lofty::ogg::VorbisComments) {
    if let Some(v) = tag.vendor().split('\0').next().filter(|v| !v.is_empty()) {
        sortie.pousser("vorbis", "VENDOR", v);
    }
    for (k, v) in tag.items() {
        // Une pochette Ogg est une balise : jamais rendue.
        if k.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE") || k.eq_ignore_ascii_case("COVERART") {
            continue;
        }
        sortie.pousser("vorbis", k, v);
    }
}

fn id3v2(sortie: &mut BalisesDuFichier, tag: &lofty::id3::v2::Id3v2Tag) {
    use lofty::id3::v2::Frame;
    for trame in tag {
        let id = trame.id_str().to_string();
        match trame {
            Frame::Text(f) => sortie.pousser("id3v2", id, &f.value),
            Frame::UserText(f) => {
                sortie.pousser("id3v2", format!("TXXX:{}", f.description), &f.content)
            }
            Frame::Comment(f) => {
                let cle = if f.description.is_empty() {
                    id
                } else {
                    format!("{id}:{}", f.description)
                };
                sortie.pousser("id3v2", cle, &f.content);
            }
            Frame::UnsynchronizedText(f) => sortie.pousser("id3v2", id, &f.content),
            Frame::Url(f) => sortie.pousser("id3v2", id, f.url()),
            Frame::UserUrl(f) => {
                sortie.pousser("id3v2", format!("WXXX:{}", f.description), &f.content)
            }
            Frame::Timestamp(f) => sortie.pousser("id3v2", id, &f.timestamp.to_string()),
            Frame::KeyValue(f) => {
                let paires: Vec<String> = f
                    .key_value_pairs
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect();
                sortie.pousser("id3v2", id, &paires.join(" ; "));
            }
            Frame::Popularimeter(f) => {
                sortie.pousser(
                    "id3v2",
                    format!("POPM:{}", f.email),
                    &format!("{}/255", f.rating),
                );
            }
            // Images, données privées, identifiants binaires : rien de lisible.
            _ => {}
        }
    }
}

fn id3v1(sortie: &mut BalisesDuFichier, tag: &lofty::id3::v1::Id3v1Tag) {
    let champs = [
        ("TITLE", tag.title.as_deref()),
        ("ARTIST", tag.artist.as_deref()),
        ("ALBUM", tag.album.as_deref()),
        ("COMMENT", tag.comment.as_deref()),
    ];
    for (cle, valeur) in champs {
        if let Some(v) = valeur {
            sortie.pousser("id3v1", cle, v);
        }
    }
    if let Some(annee) = tag.year {
        sortie.pousser("id3v1", "YEAR", &annee.to_string());
    }
    if let Some(n) = tag.track_number {
        sortie.pousser("id3v1", "TRACK", &n.to_string());
    }
}

fn ape(sortie: &mut BalisesDuFichier, tag: &lofty::ape::ApeTag) {
    use lofty::tag::ItemValue;
    for item in tag {
        match item.value() {
            ItemValue::Text(v) | ItemValue::Locator(v) => sortie.pousser("ape", item.key(), v),
            // `Cover Art (Front)` et toute donnée binaire.
            ItemValue::Binary(_) => {}
        }
    }
}

fn mp4(sortie: &mut BalisesDuFichier, ilst: &lofty::mp4::Ilst) {
    use lofty::mp4::{AtomData, AtomIdent};
    for atome in ilst {
        let cle = match atome.ident() {
            // Les codes MP4 sont en Latin-1 : `©nam` commence par 0xA9.
            AtomIdent::Fourcc(code) => code.iter().map(|&b| b as char).collect::<String>(),
            AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
        };
        for donnee in atome.data() {
            let valeur = match donnee {
                AtomData::UTF8(s) | AtomData::UTF16(s) => s.clone(),
                AtomData::SignedInteger(n) => n.to_string(),
                AtomData::UnsignedInteger(n) => n.to_string(),
                AtomData::Bool(b) => u8::from(*b).to_string(),
                // `trkn` / `disk` : [0, 0, n (u16 BE), total (u16 BE), …].
                AtomData::Unknown { data, .. }
                    if (cle == "trkn" || cle == "disk") && data.len() >= 6 =>
                {
                    let n = u16::from_be_bytes([data[2], data[3]]);
                    let total = u16::from_be_bytes([data[4], data[5]]);
                    if total > 0 {
                        format!("{n}/{total}")
                    } else {
                        n.to_string()
                    }
                }
                // Images et données binaires : rien de lisible.
                _ => continue,
            };
            sortie.pousser("mp4", cle.clone(), &valeur);
        }
    }
}

/// Un DSF : lofty ne lit pas ce conteneur ; son bloc ID3v2 passe par le
/// lecteur maison du scan (`read_dsf_id3v2_tags`).
fn lire_dsf(path: &Path) -> BalisesDuFichier {
    let mut sortie = BalisesDuFichier {
        format: Some("DSF".into()),
        ..Default::default()
    };
    let decalage = super::parse_dsf_header_full(path)
        .ok()
        .and_then(|h| h.metadata_offset);
    if let Some(tags) = super::read_dsf_id3v2_tags(path, decalage) {
        for (id, v) in &tags.text_frames {
            sortie.pousser("id3v2", id.clone(), v);
        }
        for (desc, v) in &tags.txxx_frames {
            sortie.pousser("id3v2", format!("TXXX:{desc}"), v);
        }
        for (proprietaire, v) in &tags.ufid_frames {
            sortie.pousser("id3v2", format!("UFID:{proprietaire}"), v);
        }
    }
    sortie
}

#[cfg(test)]
#[path = "balises_brutes_tests_6124.rs"]
mod tests;
