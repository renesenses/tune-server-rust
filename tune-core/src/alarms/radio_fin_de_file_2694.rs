//! Fil 2694 — la file d'un réveil s'arrête à sa fin : le vrai planificateur
//! (`AlarmScheduler::fire_alarm`) pose la consigne de fin voulue que la radio
//! de fin de file respecte (`poller::fin_de_piste`).
use super::contenus_album_playlist_7058::Banc;
use super::*;

#[tokio::test]
async fn un_reveil_pose_une_fin_voulue_sur_sa_file_2694() {
    let b = Banc::nouveau().await;
    let scheduler = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    scheduler.fire_alarm(&b.alarme("album", 1)).await;
    assert!(
        crate::playback::radio_fin_de_file::fin_voulue(&b.db, b.zone),
        "la file du réveil finit là : pas de radio de fin de file"
    );
}
