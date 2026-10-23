//! Les listes HLS (RFC 8216) : liste maîtresse et liste média.
//!
//! Un lecteur écrit ici plutôt qu'une caisse : une radio n'emploie qu'une
//! dizaine de balises, et chacune est lue pour une raison dite ci-dessous.
//! Les balises inconnues sont ignorées, comme la RFC le demande (§6.3.1) :
//! LL-HLS (`EXT-X-PART`…) et `EXT-X-PROGRAM-DATE-TIME` passent donc sans
//! dommage.

/// Une variante de la liste maîtresse (`EXT-X-STREAM-INF`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Variante {
    /// L'adresse de la liste média, résolue contre la liste maîtresse.
    pub(crate) uri: String,
    /// `BANDWIDTH`, en bit/s : le débit de pointe annoncé.
    pub(crate) bande: u64,
    /// `AVERAGE-BANDWIDTH`, en bit/s, quand la liste le donne.
    pub(crate) bande_moyenne: Option<u64>,
    /// `CODECS`, tel quel (`mp4a.40.5`).
    pub(crate) codecs: Option<String>,
    /// `RESOLUTION` présent ou codec vidéo annoncé : la variante porte de
    /// l'image, que Tune téléchargerait pour rien.
    pub(crate) video: bool,
    /// `AUDIO` : le groupe de pistes audio séparées (`EXT-X-MEDIA`).
    pub(crate) groupe_audio: Option<String>,
}

/// Une piste audio séparée (`EXT-X-MEDIA:TYPE=AUDIO`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PisteAudio {
    pub(crate) groupe: String,
    pub(crate) uri: Option<String>,
    pub(crate) par_defaut: bool,
}

/// Le chiffrement annoncé par `EXT-X-KEY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cle {
    /// `METHOD=NONE`, ou aucune balise.
    Aucune,
    /// `METHOD=AES-128` : le segment ENTIER est chiffré en AES-128-CBC, la
    /// clé est servie en clair à tout client. Ce n'est pas une DRM.
    Aes128 { uri: String, iv: Option<[u8; 16]> },
    /// Toute autre méthode (`SAMPLE-AES`, `SAMPLE-AES-CTR`, FairPlay…) :
    /// des DRM, que Tune ne contourne pas.
    Autre(String),
}

/// Un segment de la liste média.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// Son numéro de séquence : `EXT-X-MEDIA-SEQUENCE` plus son rang. C'est
    /// lui, et non l'adresse, qui dit si un segment a déjà été joué.
    pub(crate) sequence: u64,
    pub(crate) uri: String,
    /// `EXTINF`, en secondes.
    pub(crate) duree: f64,
    /// Le titre que porte `EXTINF`, quand il y en a un.
    pub(crate) titre: Option<TitreListe>,
    /// `EXT-X-DISCONTINUITY` précède ce segment.
    pub(crate) discontinuite: bool,
    pub(crate) cle: Cle,
}

/// Un titre lu dans `EXTINF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TitreListe {
    pub(crate) titre: String,
    pub(crate) artiste: Option<String>,
}

/// Une liste média (`EXT-X-TARGETDURATION`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ListeMedia {
    /// `EXT-X-TARGETDURATION`, en secondes : le rythme de relecture.
    pub(crate) cible: f64,
    pub(crate) segments: Vec<Segment>,
    /// `EXT-X-ENDLIST` : la liste ne bougera plus.
    pub(crate) fin: bool,
    /// `EXT-X-MAP` : des segments fMP4/CMAF.
    pub(crate) carte: Option<String>,
    /// `EXT-X-BYTERANGE` employé : des segments découpés dans un fichier.
    pub(crate) plages: bool,
}

/// Ce qu'est une liste lue.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Liste {
    Maitresse {
        variantes: Vec<Variante>,
        pistes_audio: Vec<PisteAudio>,
    },
    Media(ListeMedia),
}

/// Résout `uri` contre l'adresse de la liste qui la porte. Les segments de la
/// BBC sont en chemins relatifs (`bbc_radio_three-audio=96000-279994985.ts`).
pub(crate) fn resoudre(base: &str, uri: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(base).map_err(|e| format!("adresse de liste invalide : {e}"))?;
    base.join(uri)
        .map(|u| u.to_string())
        .map_err(|e| format!("adresse de segment invalide : {e}"))
}

/// Les attributs d'une balise (`CLÉ=valeur,CLÉ="valeur, avec virgule"`).
fn attributs(texte: &str) -> Vec<(String, String)> {
    let mut sortie = Vec::new();
    let mut reste = texte;
    while !reste.is_empty() {
        let Some((cle, apres)) = reste.split_once('=') else {
            break;
        };
        let cle = cle.trim().to_ascii_uppercase();
        let (valeur, suite) = if let Some(guillemets) = apres.strip_prefix('"') {
            match guillemets.split_once('"') {
                Some((v, s)) => (v.to_string(), s),
                None => (guillemets.to_string(), ""),
            }
        } else {
            match apres.split_once(',') {
                Some((v, s)) => (v.trim().to_string(), s),
                None => (apres.trim().to_string(), ""),
            }
        };
        sortie.push((cle, valeur));
        reste = suite.trim_start_matches(',').trim_start();
    }
    sortie
}

fn attribut<'a>(attrs: &'a [(String, String)], nom: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(cle, _)| cle == nom)
        .map(|(_, v)| v.as_str())
}

/// `0x0123…` → 16 octets.
fn iv_hexa(texte: &str) -> Option<[u8; 16]> {
    let hexa = texte
        .strip_prefix("0x")
        .or_else(|| texte.strip_prefix("0X"))?;
    if hexa.len() != 32 {
        return None;
    }
    let mut iv = [0u8; 16];
    for (i, octet) in iv.iter_mut().enumerate() {
        *octet = u8::from_str_radix(&hexa[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(iv)
}

/// Le titre d'un `EXTINF` : `#EXTINF:10,Artiste - Titre`, ou la forme à
/// attributs de certains diffuseurs (`#EXTINF:10,title="Titre",artist="Artiste"`).
/// « no desc » (Unified Streaming, celui de la BBC) et un texte vide ne sont
/// pas des titres.
fn titre_extinf(texte: &str) -> Option<TitreListe> {
    let texte = texte.trim();
    if texte.contains("title=\"") {
        let attrs = attributs(texte);
        let titre = attribut(&attrs, "TITLE")
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        return Some(TitreListe {
            titre: titre.to_string(),
            artiste: attribut(&attrs, "ARTIST")
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string),
        });
    }
    if texte.is_empty() || texte.eq_ignore_ascii_case("no desc") {
        return None;
    }
    Some(match texte.split_once(" - ") {
        Some((artiste, titre)) if !artiste.trim().is_empty() && !titre.trim().is_empty() => {
            TitreListe {
                titre: titre.trim().to_string(),
                artiste: Some(artiste.trim().to_string()),
            }
        }
        _ => TitreListe {
            titre: texte.to_string(),
            artiste: None,
        },
    })
}

/// Lit une liste HLS. `base` est l'adresse FINALE de la liste (après
/// redirection) : les chemins relatifs se résolvent contre elle.
pub(crate) fn lire(texte: &str, base: &str) -> Result<Liste, String> {
    let texte = texte.trim_start_matches('\u{feff}');
    let mut lignes = texte.lines().map(str::trim).filter(|l| !l.is_empty());
    if lignes.next() != Some("#EXTM3U") {
        return Err("la liste ne commence pas par #EXTM3U".into());
    }

    let mut variantes = Vec::new();
    let mut pistes_audio = Vec::new();
    let mut variante_en_cours: Option<Variante> = None;

    let mut media = false;
    let mut cible = None;
    let mut sequence = 0u64;
    let mut segments = Vec::new();
    let mut fin = false;
    let mut carte = None;
    let mut plages = false;
    let mut cle = Cle::Aucune;
    let mut extinf: Option<(f64, Option<TitreListe>)> = None;
    let mut discontinuite = false;

    for ligne in lignes {
        if let Some(reste) = ligne.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = attributs(reste);
            let codecs = attribut(&attrs, "CODECS").map(str::to_string);
            let video = attribut(&attrs, "RESOLUTION").is_some()
                || codecs.as_deref().is_some_and(|c| {
                    c.split(',').any(|c| {
                        let c = c.trim();
                        c.starts_with("avc") || c.starts_with("hvc") || c.starts_with("hev")
                    })
                });
            variante_en_cours = Some(Variante {
                uri: String::new(),
                bande: attribut(&attrs, "BANDWIDTH")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0),
                bande_moyenne: attribut(&attrs, "AVERAGE-BANDWIDTH").and_then(|b| b.parse().ok()),
                codecs,
                video,
                groupe_audio: attribut(&attrs, "AUDIO").map(str::to_string),
            });
        } else if let Some(reste) = ligne.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributs(reste);
            if attribut(&attrs, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO")) {
                pistes_audio.push(PisteAudio {
                    groupe: attribut(&attrs, "GROUP-ID").unwrap_or("").to_string(),
                    uri: attribut(&attrs, "URI")
                        .map(|u| resoudre(base, u))
                        .transpose()?,
                    par_defaut: attribut(&attrs, "DEFAULT")
                        .is_some_and(|d| d.eq_ignore_ascii_case("YES")),
                });
            }
        } else if let Some(reste) = ligne.strip_prefix("#EXT-X-TARGETDURATION:") {
            media = true;
            cible = reste.trim().parse::<f64>().ok();
        } else if let Some(reste) = ligne.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media = true;
            sequence = reste
                .trim()
                .parse()
                .map_err(|_| format!("EXT-X-MEDIA-SEQUENCE illisible : {reste}"))?;
        } else if let Some(reste) = ligne.strip_prefix("#EXTINF:") {
            media = true;
            let (duree, titre) = reste.split_once(',').unwrap_or((reste, ""));
            extinf = Some((duree.trim().parse().unwrap_or(0.0), titre_extinf(titre)));
        } else if ligne == "#EXT-X-DISCONTINUITY" {
            discontinuite = true;
        } else if ligne == "#EXT-X-ENDLIST" {
            fin = true;
        } else if let Some(reste) = ligne.strip_prefix("#EXT-X-MAP:") {
            carte = Some(attribut(&attributs(reste), "URI").unwrap_or("").to_string());
        } else if ligne.starts_with("#EXT-X-BYTERANGE") {
            plages = true;
        } else if let Some(reste) = ligne.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributs(reste);
            let methode = attribut(&attrs, "METHOD")
                .unwrap_or("NONE")
                .to_ascii_uppercase();
            cle = match methode.as_str() {
                "NONE" => Cle::Aucune,
                "AES-128" if attribut(&attrs, "KEYFORMAT").is_none_or(|f| f == "identity") => {
                    Cle::Aes128 {
                        uri: resoudre(base, attribut(&attrs, "URI").unwrap_or(""))?,
                        iv: attribut(&attrs, "IV").and_then(iv_hexa),
                    }
                }
                _ => Cle::Autre(methode),
            };
        } else if ligne.starts_with('#') {
            // Balise inconnue ou commentaire : ignorés (RFC 8216 §6.3.1).
        } else if let Some(mut variante) = variante_en_cours.take() {
            variante.uri = resoudre(base, ligne)?;
            variantes.push(variante);
        } else if let Some((duree, titre)) = extinf.take() {
            segments.push(Segment {
                sequence: sequence + segments.len() as u64,
                uri: resoudre(base, ligne)?,
                duree,
                titre,
                discontinuite: std::mem::take(&mut discontinuite),
                cle: cle.clone(),
            });
        }
    }

    if !variantes.is_empty() {
        return Ok(Liste::Maitresse {
            variantes,
            pistes_audio,
        });
    }
    if !media {
        return Err("ni variante ni segment : ce n'est pas une liste HLS".into());
    }
    let cible = cible.ok_or("EXT-X-TARGETDURATION absent de la liste média")?;
    Ok(Liste::Media(ListeMedia {
        cible,
        segments,
        fin,
        carte,
        plages,
    }))
}

/// Les codecs que Tune décode dans un segment HLS : l'AAC (LC, HE-AAC,
/// HE-AACv2) et le MP3. Le reste (AC-3, E-AC-3, Opus, FLAC — ces deux
/// derniers n'existent qu'en fMP4) est écarté du choix.
fn codec_decodable(codec: &str) -> bool {
    let c = codec.trim().to_ascii_lowercase();
    matches!(
        c.as_str(),
        "mp4a.40.2" | "mp4a.40.5" | "mp4a.40.29" | "mp4a.40.34" | "mp4a.69" | "mp4a.6b"
    ) || c.starts_with("avc")
        || c.starts_with("hvc")
        || c.starts_with("hev")
}

/// La variante porte-t-elle un son que Tune sait lire ? Sans `CODECS`, on
/// ne sait pas : on essaie, le premier segment tranchera.
fn variante_decodable(v: &Variante) -> bool {
    match v.codecs.as_deref() {
        None => true,
        Some(codecs) => {
            let liste: Vec<&str> = codecs.split(',').collect();
            liste.iter().all(|c| codec_decodable(c))
                && (liste.iter().any(|c| c.trim().starts_with("mp4a")) || v.groupe_audio.is_some())
        }
    }
}

/// L'ordre dans lequel essayer les variantes.
///
/// Le choix, justifié :
///
/// * **le son seul d'abord**. Une variante avec de l'image coûte des
///   mégaoctets par segment que Tune jetterait ;
/// * **puis le débit le plus haut**. Une radio HLS publie des variantes
///   audio de 48 à 320 kbit/s : même la plus haute ne pèse rien face à un
///   réseau domestique, et c'est la meilleure qualité. On ne fait PAS d'ABR :
///   changer de variante en cours d'écoute change souvent le codec ou la
///   fréquence (BBC : AAC-LC 320k au Royaume-Uni, HE-AAC 96k ailleurs), ce
///   qui couperait la session (garde `expected_format` du lecteur) ;
/// * **les suivantes servent de repli** : une variante qui refuse
///   (403 géographique, 404) ou ne s'ouvre pas laisse sa place à la suivante,
///   moins haute, au moment de l'ouverture.
///
/// `plafond_bps` borne le débit quand l'auditeur le demande
/// (`TUNE_HLS_DEBIT_MAX_KBPS`) ; les variantes au-dessus passent en dernier,
/// sans disparaître : mieux vaut une variante trop haute que pas de son.
pub(crate) fn ordre_des_variantes(
    variantes: &[Variante],
    plafond_bps: Option<u64>,
) -> Vec<Variante> {
    let mut retenues: Vec<Variante> = variantes
        .iter()
        .filter(|v| variante_decodable(v))
        .cloned()
        .collect();
    retenues.sort_by(|a, b| {
        let au_dessus = |v: &Variante| plafond_bps.is_some_and(|p| v.bande > p);
        (a.video, au_dessus(a))
            .cmp(&(b.video, au_dessus(b)))
            .then(b.bande.cmp(&a.bande))
            .then(b.bande_moyenne.cmp(&a.bande_moyenne))
    });
    retenues
}

/// L'adresse de la liste média à lire pour une variante : la sienne, ou
/// celle de la piste audio de son groupe (par défaut, sinon la première)
/// quand le son voyage à part.
pub(crate) fn liste_audio_de(variante: &Variante, pistes: &[PisteAudio]) -> String {
    if let Some(groupe) = variante.groupe_audio.as_deref() {
        let du_groupe: Vec<&PisteAudio> = pistes
            .iter()
            .filter(|p| p.groupe == groupe && p.uri.is_some())
            .collect();
        if let Some(piste) = du_groupe
            .iter()
            .find(|p| p.par_defaut)
            .or_else(|| du_groupe.first())
            && let Some(uri) = piste.uri.clone()
        {
            return uri;
        }
    }
    variante.uri.clone()
}

/// Le plafond de débit réglé par l'auditeur, en bit/s.
pub(crate) fn plafond_regle() -> Option<u64> {
    std::env::var("TUNE_HLS_DEBIT_MAX_KBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|k| *k > 0)
        .map(|k| k * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example.net/live/station/master.m3u8";

    #[test]
    fn la_liste_maitresse_de_la_bbc_donne_sa_variante_resolue() {
        let texte = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=101760,CODECS=\"mp4a.40.5\"\nvariante%3d96000.m3u8\n";
        let Liste::Maitresse { variantes, .. } = lire(texte, BASE).unwrap() else {
            panic!("liste maîtresse attendue");
        };
        assert_eq!(variantes.len(), 1);
        assert_eq!(
            variantes[0].uri,
            "https://cdn.example.net/live/station/variante%3d96000.m3u8"
        );
        assert_eq!(variantes[0].bande, 101_760);
        assert_eq!(variantes[0].codecs.as_deref(), Some("mp4a.40.5"));
        assert!(!variantes[0].video);
    }

    #[test]
    fn la_liste_media_numerote_ses_segments_par_la_sequence() {
        let texte = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:279994985\n#EXT-X-TARGETDURATION:6\n#EXT-X-PROGRAM-DATE-TIME:2026-10-14T08:51:37.600000Z\n#EXTINF:6.4, no desc\nseg-279994985.ts\n#EXTINF:6.4, no desc\n../autre/seg-279994986.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6.4,Miles Davis - So What\n/abs/seg-279994987.ts\n";
        let Liste::Media(m) = lire(texte, BASE).unwrap() else {
            panic!("liste média attendue");
        };
        assert_eq!(m.cible, 6.0);
        assert!(!m.fin);
        let seq: Vec<u64> = m.segments.iter().map(|s| s.sequence).collect();
        assert_eq!(seq, [279_994_985, 279_994_986, 279_994_987]);
        assert_eq!(
            m.segments[0].uri,
            "https://cdn.example.net/live/station/seg-279994985.ts"
        );
        assert_eq!(
            m.segments[1].uri,
            "https://cdn.example.net/live/autre/seg-279994986.ts"
        );
        assert_eq!(
            m.segments[2].uri,
            "https://cdn.example.net/abs/seg-279994987.ts"
        );
        assert_eq!(m.segments[0].titre, None, "« no desc » n'est pas un titre");
        assert!(!m.segments[1].discontinuite);
        assert!(m.segments[2].discontinuite);
        assert_eq!(
            m.segments[2].titre,
            Some(TitreListe {
                titre: "So What".into(),
                artiste: Some("Miles Davis".into())
            })
        );
    }

    #[test]
    fn le_titre_a_attributs_est_lu() {
        assert_eq!(
            titre_extinf("title=\"Blue in Green\",artist=\"Bill Evans, trio\""),
            Some(TitreListe {
                titre: "Blue in Green".into(),
                artiste: Some("Bill Evans, trio".into())
            })
        );
    }

    #[test]
    fn les_variantes_sont_ordonnees_son_seul_puis_debit() {
        let v = |uri: &str, bande: u64, codecs: &str, video: bool| Variante {
            uri: uri.into(),
            bande,
            bande_moyenne: None,
            codecs: Some(codecs.into()),
            video,
            groupe_audio: None,
        };
        let variantes = vec![
            v("lc48", 56_000, "mp4a.40.2", false),
            v("video", 2_000_000, "avc1.4d401f,mp4a.40.2", true),
            v("he96", 101_760, "mp4a.40.5", false),
            v("ac3", 400_000, "ac-3", false),
            v("lc320", 340_000, "mp4a.40.2", false),
        ];
        let ordre: Vec<String> = ordre_des_variantes(&variantes, None)
            .into_iter()
            .map(|v| v.uri)
            .collect();
        assert_eq!(
            ordre,
            ["lc320", "he96", "lc48", "video"],
            "son seul d'abord, débit décroissant, AC-3 écarté"
        );
        let ordre: Vec<String> = ordre_des_variantes(&variantes, Some(128_000))
            .into_iter()
            .map(|v| v.uri)
            .collect();
        assert_eq!(ordre, ["he96", "lc48", "lc320", "video"]);
    }

    #[test]
    fn les_cles_sont_reconnues() {
        let texte = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n#EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\nb.ts\n";
        let Liste::Media(m) = lire(texte, BASE).unwrap() else {
            panic!("liste média attendue");
        };
        assert!(matches!(&m.segments[0].cle, Cle::Aes128 { iv: Some(iv), .. } if iv[15] == 0x0f));
        assert_eq!(m.segments[1].cle, Cle::Autre("SAMPLE-AES".into()));
    }

    #[test]
    fn un_texte_quelconque_n_est_pas_une_liste() {
        assert!(lire("<html></html>", BASE).is_err());
        assert!(lire("#EXTM3U\nhttp://radio.example/stream.mp3\n", BASE).is_err());
    }
}
