//! Repli conservateur quand un renderer REFUSE `SetAVTransportURI`.
//!
//! Un refus applicatif `501 Action Failed`, `714 Illegal MIME-type` ou
//! `716 Resource not found` ne dit pas forcément « je ne sais pas lire ce
//! flux » : il dit souvent « je n'aime pas la façon dont tu me l'annonces ».
//! Le cas d'école est la pile `upmpdcli` (libupnp) : avec son contrôle
//! `checkcontentformat` actif, elle compare le MIME du `protocolInfo` de la
//! DIDL à sa propre liste, et tout écart remonte en `501 Action Failed`, le
//! code générique de libupnp pour une action qui a échoué. Le même flux,
//! annoncé dans l'orthographe que le renderer publie dans son
//! `GetProtocolInfo` → `Sink`, passe.
//!
//! La reprise 714 existait déjà dans `play_media` (orthographe exacte du Sink,
//! puis étiquette PCM). Ce module y ajoute :
//!
//! - le classement de la faute (`501`, `714`, `716`, autre) ;
//! - la MÉMOIRE par appareil (UDN) et par MIME source du profil qui a fini par
//!   passer, pour ne pas repayer les refus à chaque piste ni sur le gapless ;
//! - l'extraction du `protocolInfo` émis, pour que chaque essai se lise au
//!   journal avec ce qui a été envoyé et ce qui a été répondu.
//!
//! Aucune boucle : deux replis au plus par pose d'URI, décidés par
//! `play_media`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Ce que le renderer a répondu à un `SetAVTransportURI` refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FauteSetUri {
    /// `714 Illegal MIME-type` : le MIME annoncé n'est pas dans son Sink.
    MimeIllegal714,
    /// `501 Action Failed` : échec générique — upmpdcli/libupnp y range le
    /// refus de son contrôle de format.
    ActionEchouee501,
    /// `716 Resource not found` : certains renderers y rangent une ressource
    /// dont ils refusent le format annoncé.
    RessourceIntrouvable716,
    /// Toute autre faute : pas de repli de profil, l'erreur remonte telle
    /// quelle (701, 402…, qui ont leur propre sens).
    Autre,
}

impl FauteSetUri {
    pub(crate) fn code(self) -> &'static str {
        match self {
            FauteSetUri::MimeIllegal714 => "714",
            FauteSetUri::ActionEchouee501 => "501",
            FauteSetUri::RessourceIntrouvable716 => "716",
            FauteSetUri::Autre => "autre",
        }
    }
}

/// Classe la réponse d'un `SetAVTransportURI` refusé. Lit le `errorCode`
/// UPnP ; à défaut, la description `Illegal MIME` (forme historique de la
/// reprise 714).
pub(crate) fn classer_faute_set_uri(reponse: &str) -> FauteSetUri {
    let code = super::dlna::extract_tag(reponse, "errorCode")
        .map(|c| c.trim().to_string())
        .unwrap_or_default();
    match code.as_str() {
        "714" => FauteSetUri::MimeIllegal714,
        "501" => FauteSetUri::ActionEchouee501,
        "716" => FauteSetUri::RessourceIntrouvable716,
        _ if reponse.to_ascii_lowercase().contains("illegal mime") => FauteSetUri::MimeIllegal714,
        _ => FauteSetUri::Autre,
    }
}

/// Le profil d'annonce qui a fini par passer sur un appareil pour un MIME
/// source donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfilSetUri {
    /// MIME à annoncer dans la DIDL à la place du MIME source.
    pub mime_annonce: String,
    /// Niveau de DIDL minimal à employer (0 complet, 1 minimal, 2 vide).
    pub niveau_didl_min: u8,
    /// Le MIME annoncé est un alias du format servi (mêmes octets) : il est
    /// aussi servi en `Content-Type` (#4958). Faux pour l'étiquette PCM.
    pub servi_sous_ce_mime: bool,
}

/// Mémoire process : (UDN, MIME source en minuscules) → profil appris.
///
/// Process et non champ de la sortie : une sortie DLNA peut être recréée
/// (redécouverte, changement de zone) sans que l'appareil ait changé de pile
/// UPnP. L'UDN, lui, ne bouge pas.
static PROFILS: LazyLock<Mutex<HashMap<(String, String), ProfilSetUri>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cle(udn: &str, mime_source: &str) -> (String, String) {
    (udn.to_string(), mime_source.trim().to_ascii_lowercase())
}

pub(crate) fn profil_memorise(udn: &str, mime_source: &str) -> Option<ProfilSetUri> {
    PROFILS
        .lock()
        .ok()
        .and_then(|m| m.get(&cle(udn, mime_source)).cloned())
}

pub(crate) fn memoriser_profil(udn: &str, mime_source: &str, profil: ProfilSetUri) {
    if let Ok(mut m) = PROFILS.lock() {
        m.insert(cle(udn, mime_source), profil);
    }
}

pub(crate) fn oublier_profil(udn: &str, mime_source: &str) {
    if let Ok(mut m) = PROFILS.lock() {
        m.remove(&cle(udn, mime_source));
    }
}

/// Le `protocolInfo` de la première ressource d'une DIDL, échappée (telle
/// qu'elle part dans `CurrentURIMetaData`) ou non. `None` pour une DIDL vide
/// ou sans `protocolInfo`. Sert au journal : chaque essai dit ce qu'il a
/// envoyé.
pub(crate) fn protocol_info_du_didl(metadata: &str) -> Option<String> {
    for (ouverture, fermeture) in [("protocolInfo=&quot;", "&quot;"), ("protocolInfo=\"", "\"")] {
        if let Some(debut) = metadata.find(ouverture) {
            let reste = &metadata[debut + ouverture.len()..];
            let fin = reste.find(fermeture)?;
            return Some(reste[..fin].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faute(code: u16, description: &str) -> String {
        format!(
            "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
        )
    }

    #[test]
    fn classe_les_trois_fautes_de_format_et_laisse_les_autres() {
        assert_eq!(
            classer_faute_set_uri(&faute(501, "Action Failed")),
            FauteSetUri::ActionEchouee501
        );
        assert_eq!(
            classer_faute_set_uri(&faute(714, "Illegal MIME-type")),
            FauteSetUri::MimeIllegal714
        );
        assert_eq!(
            classer_faute_set_uri(&faute(716, "Resource not found")),
            FauteSetUri::RessourceIntrouvable716
        );
        // 701 « Transition not available » a sa propre reprise ; 402 dit
        // « arguments invalides » : ni l'un ni l'autre n'appelle un repli.
        assert_eq!(
            classer_faute_set_uri(&faute(701, "Transition not available")),
            FauteSetUri::Autre
        );
        assert_eq!(
            classer_faute_set_uri(&faute(402, "Invalid Args")),
            FauteSetUri::Autre
        );
        // Forme historique sans code : la description suffit pour le 714.
        assert_eq!(
            classer_faute_set_uri("<UPnPError>Illegal MIME-Type</UPnPError>"),
            FauteSetUri::MimeIllegal714
        );
    }

    #[test]
    fn la_memoire_est_par_appareil_et_par_mime_source() {
        let udn = "uuid:test-memoire-repli-set-uri";
        let profil = ProfilSetUri {
            mime_annonce: "audio/x-flac".into(),
            niveau_didl_min: 1,
            servi_sous_ce_mime: true,
        };
        memoriser_profil(udn, "audio/FLAC", profil.clone());
        assert_eq!(profil_memorise(udn, "audio/flac"), Some(profil));
        assert_eq!(profil_memorise(udn, "audio/mpeg"), None);
        assert_eq!(profil_memorise("uuid:autre-appareil", "audio/flac"), None);
        oublier_profil(udn, "audio/flac");
        assert_eq!(profil_memorise(udn, "audio/flac"), None);
    }

    #[test]
    fn lit_le_protocol_info_echappe_ou_non() {
        let echappe = "&lt;res protocolInfo=&quot;http-get:*:audio/flac:DLNA.ORG_OP=01&quot; duration=&quot;0:01:00.000&quot;&gt;";
        assert_eq!(
            protocol_info_du_didl(echappe).as_deref(),
            Some("http-get:*:audio/flac:DLNA.ORG_OP=01")
        );
        let brut = r#"<res protocolInfo="http-get:*:audio/wav:*">"#;
        assert_eq!(
            protocol_info_du_didl(brut).as_deref(),
            Some("http-get:*:audio/wav:*")
        );
        assert_eq!(protocol_info_du_didl(""), None);
    }
}
