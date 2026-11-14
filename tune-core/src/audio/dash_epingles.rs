//! #8701 — les fMP4 DASH assemblés (`tune-dash-<uuid>.mp4`) des pistes qu'une
//! zone joue ou s'apprête à jouer restent servis par le cache d'URL de TIDAL
//! au-delà de son TTL de 240 s, et le ramasse-miettes ne les retire pas.
//!
//! Sans cela, un saut dans une piste qui joue depuis plus de 4 min ne
//! retrouvait plus le fichier : le cache l'avait oublié, et la piste entière
//! était retéléchargée (89 segments) avant la moindre note à la nouvelle
//! position, alors que le fichier complet était toujours sur le disque.
//!
//! Chaque zone garde au plus [`PAR_ZONE`] fichiers : la piste en cours et
//! celle que le gapless prépare. Une nouvelle piste résolue pour la zone
//! libère la plus ancienne (changement de piste). Une épingle oubliée (zone
//! arrêtée sans nouvelle piste) ne vaut plus au-delà de [`DUREE_MAX`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// La piste en cours et la suivante préparée par le gapless.
pub const PAR_ZONE: usize = 2;

/// Au-delà, une épingle ne retient plus le fichier : bien plus longue que
/// n'importe quelle piste, elle borne ce qu'une zone arrêtée laisse sur le
/// disque.
pub const DUREE_MAX: Duration = Duration::from_secs(4 * 3600);

struct Epingle {
    chemin: String,
    depuis: Instant,
}

fn registre() -> &'static Mutex<HashMap<i64, VecDeque<Epingle>>> {
    static R: OnceLock<Mutex<HashMap<i64, VecDeque<Epingle>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Le chemin du fichier DASH désigné par `url_ou_chemin` (`file://…`, avec ou
/// sans le suffixe `.decoding` de l'orchestrateur), s'il porte le nom que lui
/// donne TIDAL. `None` pour tout autre fichier ou URL.
fn chemin_dash(url_ou_chemin: &str) -> Option<&str> {
    let chemin = url_ou_chemin
        .strip_prefix("file://")
        .unwrap_or(url_ou_chemin);
    let chemin = chemin.strip_suffix(".decoding").unwrap_or(chemin);
    let nom = std::path::Path::new(chemin).file_name()?.to_str()?;
    (nom.starts_with("tune-dash-") && nom.ends_with(".mp4")).then_some(chemin)
}

/// La zone `zone_id` joue (ou prépare) le fichier DASH `url_ou_chemin` : il
/// est épinglé. Au-delà de [`PAR_ZONE`] fichiers pour la zone, le plus ancien
/// est libéré. Sans effet sur une URL qui n'est pas un fMP4 DASH de TIDAL.
pub fn epingler(zone_id: i64, url_ou_chemin: &str) {
    let Some(chemin) = chemin_dash(url_ou_chemin) else {
        return;
    };
    let mut r = registre().lock().unwrap_or_else(|e| e.into_inner());
    let file = r.entry(zone_id).or_default();
    file.retain(|e| e.chemin != chemin);
    file.push_back(Epingle {
        chemin: chemin.to_string(),
        depuis: Instant::now(),
    });
    while file.len() > PAR_ZONE {
        if let Some(libere) = file.pop_front() {
            tracing::debug!(zone_id, path = %libere.chemin, "dash_epingle_liberee_8701");
        }
    }
}

/// Une zone joue-t-elle (ou prépare-t-elle) ce fichier DASH ?
pub fn est_epingle(url_ou_chemin: &str) -> bool {
    let Some(chemin) = chemin_dash(url_ou_chemin) else {
        return false;
    };
    let r = registre().lock().unwrap_or_else(|e| e.into_inner());
    r.values()
        .flatten()
        .any(|e| e.chemin == chemin && e.depuis.elapsed() < DUREE_MAX)
}

/// Libère toutes les épingles de la zone.
pub fn liberer_la_zone(zone_id: i64) {
    registre()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&zone_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chemin(nom: &str) -> String {
        std::env::temp_dir()
            .join(format!("tune-dash-{nom}.mp4"))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn la_piste_en_cours_et_la_suivante_restent_epinglees_8701() {
        let zone = 870_101;
        let (a, b, c) = (chemin("ep-a"), chemin("ep-b"), chemin("ep-c"));
        epingler(zone, &format!("file://{a}"));
        assert!(est_epingle(&a));
        assert!(est_epingle(&format!("file://{a}")));
        assert!(est_epingle(&format!("{a}.decoding")));
        epingler(zone, &b);
        assert!(est_epingle(&a) && est_epingle(&b), "gapless : A et B");
        // Changement de piste : C est résolue, A est libérée.
        epingler(zone, &c);
        assert!(!est_epingle(&a), "A n'est plus jouée ni préparée");
        assert!(est_epingle(&b) && est_epingle(&c));
        // Re-résoudre B (un saut) la rafraîchit sans rien libérer d'autre.
        epingler(zone, &b);
        assert!(est_epingle(&b) && est_epingle(&c));
        liberer_la_zone(zone);
        assert!(!est_epingle(&b) && !est_epingle(&c));
    }

    #[test]
    fn seuls_les_fichiers_dash_de_tidal_s_epinglent_8701() {
        let zone = 870_102;
        epingler(zone, "https://cdn.example/piste.flac");
        epingler(zone, "file:///musique/album/piste.flac");
        assert!(!est_epingle("https://cdn.example/piste.flac"));
        assert!(!est_epingle("file:///musique/album/piste.flac"));
        liberer_la_zone(zone);
    }

    #[test]
    fn les_zones_ne_se_liberent_pas_entre_elles_8701() {
        let (z1, z2) = (870_103, 870_104);
        let a = chemin("ep-zones-a");
        epingler(z1, &a);
        for n in 0..5 {
            epingler(z2, &chemin(&format!("ep-zones-{n}")));
        }
        assert!(
            est_epingle(&a),
            "une autre zone ne libère pas le fichier de z1"
        );
        liberer_la_zone(z1);
        liberer_la_zone(z2);
    }
}
