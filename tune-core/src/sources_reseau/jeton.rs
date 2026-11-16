//! Le jeton de la passerelle HTTP des sources distantes (#8189).
//!
//! Un renderer (DLNA, AirPlay, Chromecast) lit une URL, pas un chemin, et ne
//! sait présenter aucun identifiant. La passerelle lui sert donc le fichier
//! distant sous une URL qui porte un jeton :
//! - PROPRE à un fichier (source et chemin signés ensemble) : il n'ouvre
//!   rien d'autre, et ne permet aucun listage ;
//! - COURT : il expire (quelques heures au plus) ;
//! - signé par HMAC-SHA256 sous une clé tirée au démarrage du processus et
//!   jamais écrite : un redémarrage invalide tous les jetons.

use std::sync::OnceLock;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Durée de vie par défaut d'un jeton : le temps d'un long morceau, pause
/// comprise.
pub const DUREE_PAR_DEFAUT: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

fn cle() -> &'static [u8; 32] {
    static CLE: OnceLock<[u8; 32]> = OnceLock::new();
    CLE.get_or_init(|| {
        let mut c = [0u8; 32];
        // Sans aléa du système, une clé fixe ouvrirait la passerelle à tous :
        // on préfère un serveur qui ne sert plus rien.
        getrandom::getrandom(&mut c).expect("aléa du système indisponible");
        c
    })
}

fn signature(source: &str, chemin: &str, expire: u64) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(cle()).expect("clé HMAC de 32 octets");
    mac.update(source.as_bytes());
    mac.update(b"\n");
    mac.update(chemin.as_bytes());
    mac.update(b"\n");
    mac.update(expire.to_string().as_bytes());
    mac
}

fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Un jeton pour `chemin` (interne) de la source `source`, valable `duree`.
pub fn signer(source: &str, chemin: &str, duree: std::time::Duration) -> String {
    signer_a(source, chemin, maintenant() + duree.as_secs())
}

fn signer_a(source: &str, chemin: &str, expire: u64) -> String {
    let sig = signature(source, chemin, expire).finalize().into_bytes();
    format!(
        "{expire}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
    )
}

/// Le jeton vaut-il pour ce fichier, maintenant ? Comparaison en temps
/// constant.
pub fn verifier(source: &str, chemin: &str, jeton: &str) -> bool {
    verifier_a(source, chemin, jeton, maintenant())
}

fn verifier_a(source: &str, chemin: &str, jeton: &str, instant: u64) -> bool {
    let Some((expire, sig)) = jeton.split_once('.') else {
        return false;
    };
    let Ok(expire) = expire.parse::<u64>() else {
        return false;
    };
    if expire < instant {
        return false;
    }
    let Ok(sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig) else {
        return false;
    };
    signature(source, chemin, expire).verify_slice(&sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_jeton_n_ouvre_que_son_fichier_et_expire() {
        let j = signer_a("nas", "Album/01.flac", 2_000);
        assert!(verifier_a("nas", "Album/01.flac", &j, 1_000));
        assert!(
            !verifier_a("nas", "Album/02.flac", &j, 1_000),
            "autre fichier"
        );
        assert!(
            !verifier_a("autre", "Album/01.flac", &j, 1_000),
            "autre source"
        );
        assert!(!verifier_a("nas", "Album/01.flac", &j, 2_001), "expiré");
        let (e, s) = j.split_once('.').unwrap();
        let allonge = format!("{}.{s}", e.parse::<u64>().unwrap() + 10_000);
        assert!(
            !verifier_a("nas", "Album/01.flac", &allonge, 1_000),
            "date forgée"
        );
        assert!(!verifier_a("nas", "Album/01.flac", "n'importe quoi", 1_000));
        assert!(!verifier_a("nas", "Album/01.flac", "", 1_000));
    }
}
