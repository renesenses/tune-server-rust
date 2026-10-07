//! #2211 — **le fondu enchaîné vu de la zone** : le réglage (une durée, `0` =
//! désactivé) et la consigne que l'orchestrateur remet à la sortie locale pour
//! chaque frontière entre deux pistes.
//!
//! Le moteur ([`super::fondu_enchaine`]) ne voit que des échantillons ; il ne
//! sait pas si deux pistes viennent du même album, ni si cet album est un live.
//! L'orchestrateur, lui, le sait au moment où il arme la piste suivante. Il
//! le dit par une [`ConsigneDeJonction`], et la sortie tranche le reste sur le
//! signal (la queue sortante finit-elle sur un blanc ?).
//!
//! # Les règles
//!
//! * durée de 0 à [`DUREE_MAX_S`] secondes, `0` par défaut : sans geste de
//!   l'utilisateur, rien ne change ;
//! * jamais en PURE ni en bit-perfect strict : la durée appliquée vaut `0` ;
//! * jamais entre deux pistes d'un même album live : le gapless prime ;
//! * entre deux pistes d'un même album non live, seulement si la sortante
//!   finit sur un blanc — un album « sans blanc » enchaîne en gapless ;
//! * jamais avec un DSD de part ou d'autre : un porteur DoP additionné perd
//!   son marqueur et le DAC se tait.

use std::sync::Arc;

use super::fondu_enchaine::{ConsigneDeJonction, DUREE_MAX_S, MotifSansFondu};
use crate::db::backend::DbBackend;

/// La clé de réglage de la durée, en secondes (`0` = désactivé).
#[must_use]
pub fn cle_de_duree(zone_id: i64) -> String {
    format!("zone_{zone_id}_crossfade_s")
}

/// Une durée demandée est-elle acceptable ? `Some(secondes)` dans
/// `[0, DUREE_MAX_S]`, `None` sinon (négative, trop longue, NaN).
#[must_use]
pub fn duree_valide(secondes: f64) -> Option<f64> {
    (secondes.is_finite() && (0.0..=DUREE_MAX_S).contains(&secondes)).then_some(secondes)
}

/// La durée RÉGLÉE pour la zone, en secondes — ce que l'écran affiche.
/// `0` quand rien n'est réglé ou que la valeur stockée est illisible.
#[must_use]
pub fn duree_reglee_s(db: &Arc<dyn DbBackend>, zone_id: i64) -> f64 {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle_de_duree(zone_id))
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(duree_valide)
        .unwrap_or(0.0)
}

/// La durée APPLIQUÉE à la sortie locale, en millisecondes : `0` en PURE et
/// en bit-perfect strict, quel que soit le réglage.
#[must_use]
pub fn duree_appliquee_ms(db: &Arc<dyn DbBackend>, zone_id: i64) -> u32 {
    if super::audiophile::zone_enabled(db, zone_id)
        || super::bitperfect_strict::zone_enabled(db, zone_id)
    {
        return 0;
    }
    (duree_reglee_s(db, zone_id) * 1000.0).round() as u32
}

/// Une piste vue depuis la frontière : ce qui suffit à juger « même album »
/// et « DSD ».
#[derive(Debug, Clone, Copy, Default)]
pub struct PisteDeJonction<'a> {
    pub album_id: Option<i64>,
    pub album_titre: Option<&'a str>,
    /// Le format tel que la file ou la lecture le porte (`flac`, `dsf`…).
    pub format: Option<&'a str>,
}

fn est_dsd(format: Option<&str>) -> bool {
    format.is_some_and(|f| {
        let f = f.to_ascii_lowercase();
        f.contains("dsf") || f.contains("dff") || f.contains("dsd")
    })
}

/// Même album ? Par l'identifiant quand les deux le portent ; sinon par le
/// titre d'album, non vide, à la casse près. Dans le doute (rien de connu),
/// ce n'est PAS le même album.
#[must_use]
pub fn meme_album(a: PisteDeJonction<'_>, b: PisteDeJonction<'_>) -> bool {
    if let (Some(x), Some(y)) = (a.album_id, b.album_id) {
        return x == y;
    }
    match (a.album_titre, b.album_titre) {
        (Some(x), Some(y)) => {
            let (x, y) = (x.trim(), y.trim());
            !x.is_empty() && x.to_lowercase() == y.to_lowercase()
        }
        _ => false,
    }
}

/// **La consigne d'une frontière**, fonction pure. `album_live` répond pour
/// un identifiant d'album.
pub fn consigne_pure(
    courante: PisteDeJonction<'_>,
    suivante: PisteDeJonction<'_>,
    album_live: impl Fn(i64) -> bool,
) -> ConsigneDeJonction {
    if est_dsd(courante.format) || est_dsd(suivante.format) {
        return ConsigneDeJonction::Interdite(MotifSansFondu::Dop);
    }
    if !meme_album(courante, suivante) {
        return ConsigneDeJonction::Permise;
    }
    let live = courante
        .album_id
        .or(suivante.album_id)
        .is_some_and(album_live);
    if live {
        ConsigneDeJonction::Interdite(MotifSansFondu::AlbumLive)
    } else {
        ConsigneDeJonction::PermiseSiBlanc
    }
}

/// La consigne d'une frontière, lue en base : l'album de la suivante vient de
/// sa piste, le caractère live des types secondaires de l'album.
pub fn consigne(
    db: &Arc<dyn DbBackend>,
    courante: &crate::playback::NowPlaying,
    suivante: &crate::db::play_queue_repo::QueueEntry,
) -> ConsigneDeJonction {
    let album_suivant = suivante.track_id.and_then(|id| {
        crate::db::track_repo::TrackRepo::with_backend(db.clone())
            .get(id)
            .ok()
            .flatten()
            .and_then(|t| t.album_id)
    });
    let a = PisteDeJonction {
        album_id: courante.album_id,
        album_titre: courante.album_title.as_deref(),
        format: courante.format.as_deref(),
    };
    let b = PisteDeJonction {
        album_id: album_suivant,
        album_titre: suivante.album_title.as_deref(),
        format: suivante.format.as_deref(),
    };
    consigne_pure(a, b, |album_id| {
        crate::db::album_repo::AlbumRepo::with_backend(db.clone())
            .types_secondaires_par_album(&[album_id])
            .ok()
            .and_then(|mut table| table.remove(&album_id))
            .is_some_and(|types| crate::metadata::release_type::est_live(&types))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piste<'a>(
        album_id: Option<i64>,
        titre: Option<&'a str>,
        format: Option<&'a str>,
    ) -> PisteDeJonction<'a> {
        PisteDeJonction {
            album_id,
            album_titre: titre,
            format,
        }
    }

    #[test]
    fn la_duree_est_bornee_de_zero_a_douze_secondes() {
        assert_eq!(duree_valide(0.0), Some(0.0));
        assert_eq!(duree_valide(12.0), Some(12.0));
        assert_eq!(duree_valide(5.5), Some(5.5));
        assert_eq!(duree_valide(-0.1), None);
        assert_eq!(duree_valide(12.01), None);
        assert_eq!(duree_valide(f64::NAN), None);
    }

    #[test]
    fn deux_albums_differents_fondent() {
        let c = consigne_pure(
            piste(Some(1), Some("A"), Some("flac")),
            piste(Some(2), Some("B"), Some("flac")),
            |_| false,
        );
        assert_eq!(c, ConsigneDeJonction::Permise);
    }

    #[test]
    fn un_meme_album_ne_fond_que_sur_un_blanc() {
        let c = consigne_pure(
            piste(Some(7), Some("A"), Some("flac")),
            piste(Some(7), Some("A"), Some("flac")),
            |_| false,
        );
        assert_eq!(c, ConsigneDeJonction::PermiseSiBlanc);
    }

    #[test]
    fn un_album_live_ne_fond_jamais() {
        let c = consigne_pure(
            piste(Some(7), None, Some("flac")),
            piste(Some(7), None, Some("flac")),
            |id| id == 7,
        );
        assert_eq!(c, ConsigneDeJonction::Interdite(MotifSansFondu::AlbumLive));
        // Contre-épreuve : le même album, NON live, fond sous condition.
        let c = consigne_pure(
            piste(Some(7), None, Some("flac")),
            piste(Some(7), None, Some("flac")),
            |_| false,
        );
        assert_eq!(c, ConsigneDeJonction::PermiseSiBlanc);
    }

    #[test]
    fn sans_identifiant_le_titre_tranche_et_le_doute_permet() {
        assert!(meme_album(
            piste(None, Some(" Kind of Blue "), None),
            piste(Some(3), Some("kind of blue"), None)
        ));
        assert!(!meme_album(
            piste(None, None, None),
            piste(None, None, None)
        ));
        assert!(!meme_album(
            piste(None, Some(""), None),
            piste(None, Some(""), None)
        ));
    }

    #[test]
    fn un_dsd_de_part_ou_d_autre_ne_fond_jamais() {
        for (a, b) in [(Some("dsf"), Some("flac")), (Some("flac"), Some("DFF"))] {
            assert_eq!(
                consigne_pure(piste(Some(1), None, a), piste(Some(2), None, b), |_| false),
                ConsigneDeJonction::Interdite(MotifSansFondu::Dop)
            );
        }
    }
}
