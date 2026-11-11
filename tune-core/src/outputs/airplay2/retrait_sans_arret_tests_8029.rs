//! #8029 — une enceinte AirPlay 2 retirée du registre sans `stop()` laissait
//! son démon tourner.
//!
//! Le témoin lance un faux démon (un script qui note son PID puis dort) par la
//! commande de production, le confie à la sortie comme `spawn_daemon`, puis
//! détruit la sortie sans l'arrêter — ce que fait un retrait du registre.

use super::*;
use std::os::unix::fs::PermissionsExt;

/// Le processus `pid` est-il encore vivant ? Un zombie (tué, pas encore
/// récolté) compte pour mort.
fn vivant(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            // L'état suit la parenthèse fermante du nom de commande.
            let etat = stat
                .rsplit_once(')')
                .and_then(|(_, reste)| reste.trim_start().chars().next());
            !matches!(etat, Some('Z') | Some('X') | None)
        }
        Err(_) => false,
    }
}

#[tokio::test]
async fn le_demon_meurt_avec_la_sortie_retiree_sans_arret_8029() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("faux-airplay-daemon");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sortie = Airplay2Output::new(
        "Enceinte 8029".into(),
        "127.0.0.1".into(),
        7000,
        "airplay2-retrait-8029".into(),
        "00:11:22:33:44:80".into(),
    );
    let mut child = commande_du_demon(script.to_str().unwrap())
        .spawn()
        .expect("lancement du faux démon");
    let pid = child.id().expect("pid du faux démon");
    let stdin = child.stdin.take().unwrap();
    *sortie.process.lock().await = Some(DaemonProcess {
        child: Some(child),
        stdin: Box::new(stdin),
    });
    assert!(vivant(pid), "témoin : le faux démon tourne");

    // Le retrait du registre : la sortie est détruite, `stop()` jamais appelé.
    drop(sortie);

    let mut mort = false;
    for _ in 0..100 {
        if !vivant(pid) {
            mort = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !mort {
        // Ne pas laisser le faux démon derrière le témoin.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    assert!(
        mort,
        "le démon AirPlay 2 doit s'arrêter avec la sortie retirée du registre, \
         sinon il tourne orphelin jusqu'à l'arrêt du serveur (#8029)"
    );
}
