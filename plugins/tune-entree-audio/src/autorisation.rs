//! L'autorisation macOS de capter une entrée audio (TCC).
//!
//! macOS traite TOUTE entrée audio comme un micro, même une interface USB ou
//! un périphérique virtuel. Sans autorisation, CoreAudio n'échoue pas : il
//! rend des ZÉROS. Un flux de silence sans explication est exactement ce qu'il
//! faut éviter, d'où deux signaux :
//!
//! * le statut que macOS tient pour le processus RESPONSABLE
//!   (`AVCaptureDevice.authorizationStatus(for: .audio)`) :
//!   - serveur lancé depuis un terminal : c'est l'app du terminal (iTerm2,
//!     Terminal) qui est autorisée ou non ;
//!   - serveur lancé par `launchd` ou par ssh : le binaire lui-même, sans
//!     `Info.plist` — le statut reste « jamais demandé » et la capture rend
//!     des zéros ;
//!   - l'app « Tune Server.app » : son bundle, qui doit porter
//!     `NSMicrophoneUsageDescription` (et le binaire signé, le droit
//!     `com.apple.security.device.audio-input`) pour que macOS pose la
//!     question ;
//! * le silence NUMÉRIQUE mesuré sur la capture (échantillons exactement
//!   nuls) : quand il dure et que le statut n'est pas « accordée », `/etat` le
//!   dit en clair, avec le réglage à ouvrir.
//!
//! Hors macOS, aucun contrôle de ce genre : l'autorisation est `accordee`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Autorisation {
    Accordee,
    Refusee,
    NonDemandee,
}

/// Le statut macOS, tel quel. Une fonction de test peut le remplacer.
pub type LireAutorisation = fn() -> Autorisation;

/// Au-delà, un silence numérique continu est suspect.
pub const SILENCE_SUSPECT_S: f64 = 3.0;

/// Le réglage à ouvrir, pour le message.
pub const REGLAGE: &str = "Réglages Système → Confidentialité et sécurité → Microphone";

/// Le diagnostic lisible, quand il y en a un.
pub fn diagnostic(
    autorisation: Autorisation,
    silence_numerique_s: f64,
    capture_active: bool,
    application: &str,
) -> Option<String> {
    match autorisation {
        Autorisation::Accordee => (capture_active && silence_numerique_s >= SILENCE_SUSPECT_S)
            .then(|| {
                format!(
                    "L'entrée rend un silence NUMÉRIQUE depuis {silence_numerique_s:.0} s (échantillons exactement nuls) : aucune source ne joue, ou la source est coupée."
                )
            }),
        Autorisation::Refusee => Some(format!(
            "macOS refuse l'accès aux entrées audio : la capture ne rend que des zéros. Autoriser « {application} » dans {REGLAGE}, puis relancer Tune."
        )),
        Autorisation::NonDemandee => Some(if capture_active && silence_numerique_s >= SILENCE_SUSPECT_S {
            format!(
                "macOS n'a jamais autorisé ce processus à capter une entrée audio : la capture ne rend que des zéros depuis {silence_numerique_s:.0} s. Lancer Tune depuis « Tune Server.app » (ou un terminal autorisé) et accepter la demande d'accès au micro, ou l'autoriser dans {REGLAGE}."
            )
        } else {
            format!(
                "macOS n'a pas encore autorisé ce processus à capter une entrée audio : la demande apparaîtra à la première capture, sinon l'autoriser dans {REGLAGE}."
            )
        }),
    }
}

/// Le statut que macOS tient pour ce processus.
pub fn du_systeme() -> Autorisation {
    #[cfg(all(target_os = "macos", feature = "capture"))]
    {
        macos::statut()
    }
    #[cfg(not(all(target_os = "macos", feature = "capture")))]
    {
        Autorisation::Accordee
    }
}

/// Le nom de l'application que macOS tient pour responsable, au mieux.
pub fn application_responsable() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(p) = std::env::current_exe() {
            let s = p.to_string_lossy().to_string();
            if let Some(i) = s.find(".app/") {
                let debut = s[..i].rfind('/').map(|j| j + 1).unwrap_or(0);
                return s[debut..i + 4].to_string();
            }
        }
        if let Ok(t) = std::env::var("TERM_PROGRAM") {
            return match t.as_str() {
                "iTerm.app" => "iTerm".into(),
                "Apple_Terminal" => "Terminal".into(),
                autre => autre.into(),
            };
        }
        return "tune-server".into();
    }
    "tune-server".into()
}

#[cfg(all(target_os = "macos", feature = "capture"))]
mod macos {
    //! `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`,
    //! par le runtime Objective-C, sans caisse supplémentaire.
    use std::ffi::{c_char, c_void};

    use super::Autorisation;

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(nom: *const c_char) -> *mut c_void;
        fn sel_registerName(nom: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    pub fn statut() -> Autorisation {
        // SAFETY: la classe et le sélecteur existent depuis macOS 10.14 ;
        // `objc_msgSend` est appelé avec la signature exacte de la méthode
        // (id, SEL, NSString*) -> NSInteger.
        let brut = unsafe {
            let classe = objc_getClass(c"AVCaptureDevice".as_ptr());
            if classe.is_null() {
                return Autorisation::NonDemandee;
            }
            let sel = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
            let envoyer: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> isize =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            envoyer(classe, sel, AVMediaTypeAudio)
        };
        // AVAuthorizationStatus : 0 notDetermined, 1 restricted, 2 denied,
        // 3 authorized.
        match brut {
            3 => Autorisation::Accordee,
            1 | 2 => Autorisation::Refusee,
            _ => Autorisation::NonDemandee,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_refus_est_dit_avec_le_reglage_a_ouvrir() {
        let d = diagnostic(Autorisation::Refusee, 0.0, true, "Tune Server.app").unwrap();
        assert!(d.contains("zéros"), "{d}");
        assert!(d.contains("Tune Server.app"), "{d}");
        assert!(d.contains("Microphone"), "{d}");
    }

    #[test]
    fn jamais_demandee_et_des_zeros_est_dit_comme_tel() {
        let d = diagnostic(Autorisation::NonDemandee, 5.0, true, "tune-server").unwrap();
        assert!(d.contains("jamais autorisé"), "{d}");
        assert!(d.contains("5 s"), "{d}");
    }

    #[test]
    fn accordee_ne_dit_rien_tant_que_le_signal_vit() {
        assert_eq!(diagnostic(Autorisation::Accordee, 0.2, true, "x"), None);
        let d = diagnostic(Autorisation::Accordee, 10.0, true, "x").unwrap();
        assert!(d.contains("silence NUMÉRIQUE"), "{d}");
    }

    #[test]
    fn le_statut_se_serialise_comme_l_etat_l_annonce() {
        assert_eq!(
            serde_json::to_value(Autorisation::NonDemandee).unwrap(),
            "non_demandee"
        );
        assert_eq!(
            serde_json::to_value(Autorisation::Refusee).unwrap(),
            "refusee"
        );
        assert_eq!(
            serde_json::to_value(Autorisation::Accordee).unwrap(),
            "accordee"
        );
    }

    fn racine() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// L'app macOS ne peut OBTENIR l'autorisation que si son bundle dit
    /// pourquoi (`NSMicrophoneUsageDescription`, sans quoi macOS ne pose même
    /// pas la question) et si le binaire signé en runtime durci porte le droit
    /// `com.apple.security.device.audio-input` (sans quoi l'accès est refusé
    /// en silence : des zéros).
    #[test]
    fn le_bundle_macos_declare_l_entree_audio() {
        let yml = std::fs::read_to_string(racine().join(".github/workflows/release.yml")).unwrap();
        let debut = yml
            .find("cat > \"$APP/Contents/Info.plist\" << PLIST")
            .expect("heredoc Info.plist");
        let plist = &yml[debut..debut + yml[debut..].find("\n          PLIST").unwrap()];
        let cle = "<key>NSMicrophoneUsageDescription</key><string>";
        let pos = plist
            .find(cle)
            .expect("NSMicrophoneUsageDescription absente du bundle");
        let texte = &plist[pos + cle.len()..];
        assert!(texte.find("</string>").unwrap() > 20, "description vide");

        let droits =
            std::fs::read_to_string(racine().join("packaging/macos/tune-server.entitlements"))
                .unwrap();
        let cle = "<key>com.apple.security.device.audio-input</key>";
        let pos = droits.find(cle).expect("droit audio-input absent");
        assert!(
            droits[pos + cle.len()..]
                .trim_start()
                .starts_with("<true/>")
        );
    }
}
