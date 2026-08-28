//! LRC lyrics support: parser, sidecar discovery, embedded-tag reading.
//!
//! Canonical LRC parser for the whole codebase (the LRCLIB module in
//! `crate::lyrics` delegates here). Handles:
//! - `[mm:ss.xx]` timestamps with 1-3 fractional digits (centiseconds or
//!   milliseconds),
//! - several timestamps on one line (`[00:12.00][01:15.00]chorus` yields
//!   two entries),
//! - metadata tags (`[ar:..]`, `[ti:..]`, `[offset:..]`…) which are ignored,
//! - output sorted by `time_ms`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LrcLine {
    pub time_ms: u64,
    pub text: String,
}

pub fn parse_lrc(content: &str) -> Vec<LrcLine> {
    let mut lines = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Collect every leading `[mm:ss.xx]` timestamp. Metadata tags like
        // `[ti:Title]` fail to parse as timestamps and skip the whole line.
        let mut rest = line;
        let mut stamps: Vec<u64> = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            let Some(ms) = parse_lrc_timestamp(&after[..end]) else {
                break;
            };
            stamps.push(ms);
            rest = after[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            continue;
        }
        let text = rest.trim().to_string();
        for ms in stamps {
            lines.push(LrcLine {
                time_ms: ms,
                text: text.clone(),
            });
        }
    }
    lines.sort_by_key(|l| l.time_ms);
    lines
}

fn parse_lrc_timestamp(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let minutes: u64 = parts[0].trim().parse().ok()?;
    let sec_parts: Vec<&str> = parts[1].split('.').collect();
    let seconds: u64 = sec_parts[0].trim().parse().ok()?;
    let centiseconds: u64 = if sec_parts.len() > 1 {
        let frac = sec_parts[1].trim();
        let val: u64 = frac.parse().ok()?;
        match frac.len() {
            1 => val * 100,
            2 => val * 10,
            3 => val,
            _ => val / 10u64.pow(frac.len() as u32 - 3),
        }
    } else {
        0
    };
    Some(minutes * 60_000 + seconds * 1000 + centiseconds)
}

/// True when the text embeds at least one parseable LRC timestamp
/// (used to detect LRC content stored inside a USLT/LYRICS tag).
pub fn has_lrc_timestamps(content: &str) -> bool {
    !parse_lrc(content).is_empty()
}

/// Look for a sidecar `.lrc` next to the audio file (same stem). Tries the
/// lowercase `.lrc` extension first, then uppercase `.LRC`. Read-only:
/// never writes anything into the user's music folders.
pub fn find_sidecar_lrc(audio_path: &str) -> Option<String> {
    let candidate = sidecar_lrc_path(audio_path)?;
    std::fs::read_to_string(&candidate).ok()
}

/// Chemin du `.lrc` voisin s'il existe (même souche, `.lrc` puis `.LRC`) —
/// sans en lire le contenu.
///
/// Séparé de [`find_sidecar_lrc`] pour que la passe de fond « paroles » puisse
/// **enregistrer le chemin trouvé** dans `track_metadata` : l'indicateur de
/// couverture doit pouvoir dire « cette piste a des paroles, et les voici »,
/// pas seulement « oui ». Lecture seule : n'écrit jamais dans les dossiers de
/// musique de l'utilisateur.
pub fn sidecar_lrc_path(audio_path: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(audio_path);
    for ext in ["lrc", "LRC"] {
        let candidate = path.with_extension(ext);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Read lyrics embedded in the audio file's tags (USLT for ID3, LYRICS for
/// Vorbis comments…) via lofty — same mechanics as the scanner's
/// `read_extended_metadata`, restricted to the lyrics item and skipping
/// cover art to keep memory flat.
pub fn read_embedded_lyrics(audio_path: &str) -> Option<String> {
    use lofty::config::{ParseOptions, ParsingMode};
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::ItemKey;

    let tagged = Probe::open(audio_path)
        .and_then(|p| {
            p.options(
                ParseOptions::new()
                    .parsing_mode(ParsingMode::Relaxed)
                    .max_junk_bytes(1024 * 1024)
                    .read_cover_art(false),
            )
            .guess_file_type()?
            .read()
        })
        .ok()?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    tag.get_string(ItemKey::Lyrics)
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Écriture — la seconde moitié de l'issue #2172
// ---------------------------------------------------------------------------
//
// Tout ce qui précède lit. Ce qui suit écrit, et écrit dans les dossiers de
// musique de l'utilisateur : c'est le seul endroit du module où une erreur
// abîme quelque chose. D'où trois règles, tenues ici et pas seulement chez
// l'appelant :
//
// 1. **Rien n'est jamais écrasé.** Un `.lrc` déjà posé — fût-il en `.LRC` —
//    arrête l'écriture net.
// 2. **Un `.lrc` sans horodatage n'est pas un `.lrc`.** Écrire des paroles
//    plates sous cette extension produirait un fichier que la cascade
//    d'affichage lirait comme synchronisé et n'afficherait jamais.
// 3. **Pas de fichier audio, pas de voisin.** On ne sème pas des `.lrc`
//    orphelins dans une bibliothèque dont une piste a disparu.

/// Pourquoi un `.lrc` voisin n'a pas été écrit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarWriteError {
    /// Le chemin audio n'a pas de souche exploitable.
    BadPath,
    /// Le fichier audio n'existe pas (plus) : son voisin n'aurait aucun sens.
    MissingAudioFile,
    /// Corps vide ou sans le moindre horodatage LRC.
    NotLrc,
    /// Un fichier existe déjà à cet emplacement. **Jamais écrasé.**
    AlreadyExists(std::path::PathBuf),
    /// Échec d'entrée/sortie.
    Io(String),
}

impl std::fmt::Display for SidecarWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPath => write!(f, "chemin audio inexploitable"),
            Self::MissingAudioFile => write!(f, "fichier audio absent"),
            Self::NotLrc => write!(f, "paroles sans horodatage LRC"),
            Self::AlreadyExists(p) => write!(f, "déjà présent : {}", p.display()),
            Self::Io(e) => write!(f, "écriture : {e}"),
        }
    }
}

/// Chemin du `.lrc` voisin **à écrire** : même souche, extension `.lrc` en
/// minuscules.
///
/// Distinct de [`sidecar_lrc_path`], qui ne rend un chemin que s'il existe
/// déjà. Ici le fichier n'existe précisément pas encore.
pub fn sidecar_lrc_write_path(audio_path: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(audio_path);
    // Une souche est indispensable : `/musique/` ou `..` n'en ont pas, et
    // `with_extension` y produirait n'importe quoi.
    path.file_stem()?;
    Some(path.with_extension("lrc"))
}

/// Écrit les paroles synchronisées dans un `.lrc` posé à côté du fichier
/// audio, et rend le chemin écrit.
///
/// La seule écriture de Tune dans les dossiers de musique côté paroles. Elle
/// est **additive** : elle crée un fichier neuf, ne touche jamais au fichier
/// audio, et refuse plutôt que d'écraser.
///
/// L'appelant doit avoir vérifié le consentement — voir
/// `crate::library::lyrics_pass::write_consent_given`.
pub fn write_sidecar_lrc(
    audio_path: &str,
    lrc: &str,
) -> Result<std::path::PathBuf, SidecarWriteError> {
    use std::io::Write;

    if !has_lrc_timestamps(lrc) {
        return Err(SidecarWriteError::NotLrc);
    }
    if !std::path::Path::new(audio_path).is_file() {
        return Err(SidecarWriteError::MissingAudioFile);
    }
    // Un `.LRC` majuscule compte aussi comme « déjà là » : la lecture le
    // trouverait, et poser un `.lrc` à côté sèmerait deux vérités.
    if let Some(existing) = sidecar_lrc_path(audio_path) {
        return Err(SidecarWriteError::AlreadyExists(existing));
    }
    let target = sidecar_lrc_write_path(audio_path).ok_or(SidecarWriteError::BadPath)?;

    // `create_new` : c'est le noyau qui refuse si le fichier apparaît
    // entre-temps. Un `exists()` peut mentir une milliseconde plus tard ;
    // cette garantie-là, non.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => SidecarWriteError::AlreadyExists(target.clone()),
            _ => SidecarWriteError::Io(e.to_string()),
        })?;

    let body = if lrc.ends_with('\n') {
        lrc.to_string()
    } else {
        format!("{lrc}\n")
    };
    file.write_all(body.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| SidecarWriteError::Io(e.to_string()))?;

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_lrc() {
        let content = "[00:12.50] First line\n[00:25.30] Second line\n[01:00.00] Third line";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 12_500);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time_ms, 25_300);
        assert_eq!(lines[2].time_ms, 60_000);
    }

    #[test]
    fn skip_metadata_tags() {
        let content =
            "[ti:Song Title]\n[ar:Artist]\n[al:Album]\n[offset:+500]\n[00:05.00] Actual lyrics";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Actual lyrics");
    }

    #[test]
    fn empty_input() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("   \n\n  ").is_empty());
    }

    #[test]
    fn three_digit_milliseconds() {
        let content = "[01:23.456] Precise timing";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 83_456);
    }

    #[test]
    fn two_digit_centiseconds() {
        let content = "[00:12.34] Centi";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 12_340);
    }

    #[test]
    fn no_fractional_seconds() {
        let content = "[02:30] No fraction";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 150_000);
    }

    #[test]
    fn sorted_output() {
        let content = "[01:00.00] Later\n[00:30.00] Earlier";
        let lines = parse_lrc(content);
        assert_eq!(lines[0].text, "Earlier");
        assert_eq!(lines[1].text, "Later");
    }

    #[test]
    fn multiple_timestamps_one_line() {
        let content = "[00:12.00][01:15.00]Chorus line\n[00:30.00] Verse";
        let lines = parse_lrc(content);
        assert_eq!(lines.len(), 3);
        // Sorted: 12s chorus, 30s verse, 75s chorus.
        assert_eq!(lines[0].time_ms, 12_000);
        assert_eq!(lines[0].text, "Chorus line");
        assert_eq!(lines[1].time_ms, 30_000);
        assert_eq!(lines[1].text, "Verse");
        assert_eq!(lines[2].time_ms, 75_000);
        assert_eq!(lines[2].text, "Chorus line");
    }

    #[test]
    fn garbage_bracket_is_skipped() {
        let lines = parse_lrc("[bad] nope\nnot a timestamp");
        assert!(lines.is_empty());
    }

    #[test]
    fn detects_lrc_in_tag_content() {
        assert!(has_lrc_timestamps("[00:01.00] hey"));
        assert!(!has_lrc_timestamps("Plain lyrics\nSecond line"));
        assert!(!has_lrc_timestamps("[ar:Someone]\nstill plain"));
    }

    #[test]
    fn sidecar_nonexistent() {
        assert!(find_sidecar_lrc("/nonexistent/track.flac").is_none());
    }

    #[test]
    fn sidecar_uppercase_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let audio = dir.path().join("Song.flac");
        std::fs::write(dir.path().join("Song.LRC"), "[00:01.00] up").unwrap();
        let content = find_sidecar_lrc(audio.to_str().unwrap());
        assert_eq!(content.as_deref(), Some("[00:01.00] up"));
    }

    // -- Écriture (#2172) --------------------------------------------------

    /// Un dossier avec un « fichier audio » réel : `write_sidecar_lrc` refuse
    /// d'écrire à côté d'un fichier absent, donc il faut vraiment en poser un.
    fn faux_morceau(dir: &tempfile::TempDir, nom: &str) -> String {
        let audio = dir.path().join(nom);
        std::fs::write(&audio, b"pas vraiment du son").unwrap();
        audio.to_str().unwrap().to_string()
    }

    #[test]
    fn ecrit_un_lrc_voisin_que_la_lecture_retrouve() {
        let dir = tempfile::TempDir::new().unwrap();
        let audio = faux_morceau(&dir, "Song.flac");

        let ecrit = write_sidecar_lrc(&audio, "[00:01.00] une ligne").unwrap();
        assert_eq!(ecrit, dir.path().join("Song.lrc"));
        // La boucle se referme : ce que l'écriture pose, la lecture le trouve.
        assert_eq!(
            find_sidecar_lrc(&audio).as_deref(),
            Some("[00:01.00] une ligne\n"),
            "le corps est terminé par un saut de ligne"
        );
    }

    #[test]
    fn n_ecrase_jamais_un_lrc_deja_pose() {
        let dir = tempfile::TempDir::new().unwrap();
        let audio = faux_morceau(&dir, "Song.flac");
        std::fs::write(
            dir.path().join("Song.lrc"),
            "[00:09.00] celui de l'utilisateur",
        )
        .unwrap();

        let err = write_sidecar_lrc(&audio, "[00:01.00] le notre").unwrap_err();
        assert!(matches!(err, SidecarWriteError::AlreadyExists(_)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Song.lrc")).unwrap(),
            "[00:09.00] celui de l'utilisateur",
            "le fichier de l'utilisateur est intact"
        );
    }

    #[test]
    fn n_ecrase_pas_davantage_un_lrc_en_majuscules() {
        // La lecture accepte `.LRC` ; l'écriture doit donc le voir aussi,
        // sinon on poserait un second fichier de paroles contradictoire.
        let dir = tempfile::TempDir::new().unwrap();
        let audio = faux_morceau(&dir, "Song.flac");
        std::fs::write(dir.path().join("Song.LRC"), "[00:09.00] majuscules").unwrap();

        let err = write_sidecar_lrc(&audio, "[00:01.00] le notre").unwrap_err();
        assert!(matches!(err, SidecarWriteError::AlreadyExists(_)));
        assert!(
            !dir.path().join("Song.lrc").exists(),
            "aucun `.lrc` minuscule ne doit apparaitre a cote du `.LRC`"
        );
    }

    #[test]
    fn refuse_des_paroles_sans_horodatage() {
        let dir = tempfile::TempDir::new().unwrap();
        let audio = faux_morceau(&dir, "Song.flac");

        assert_eq!(
            write_sidecar_lrc(&audio, "Des paroles plates\nsans horodatage"),
            Err(SidecarWriteError::NotLrc)
        );
        assert_eq!(
            write_sidecar_lrc(&audio, ""),
            Err(SidecarWriteError::NotLrc)
        );
        assert!(
            !dir.path().join("Song.lrc").exists(),
            "un refus ne doit laisser aucun fichier derriere lui"
        );
    }

    #[test]
    fn refuse_d_ecrire_a_cote_d_un_fichier_audio_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let fantome = dir.path().join("Disparu.flac");
        assert_eq!(
            write_sidecar_lrc(fantome.to_str().unwrap(), "[00:01.00] x"),
            Err(SidecarWriteError::MissingAudioFile)
        );
        assert!(!dir.path().join("Disparu.lrc").exists());
    }

    #[test]
    fn chemin_d_ecriture_toujours_en_minuscules_et_sans_souche_pas_de_chemin() {
        assert_eq!(
            sidecar_lrc_write_path("/musique/Song.FLAC"),
            Some(std::path::PathBuf::from("/musique/Song.lrc"))
        );
        assert_eq!(sidecar_lrc_write_path("/"), None);
        assert_eq!(sidecar_lrc_write_path(""), None);
    }
}
