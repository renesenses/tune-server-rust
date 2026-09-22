//! **Ce qu'on ANNONCE au renderer pour une piste de serveur média.**
//!
//! # Le défaut
//!
//! Sur le `.18` en 0.9.161, une piste venue d'un serveur UPnP partait vers
//! l'Eversolo DMP-A8 et jouait du **SILENCE** : le compteur avançait, le
//! bandeau affichait « FLAC 44.1/24 », et rien ne sortait. Aucune erreur,
//! aucun voyant.
//!
//! La chaîne était celle-ci :
//!
//! 1. `resolve_direct` calculait le MIME avec `guess_mime_from_url(audio_url)`
//!    et rien d'autre ;
//! 2. cette aide retombe sur `audio/mpeg` quand l'URL ne porte **aucune
//!    extension reconnue** (`bandcamp::guess_mime_from_url`) ;
//! 3. or le serveur média de Tune publie ses `<res>` SANS extension —
//!    `http://hôte:8888/api/v1/library/tracks/<id>/audio`
//!    ([`crate::upnp_server::track_audio_url`]) —, et le réseau du mainteneur
//!    contient d'autres instances de Tune vues comme serveurs médias ;
//! 4. `audio/mpeg` devient `DLNA.ORG_PN=MP3` dans le `protocolInfo` envoyé au
//!    renderer (`outputs/didl.rs`) ;
//! 5. et un `DLNA.ORG_PN` FAUX fait rabattre le flux sur le profil déclaré :
//!    l'appareil lit du FLAC comme si c'était du MP3, et joue du silence —
//!    le mécanisme déjà écrit en toutes lettres dans `outputs/dlna.rs`
//!    (`didl_metadata_minimale`, #1137 / #1458 / #2394).
//!
//! Le bandeau, lui, ne se trompait pas : il lit `PlayRequest.media_format`,
//! c'est-à-dire le codec du `protocolInfo` DIDL. **Tune savait donc que
//! c'était du FLAC au moment même où il annonçait du MP3.** L'écart entre les
//! deux est exactement le défaut.
//!
//! Même classe que le `.flc` de Lyrion (`bandcamp.rs`), à ceci près que là-bas
//! la correction tenait dans une extension de plus ; ici, l'URL ne nomme
//! **rien** et ne le nommera jamais.
//!
//! # Ce que ce module fait
//!
//! Il ordonne les trois choses que Tune peut savoir, de la plus sûre à la
//! moins sûre, et **n'invente pas la quatrième** :
//!
//! 1. l'**extension de l'URL**, quand il y en a une. Elle décrit les octets
//!    qui vont réellement être servis ; le comportement d'avant est intact
//!    pour toute URL qui en porte une ;
//! 2. le **format DIT PAR L'APPELANT** — `PlayRequest.media_format`, lu par le
//!    client dans le `res@protocolInfo` du DIDL au moment du parcours. C'est
//!    le serveur média lui-même qui parle, en direct ;
//! 3. le **format INDEXÉ** — `tracks.format`, que l'indexation tire du même
//!    `protocolInfo` (`routes/indexation_upnp.rs::PisteDistante::format`) et
//!    range sur la ligne. C'est ce qui reste quand la lecture vient d'une file
//!    ou d'une playlist, où plus personne ne porte le DIDL ;
//! 4. faute de tout cela, [`MIME_PAR_DEFAUT`]. On ne sait pas — et on ne fait
//!    alors pas pire qu'avant.
//!
//! Aucune sonde, aucun octet lu : ce module ne fait que relire ce que deux
//! chemins écrivaient déjà, et que la lecture ne relisait pas.

use super::bandcamp::{MIME_PAR_DEFAUT, mime_depuis_l_extension};

/// Le MIME d'un format tel que le `protocolInfo` d'un serveur média le nomme.
///
/// L'entrée est la forme RÉDUITE que l'indexation range dans `tracks.format` —
/// le sous-type du MIME, `x-` retiré et en minuscules : `audio/x-flac` →
/// `flac`. Les mêmes étiquettes arrivent par `PlayRequest.media_format`, que
/// le client remplit depuis le même attribut.
///
/// Rend `None` sur ce qu'on ne sait pas traduire : un format inconnu doit
/// laisser l'appelant retomber sur son défaut, et surtout pas se voir
/// fabriquer un `audio/<n'importe quoi>` que le calcul de `DLNA.ORG_PN`
/// interpréterait à sa façon.
pub(super) fn mime_depuis_le_format(format: &str) -> Option<&'static str> {
    match format
        .trim()
        .trim_start_matches("x-")
        .to_lowercase()
        .as_str()
    {
        "flac" => Some("audio/flac"),
        "wav" | "wave" | "vnd.wave" => Some("audio/wav"),
        "aiff" | "aif" | "aiifc" => Some("audio/aiff"),
        // `alac` et `aac` partagent le conteneur MP4 : c'est `audio/mp4` qui
        // part au renderer dans les deux cas. La distinction ALAC/AAC n'est PAS
        // perdue pour autant — elle vit dans `media_format`, que le bandeau lit
        // séparément (`transport.rs`, NAS d'Yves).
        "mp4" | "m4a" | "aac" | "alac" => Some("audio/mp4"),
        "mpeg" | "mp3" | "mpeg3" | "mpg" => Some("audio/mpeg"),
        "ogg" | "opus" | "vorbis" => Some("audio/ogg"),
        // DSD : `didl.rs` sait déjà qu'un DSD sort SANS `DLNA.ORG_PN`, ce qui
        // est le seul comportement juste. Le nommer ici évite qu'un `.dsf`
        // servi sans extension parte, lui aussi, étiqueté MP3.
        "dsd" | "dsf" | "dff" => Some("application/x-dsd"),
        _ => None,
    }
}

/// Le MIME à ANNONCER pour une piste de serveur média (`source = "upnp"`).
///
/// L'ordre des trois sources est celui du module ; il n'est pas indifférent, et
/// le premier terme est ce qui garantit l'absence de régression : une URL qui
/// porte `.mp3`, `.flac` ou `.wav` donne exactement ce qu'elle donnait avant.
pub(super) fn mime_d_une_piste_upnp(
    url: &str,
    format_dit_par_l_appelant: Option<&str>,
    format_indexe: Option<&str>,
) -> &'static str {
    mime_depuis_l_extension(url)
        .or_else(|| format_dit_par_l_appelant.and_then(mime_depuis_le_format))
        .or_else(|| format_indexe.and_then(mime_depuis_le_format))
        .unwrap_or(MIME_PAR_DEFAUT)
}

#[cfg(test)]
mod tests {
    use super::{mime_d_une_piste_upnp, mime_depuis_le_format};
    use crate::orchestrator::bandcamp::{guess_mime_from_url, mime_depuis_l_extension};

    /// L'URL exacte que le serveur média de Tune publie dans son `<res>`.
    const RES_SANS_EXTENSION: &str = "http://192.168.1.42:8888/api/v1/library/tracks/21825/audio";

    /// **La racine du défaut, isolée.** Tant que « l'URL dit MP3 » et « l'URL
    /// ne dit rien » rendaient la même chose, aucun appelant ne pouvait
    /// corriger le second sans écraser le premier.
    #[test]
    fn l_extension_absente_se_distingue_de_l_extension_mp3() {
        assert_eq!(mime_depuis_l_extension(RES_SANS_EXTENSION), None);
        assert_eq!(
            mime_depuis_l_extension("http://nas/x.mp3"),
            Some("audio/mpeg")
        );
        // Le défaut historique est conservé pour les appelants qui n'ont rien
        // d'autre à dire : la bascule est un ÉLARGISSEMENT, pas un changement.
        assert_eq!(guess_mime_from_url(RES_SANS_EXTENSION), "audio/mpeg");
    }

    /// Le cas du mainteneur : `<res>` sans extension, ligne indexée en `flac`.
    #[test]
    fn une_res_sans_extension_indexee_en_flac_s_annonce_en_flac() {
        assert_eq!(
            mime_d_une_piste_upnp(RES_SANS_EXTENSION, None, Some("flac")),
            "audio/flac",
            "l'URL ne nomme rien, mais la LIGNE sait : annoncer MP3 ici fait \
             jouer du silence à l'Eversolo"
        );
    }

    /// Le parcours « Serveurs média » : pas de ligne indexée, mais le DIDL est
    /// en main — le client le repasse dans `media_format`.
    #[test]
    fn le_format_dit_par_l_appelant_sert_quand_la_ligne_est_muette() {
        assert_eq!(
            mime_d_une_piste_upnp(RES_SANS_EXTENSION, Some("flac"), None),
            "audio/flac"
        );
        // `audio/x-flac` est la forme que publient la plupart des serveurs :
        // elle doit passer comme `flac`.
        assert_eq!(
            mime_d_une_piste_upnp(RES_SANS_EXTENSION, Some("x-flac"), None),
            "audio/flac"
        );
    }

    /// **La non-régression.** Une URL qui NOMME son format garde le dernier
    /// mot, y compris contre un `media_format` qui dirait autre chose : c'est
    /// l'extension qui décrit les octets réellement servis.
    #[test]
    fn une_url_qui_nomme_son_format_garde_le_dernier_mot() {
        assert_eq!(
            mime_d_une_piste_upnp(
                "http://nas:8200/MediaItems/7391.mp3",
                Some("flac"),
                Some("flac")
            ),
            "audio/mpeg"
        );
        assert_eq!(
            mime_d_une_piste_upnp("http://nas:8200/MediaItems/7391.flac", None, None),
            "audio/flac"
        );
        assert_eq!(
            mime_d_une_piste_upnp("http://nas:8200/MediaItems/7391.wav", None, Some("flac")),
            "audio/wav"
        );
    }

    /// Rien de su ⇒ rien de changé : le défaut d'avant, ni mieux ni pire.
    #[test]
    fn sans_rien_de_su_le_defaut_reste_le_defaut() {
        assert_eq!(
            mime_d_une_piste_upnp(RES_SANS_EXTENSION, None, None),
            "audio/mpeg"
        );
        assert_eq!(
            mime_d_une_piste_upnp(RES_SANS_EXTENSION, Some("ape"), Some("wma")),
            "audio/mpeg",
            "un format qu'on ne sait pas traduire ne doit pas fabriquer un MIME"
        );
    }

    /// Le tableau de traduction, et ce qu'il refuse.
    #[test]
    fn le_format_se_traduit_ou_se_tait() {
        for (format, mime) in [
            ("flac", "audio/flac"),
            ("x-flac", "audio/flac"),
            ("FLAC", "audio/flac"),
            ("wav", "audio/wav"),
            ("wave", "audio/wav"),
            ("aiff", "audio/aiff"),
            ("alac", "audio/mp4"),
            ("aac", "audio/mp4"),
            ("m4a", "audio/mp4"),
            ("mp3", "audio/mpeg"),
            ("mpeg", "audio/mpeg"),
            ("opus", "audio/ogg"),
            ("dsf", "application/x-dsd"),
        ] {
            assert_eq!(
                mime_depuis_le_format(format),
                Some(mime),
                "« {format} » devrait donner {mime}"
            );
        }
        for format in ["", "   ", "inconnu", "ape", "wma", "octet-stream"] {
            assert_eq!(
                mime_depuis_le_format(format),
                None,
                "« {format} » ne doit rien affirmer"
            );
        }
    }
}

/// **Le défaut de bout en bout**, par la porte que l'auditeur emprunte.
///
/// Les témoins ci-dessus gardent une fonction pure ; ceux-ci gardent le
/// CHEMIN : une ligne en base, `resolve_stream`, et ce qui part réellement au
/// renderer. Sans eux, la fonction pourrait rester juste et n'être appelée par
/// personne — « écrit mais pas branché ».
#[cfg(test)]
mod temoins_du_chemin {
    use crate::db::backend::DbBackend;
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::http::streamer::AudioStreamer;
    use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
    use crate::outputs::didl::dlna_flags_for_mime_bd_sr;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::PlaybackManager;
    use crate::streaming::registry::ServiceRegistry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const ID: i64 = 62_201;
    /// Ce que le serveur média de Tune publie dans son `<res>` : aucune
    /// extension (`upnp_server::track_audio_url`).
    const RES: &str = "http://192.168.1.42:8888/api/v1/library/tracks/21825/audio";

    /// Un orchestrateur dont la base porte UNE piste de serveur média : sa
    /// source, son format indexé, et son URL de lecture dans l'instantané —
    /// les trois choses que l'indexation écrit (#2219, phase 2).
    fn orchestrateur_avec_une_piste_upnp(format: Option<&str>, res: &str) -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let format = match format {
            Some(f) => format!("'{f}'"),
            None => "NULL".to_string(),
        };
        db.execute_batch(&format!(
            "INSERT INTO tracks (id,title,source,source_id,format) \
             VALUES ({ID},'Une piste du NAS','upnp','uuid:258FC2D5-E2C3-B734-0-1|85944171f73967e8',{format}); \
             INSERT INTO track_metadata (track_id,key,value) \
             VALUES ({ID},'upnp_res_url','{res}');"
        ))
        .unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    /// La demande telle que l'envoient le bouton Lecture, l'avance de file et
    /// la reprise : un `track_id`, et RIEN d'autre. Ni `source`, ni
    /// `media_format` — c'est précisément le cas où le DIDL n'est plus en main
    /// et où seule la ligne sait.
    fn demande_par_track_id(zone_id: i64) -> PlayRequest {
        PlayRequest {
            zone_id,
            output_device_id: Some("dlna:renderer-1".into()),
            track_id: Some(ID),
            source: None,
            source_id: None,
            title: Some("Une piste du NAS".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: Some(44_100),
            bit_depth: Some(24),
            media_format: None,
            track_number: None,
            disc_number: None,
        }
    }

    /// **Le défaut du mainteneur, en un test.** `<res>` sans extension, ligne
    /// indexée en `flac` : le renderer doit s'entendre annoncer du FLAC, et le
    /// `protocolInfo` qui en découle ne doit porter AUCUN profil MP3.
    ///
    /// Sabotage : remettre `guess_mime_from_url(audio_url)` dans la branche
    /// `upnp` de `resolve_direct.rs` fait rougir ce test sur `audio/mpeg`.
    #[tokio::test]
    async fn une_piste_upnp_indexee_en_flac_est_annoncee_en_flac() {
        let orch = orchestrateur_avec_une_piste_upnp(Some("flac"), RES);
        let resolved = orch.resolve_stream(&demande_par_track_id(1)).await.unwrap();

        assert_eq!(resolved.source, "upnp");
        assert_eq!(resolved.url, RES, "l'URL indexée doit partir telle quelle");
        assert_eq!(
            resolved.mime_type, "audio/flac",
            "l'URL ne porte aucune extension : le MIME doit venir du format \
             INDEXÉ. Annoncer « audio/mpeg » ici, c'est annoncer \
             DLNA.ORG_PN=MP3 à l'Eversolo — et jouer du silence"
        );

        // Ce que la sortie DLNA en fait réellement : le protocolInfo.
        let protocol_info = dlna_flags_for_mime_bd_sr(
            &resolved.mime_type,
            resolved.bit_depth,
            resolved.sample_rate,
        );
        assert!(
            !protocol_info.contains("DLNA.ORG_PN=MP3"),
            "le renderer ne doit plus recevoir un profil MP3 pour du FLAC : {protocol_info}"
        );
        assert!(
            !protocol_info.contains("DLNA.ORG_PN="),
            "du FLAC s'annonce SANS profil : le renderer lit alors les octets \
             réels au lieu d'une promesse ({protocol_info})"
        );
        // La contre-épreuve du témoin lui-même : ce même calcul, nourri du
        // MIME d'AVANT, produit bien le profil qui tue le son. Sans cette
        // ligne, l'assertion ci-dessus pourrait passer pour une tautologie.
        assert!(
            dlna_flags_for_mime_bd_sr("audio/mpeg", Some(24), Some(44_100))
                .contains("DLNA.ORG_PN=MP3"),
            "c'est bien « audio/mpeg » qui fabriquait le profil MP3"
        );
    }

    /// **La non-régression.** Une piste de serveur média dont l'URL porte une
    /// vraie extension `.mp3` continue de s'annoncer `audio/mpeg` — le format
    /// indexé ne vient PAS contredire les octets servis.
    #[tokio::test]
    async fn une_url_en_mp3_reste_annoncee_en_mp3() {
        const RES_MP3: &str = "http://192.168.1.42:8200/MediaItems/7391.mp3";
        let orch = orchestrateur_avec_une_piste_upnp(Some("mp3"), RES_MP3);
        let resolved = orch.resolve_stream(&demande_par_track_id(1)).await.unwrap();
        assert_eq!(resolved.mime_type, "audio/mpeg");
        assert!(
            dlna_flags_for_mime_bd_sr(
                &resolved.mime_type,
                resolved.bit_depth,
                resolved.sample_rate
            )
            .contains("DLNA.ORG_PN=MP3"),
            "du vrai MP3 garde son profil MP3"
        );
    }

    /// Une ligne dont le serveur n'a jamais dit le format garde EXACTEMENT le
    /// comportement d'avant : on ne sait pas, on ne prétend pas savoir.
    #[tokio::test]
    async fn sans_format_indexe_rien_ne_change() {
        let orch = orchestrateur_avec_une_piste_upnp(None, RES);
        let resolved = orch.resolve_stream(&demande_par_track_id(1)).await.unwrap();
        assert_eq!(resolved.mime_type, "audio/mpeg");
    }

    /// **Le parcours « Serveurs média ».** Pas de ligne en base : l'adresse et
    /// le codec viennent en direct du DIDL, le client repassant ce dernier dans
    /// `media_format`. C'est la MÊME fonction, donc le même correctif.
    #[tokio::test]
    async fn le_parcours_direct_annonce_le_codec_du_didl() {
        let orch = orchestrateur_avec_une_piste_upnp(None, RES);
        let mut req = demande_par_track_id(1);
        req.track_id = None;
        req.source = Some("upnp".into());
        req.source_id = Some(RES.into());
        req.media_format = Some("flac".into());
        let resolved = orch.resolve_stream(&req).await.unwrap();
        assert_eq!(
            resolved.mime_type, "audio/flac",
            "le codec lu dans le res@protocolInfo doit primer sur un défaut \
             qui n'affirme rien"
        );
    }
}
