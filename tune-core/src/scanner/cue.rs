//! Lecture des feuilles CUE.
//!
//! Une feuille CUE décrit le découpage d'un enregistrement stocké en **un seul
//! fichier** : un rip « image + cue », courant en musique classique et sur les
//! rips sans perte d'un seul tenant. Sans elle, l'album entier se présente comme
//! une piste unique — ou disparaît, si le format du fichier n'est pas lu.
//!
//! Demandé par Rhorn (forum #1283, issue #1763), qui possède « de nombreux
//! fichiers classiques en mpc, aussi en cue/ape ».
//!
//! Ce module ne fait QUE lire le texte : il ne touche ni au disque, ni à la
//! base, ni au décodeur. C'est volontaire — le découpage des pistes virtuelles
//! est la partie risquée, et elle mérite un socle dont chaque cas est testable
//! sans fichier audio.

/// Une piste déclarée par la feuille.
#[derive(Debug, Clone, PartialEq)]
pub struct CueTrack {
    /// Numéro annoncé par `TRACK nn AUDIO`, tel quel.
    pub number: u32,
    pub title: Option<String>,
    /// Interprète de la piste, ou celui de l'album à défaut.
    pub performer: Option<String>,
    /// Le `FILE` sous lequel cette piste est déclarée, tel qu'écrit dans la
    /// feuille. Une feuille peut en enchaîner plusieurs — un `.cue` par face de
    /// vinyle est le cas courant, mais le format autorise aussi plusieurs
    /// `FILE` dans une seule feuille (rip piste-à-piste). Les temps sont alors
    /// relatifs à CE fichier, jamais à l'album : sans ce champ, les pistes du
    /// second fichier se superposeraient à celles du premier.
    pub audio_file: Option<String>,
    /// Début dans le fichier, en millisecondes.
    pub start_ms: u64,
    /// Fin dans le fichier, en millisecondes : le début de la piste suivante.
    /// `None` pour la dernière, qui court jusqu'au bout du fichier — sa durée
    /// réelle n'est connue qu'une fois le fichier ouvert.
    pub end_ms: Option<u64>,
}

/// Le contenu exploitable d'une feuille CUE.
#[derive(Debug, Clone, PartialEq)]
pub struct CueSheet {
    /// Les fichiers audio référencés par `FILE`, dans l'ordre de la feuille et
    /// tels qu'écrits. Ce sont des noms relatifs au dossier de la feuille.
    ///
    /// Une liste, et non un seul nom : la version précédente écrasait la valeur
    /// à chaque `FILE`, de sorte qu'une feuille multi-`FILE` ne gardait que le
    /// dernier et rattachait toutes ses pistes au mauvais fichier.
    pub audio_files: Vec<String>,
    pub album_title: Option<String>,
    pub album_performer: Option<String>,
    /// `REM GENRE …`. Hors norme mais universellement écrit par EAC et foobar :
    /// c'est le seul endroit où une feuille porte le genre de l'album.
    pub album_genre: Option<String>,
    /// `REM DATE …`. Idem — l'année de l'album, absente du CUE standard.
    pub album_date: Option<String>,
    pub tracks: Vec<CueTrack>,
}

impl CueSheet {
    /// Le premier `FILE` déclaré, qui est le seul dans l'immense majorité des
    /// feuilles.
    pub fn premier_fichier(&self) -> Option<&str> {
        self.audio_files.first().map(String::as_str)
    }
}

/// Découpe une ligne CUE en (mot-clé majuscule, reste).
fn split_keyword(line: &str) -> Option<(String, &str)> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let (kw, rest) = match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim_start()),
        None => (t, ""),
    };
    Some((kw.to_ascii_uppercase(), rest))
}

/// Retire les guillemets d'une valeur CUE.
///
/// Les champs sont censés être entre guillemets, mais beaucoup de feuilles
/// écrites à la main s'en dispensent — les accepter nues évite de perdre un
/// titre pour une question de ponctuation.
fn unquote(v: &str) -> Option<String> {
    let t = v.trim();
    let inner = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    };
    let inner = inner.trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

/// Convertit un horodatage CUE `mm:ss:ff` en millisecondes.
///
/// La troisième composante est en **frames CD**, dont il y a exactement 75 par
/// seconde — et non des centièmes. La confondre décale chaque piste de près
/// d'une seconde sur une frame élevée, une erreur qui ne s'entend qu'à
/// l'écoute et jamais dans les chiffres.
///
/// Les minutes ne sont pas plafonnées à 60 : une image d'album dépasse
/// couramment l'heure, et `99:59:74` est une valeur parfaitement légale.
pub fn parse_cue_time(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let mm: u64 = parts[0].trim().parse().ok()?;
    let ss: u64 = parts[1].trim().parse().ok()?;
    let ff: u64 = parts[2].trim().parse().ok()?;
    if ss > 59 || ff > 74 {
        return None;
    }
    Some((mm * 60 + ss) * 1000 + ff * 1000 / 75)
}

/// Lit une feuille CUE.
///
/// Tolérante par principe : une feuille mal formée doit produire ce qu'elle a
/// de lisible plutôt qu'une erreur, sinon un album entier reste invisible pour
/// une ligne parasite.
pub fn parse_cue_sheet(content: &str) -> CueSheet {
    let mut sheet = CueSheet {
        audio_files: Vec::new(),
        album_title: None,
        album_performer: None,
        album_genre: None,
        album_date: None,
        tracks: Vec::new(),
    };
    // `TITLE` et `PERFORMER` valent pour l'album AVANT le premier `TRACK`, et
    // pour la piste après : c'est la position qui décide, pas le mot-clé.
    let mut in_track = false;
    // Le `FILE` courant : toute piste déclarée ensuite lui appartient.
    let mut fichier_courant: Option<String> = None;

    for raw in content.lines() {
        let Some((kw, rest)) = split_keyword(raw) else {
            continue;
        };
        match kw.as_str() {
            "FILE" => {
                // `FILE "album.ape" WAVE` — le type suit le nom entre
                // guillemets. Sans guillemets, on garde tout sauf le dernier
                // mot, qui est le type.
                let nom = if let Some(start) = rest.find('"')
                    && let Some(len) = rest[start + 1..].find('"')
                {
                    unquote(&rest[start..start + len + 2])
                } else {
                    let mut w: Vec<&str> = rest.split_whitespace().collect();
                    if w.len() > 1 {
                        w.pop();
                    }
                    unquote(&w.join(" "))
                };
                if let Some(nom) = nom {
                    if !sheet.audio_files.contains(&nom) {
                        sheet.audio_files.push(nom.clone());
                    }
                    fichier_courant = Some(nom);
                }
            }
            // `REM` n'est pas un commentaire libre en pratique : EAC, foobar2000
            // et dBpoweramp y déposent le genre et l'année, qui n'ont pas de
            // mot-clé dans le format. Les ignorer, c'était perdre les deux
            // seules métadonnées d'album que la feuille apporte en plus du
            // titre (Gros Bidon, fil 1495 : `REM GENRE "Rock"`, `REM DATE 1984`).
            "REM" => {
                if let Some((sous_kw, valeur)) = split_keyword(rest) {
                    match sous_kw.as_str() {
                        "GENRE" => sheet.album_genre = unquote(valeur),
                        "DATE" => sheet.album_date = unquote(valeur),
                        _ => {}
                    }
                }
            }
            "TRACK" => {
                let number = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.trim_start_matches('0').parse().ok().or(Some(0)))
                    .unwrap_or(0);
                sheet.tracks.push(CueTrack {
                    number,
                    title: None,
                    performer: None,
                    audio_file: fichier_courant.clone(),
                    start_ms: 0,
                    end_ms: None,
                });
                in_track = true;
            }
            "TITLE" => {
                if in_track {
                    if let Some(t) = sheet.tracks.last_mut() {
                        t.title = unquote(rest);
                    }
                } else {
                    sheet.album_title = unquote(rest);
                }
            }
            "PERFORMER" => {
                if in_track {
                    if let Some(t) = sheet.tracks.last_mut() {
                        t.performer = unquote(rest);
                    }
                } else {
                    sheet.album_performer = unquote(rest);
                }
            }
            "INDEX" => {
                // `INDEX 00` est le pré-gap (souvent le silence avant la
                // piste) ; `INDEX 01` est le vrai début. Ne retenir que 01,
                // sans quoi chaque piste démarrerait quelques secondes trop tôt
                // et empiéterait sur la précédente.
                let mut it = rest.split_whitespace();
                let idx = it.next().unwrap_or("");
                let time = it.next().unwrap_or("");
                if idx.trim_start_matches('0').is_empty() {
                    continue; // INDEX 00
                }
                if let (Some(track), Some(ms)) = (sheet.tracks.last_mut(), parse_cue_time(time)) {
                    track.start_ms = ms;
                }
            }
            _ => {}
        }
    }

    // La fin d'une piste est le début de la suivante — mais SEULEMENT si elles
    // partagent le même fichier. Au passage d'un `FILE` à l'autre, les temps
    // repartent de zéro : chaîner par-dessus la frontière donnerait à la
    // dernière piste d'un fichier une fin ANTÉRIEURE à son début, donc une
    // durée négative. La dernière piste de chaque fichier reste ouverte
    // jusqu'à la fin de ce fichier.
    for i in 0..sheet.tracks.len().saturating_sub(1) {
        if sheet.tracks[i].audio_file != sheet.tracks[i + 1].audio_file {
            continue;
        }
        let next_start = sheet.tracks[i + 1].start_ms;
        sheet.tracks[i].end_ms = Some(next_start);
    }

    // L'interprète de l'album comble celui des pistes qui n'en déclarent pas —
    // le cas de l'écrasante majorité des feuilles.
    if let Some(album_performer) = sheet.album_performer.clone() {
        for t in sheet.tracks.iter_mut() {
            if t.performer.is_none() {
                t.performer = Some(album_performer.clone());
            }
        }
    }

    sheet
}

/// Lit une feuille depuis des octets bruts, quel que soit son encodage.
///
/// Les feuilles CUE traînent depuis vingt ans et sont rarement en UTF-8 : on
/// trouve du Latin-1, des BOM UTF-8, parfois de l'UTF-16. Échouer sur
/// l'encodage ferait disparaître l'album pour un accent, donc la lecture est
/// permissive et ne rejette jamais.
pub fn parse_cue_bytes(bytes: &[u8]) -> CueSheet {
    // BOM UTF-16 : décodage explicite, sinon le texte ressort en octets nuls
    // intercalés et plus aucun mot-clé n'est reconnu.
    if bytes.len() >= 2 {
        let (le, be) = (&bytes[..2] == b"\xff\xfe", &bytes[..2] == b"\xfe\xff");
        if le || be {
            let units: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            return parse_cue_sheet(&String::from_utf16_lossy(&units));
        }
    }
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(body) {
        Ok(s) => parse_cue_sheet(s),
        // Latin-1 : chaque octet est un point de code. C'est l'encodage de
        // repli le plus probable pour une feuille européenne non-UTF-8.
        Err(_) => {
            let s: String = body.iter().map(|&b| b as char).collect();
            parse_cue_sheet(&s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = r#"REM GENRE Classical
PERFORMER "Glenn Gould"
TITLE "Goldberg Variations"
FILE "gould.ape" WAVE
  TRACK 01 AUDIO
    TITLE "Aria"
    INDEX 00 00:00:00
    INDEX 01 00:32:00
  TRACK 02 AUDIO
    TITLE "Variatio 1"
    PERFORMER "Glenn Gould (1981)"
    INDEX 01 02:05:37
  TRACK 03 AUDIO
    TITLE "Variatio 2"
    INDEX 01 04:10:00
"#;

    #[test]
    fn reads_album_header_and_audio_file() {
        let s = parse_cue_sheet(SHEET);
        assert_eq!(s.premier_fichier(), Some("gould.ape"));
        assert_eq!(s.audio_files, vec!["gould.ape".to_string()]);
        assert_eq!(s.album_title.as_deref(), Some("Goldberg Variations"));
        assert_eq!(s.album_performer.as_deref(), Some("Glenn Gould"));
        assert_eq!(s.tracks.len(), 3);
        // Chaque piste sait de quel fichier elle est tirée.
        assert!(
            s.tracks
                .iter()
                .all(|t| t.audio_file.as_deref() == Some("gould.ape"))
        );
    }

    /// `REM GENRE` et `REM DATE` sont hors norme mais universels : c'est là que
    /// vivent le genre et l'année d'un album rippé en image + feuille.
    #[test]
    fn reads_genre_and_date_from_rem_lines() {
        let s = parse_cue_sheet(
            "REM GENRE \"Rock\"\nREM DATE 1984\nREM COMMENT \"Vinyle collection\"\nREM DISCID A20B1C0D\nTITLE \"Stationary Traveller\"\nFILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        assert_eq!(s.album_genre.as_deref(), Some("Rock"));
        assert_eq!(s.album_date.as_deref(), Some("1984"));
        // Les autres `REM` restent ignorés, sans casser la lecture.
        assert_eq!(s.album_title.as_deref(), Some("Stationary Traveller"));
    }

    /// Une feuille qui enchaîne deux `FILE` ne doit pas rattacher toutes ses
    /// pistes au dernier : chaque piste appartient au `FILE` qui la précède.
    #[test]
    fn each_track_belongs_to_the_file_declared_above_it() {
        let s = parse_cue_sheet(
            "FILE \"face-a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 04:00:00\nFILE \"face-b.flac\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\n",
        );
        assert_eq!(s.audio_files, vec!["face-a.flac", "face-b.flac"]);
        let fichiers: Vec<Option<&str>> =
            s.tracks.iter().map(|t| t.audio_file.as_deref()).collect();
        assert_eq!(
            fichiers,
            vec![
                Some("face-a.flac"),
                Some("face-a.flac"),
                Some("face-b.flac")
            ]
        );
    }

    /// Au passage d'un `FILE` à l'autre les temps repartent de zéro : chaîner
    /// la fin par-dessus la frontière donnerait une durée négative.
    #[test]
    fn end_time_never_crosses_a_file_boundary() {
        let s = parse_cue_sheet(
            "FILE \"face-a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 04:00:00\nFILE \"face-b.flac\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\nTRACK 04 AUDIO\nINDEX 01 03:00:00\n",
        );
        assert_eq!(s.tracks[0].end_ms, Some(240_000));
        // Dernière piste de la face A : ouverte jusqu'au bout de SON fichier,
        // et surtout pas fermée sur le 00:00:00 de la face B.
        assert_eq!(s.tracks[1].end_ms, None);
        assert_eq!(s.tracks[2].end_ms, Some(180_000));
        assert_eq!(s.tracks[3].end_ms, None);
    }

    /// La troisième composante est en frames CD — 75 par seconde, pas 100.
    /// Les confondre décale la piste de près d'une seconde.
    #[test]
    fn frames_are_seventy_fifths_of_a_second() {
        assert_eq!(parse_cue_time("00:00:00"), Some(0));
        assert_eq!(parse_cue_time("00:01:00"), Some(1_000));
        assert_eq!(parse_cue_time("00:00:75"), None); // 75 n'existe pas : 0..=74
        assert_eq!(parse_cue_time("00:00:74"), Some(986)); // et non 740
        assert_eq!(parse_cue_time("02:05:37"), Some(125_493));
        // Une image d'album dépasse l'heure : les minutes ne sont pas bornées.
        assert_eq!(parse_cue_time("99:59:74"), Some(5_999_986));
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert_eq!(parse_cue_time("2:05"), None);
        assert_eq!(parse_cue_time("00:60:00"), None);
        assert_eq!(parse_cue_time("aa:bb:cc"), None);
        assert_eq!(parse_cue_time(""), None);
    }

    /// INDEX 00 est le pré-gap : le retenir ferait démarrer chaque piste trop
    /// tôt et empiéter sur la précédente.
    #[test]
    fn index_01_wins_over_the_pregap() {
        let s = parse_cue_sheet(SHEET);
        assert_eq!(s.tracks[0].start_ms, 32_000, "INDEX 00 a été retenu");
    }

    #[test]
    fn track_ends_where_the_next_one_starts() {
        let s = parse_cue_sheet(SHEET);
        assert_eq!(s.tracks[0].end_ms, Some(125_493));
        assert_eq!(s.tracks[1].end_ms, Some(250_000));
        // La dernière court jusqu'au bout du fichier : sa fin est inconnue ici.
        assert_eq!(s.tracks[2].end_ms, None);
    }

    #[test]
    fn album_performer_fills_in_for_tracks_without_one() {
        let s = parse_cue_sheet(SHEET);
        assert_eq!(s.tracks[0].performer.as_deref(), Some("Glenn Gould"));
        // Mais une piste qui déclare le sien le garde.
        assert_eq!(s.tracks[1].performer.as_deref(), Some("Glenn Gould (1981)"));
    }

    #[test]
    fn titles_before_the_first_track_belong_to_the_album() {
        let s = parse_cue_sheet(SHEET);
        assert_eq!(s.album_title.as_deref(), Some("Goldberg Variations"));
        assert_eq!(s.tracks[0].title.as_deref(), Some("Aria"));
    }

    /// Beaucoup de feuilles écrites à la main omettent les guillemets.
    #[test]
    fn accepts_unquoted_values() {
        let s = parse_cue_sheet("TITLE Sans Guillemets\nFILE album.flac WAVE\nTRACK 01 AUDIO\n");
        assert_eq!(s.album_title.as_deref(), Some("Sans Guillemets"));
        assert_eq!(s.premier_fichier(), Some("album.flac"));
    }

    /// Un nom de fichier peut contenir des espaces : c'est le cas usuel.
    #[test]
    fn keeps_spaces_inside_the_file_name() {
        let s = parse_cue_sheet("FILE \"Bach - Cantatas (disc 1).flac\" WAVE\n");
        assert_eq!(s.premier_fichier(), Some("Bach - Cantatas (disc 1).flac"));
    }

    /// Une feuille bancale doit rendre ce qu'elle a, pas rien : sinon un album
    /// entier disparaît pour une ligne parasite.
    #[test]
    fn survives_a_malformed_sheet() {
        let s = parse_cue_sheet("TRACK 01 AUDIO\nINDEX 01 pas-une-heure\nREM\n\n   \n");
        assert_eq!(s.tracks.len(), 1);
        assert_eq!(s.tracks[0].start_ms, 0);
    }

    #[test]
    fn track_numbers_drop_their_leading_zero() {
        let s = parse_cue_sheet("TRACK 01 AUDIO\nTRACK 09 AUDIO\nTRACK 10 AUDIO\n");
        let nums: Vec<u32> = s.tracks.iter().map(|t| t.number).collect();
        assert_eq!(nums, vec![1, 9, 10]);
    }

    /// Les feuilles CUE sont rarement en UTF-8 : échouer sur l'encodage ferait
    /// disparaître l'album pour un accent.
    #[test]
    fn reads_latin1_and_utf8_bom_alike() {
        let utf8_bom = b"\xef\xbb\xbfTITLE \"Fant\xc3\xa9sie\"\n";
        assert_eq!(
            parse_cue_bytes(utf8_bom).album_title.as_deref(),
            Some("Fantésie")
        );
        // Latin-1 : le même « é » sur un seul octet.
        let latin1 = b"TITLE \"Fant\xe9sie\"\n";
        assert_eq!(
            parse_cue_bytes(latin1).album_title.as_deref(),
            Some("Fantésie")
        );
    }

    #[test]
    fn reads_utf16_with_bom() {
        let mut b: Vec<u8> = vec![0xff, 0xfe];
        for u in "TITLE \"Écho\"\n".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_cue_bytes(&b).album_title.as_deref(), Some("Écho"));
    }
}
