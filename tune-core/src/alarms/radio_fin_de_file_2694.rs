//! Fil 2694 — la file d'un réveil s'arrête à sa fin : le vrai planificateur
//! (`AlarmScheduler::fire_alarm`) pose la consigne de fin voulue que la radio
//! de fin de file respecte (`poller::fin_de_piste`).
use super::contenus_album_playlist_7058::Banc;
use super::*;

#[tokio::test]
async fn un_reveil_pose_une_fin_voulue_sur_sa_file_2694() {
    let mut b = Banc::nouveau().await;
    // La consigne vit en mémoire, par zone : un identifiant propre à cette
    // épreuve la tient à l'écart des autres réveils testés en parallèle.
    let zone = 26_949_001i64;
    b.db.execute(
        "UPDATE zones SET id = ? WHERE id = ?",
        &[&zone as &dyn crate::db::backend::ToSqlValue, &b.zone],
    )
    .unwrap();
    b.zone = zone;
    let scheduler = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    scheduler.fire_alarm(&b.alarme("album", 1)).await;
    assert!(
        crate::playback::radio_fin_de_file::fin_voulue(&b.db, b.zone),
        "la file du réveil finit là : pas de radio de fin de file"
    );
}
