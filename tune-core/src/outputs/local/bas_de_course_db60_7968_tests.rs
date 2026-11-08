//! #7968 — le bas de la course `db60` d'une sortie locale : chaque position
//! du curseur donne un niveau distinct, et seule la position 0 est muette.
//!
//! `set_volume` tronquait le gain en millièmes (`as u32`), puis
//! `effective_volume_units` arrondissait de nouveau le produit au millième :
//! 1 % du curseur (gain 0,0002) valait 0, le silence courait jusqu'à 5 %, et
//! au-dessus quatre ou cinq positions voisines tombaient sur le même
//! millième. Le gain est désormais rangé en
//! [`UNITE_DU_GAIN_DE_RENDU`] (millionièmes) jusqu'au rendu.

use std::sync::atomic::Ordering;

use super::LocalOutput;
use super::gain_au_rendu_7264::GainAuRendu;
use crate::audio::volume_scale::{UNITE_DU_GAIN_DE_RENDU, gain_des_unites, gain_perceptif};
use crate::outputs::traits::OutputTarget;

/// Le gain que les rappels de rendu multiplient quand le curseur `db60` est
/// à `position` %, posé par le vrai `set_volume`.
async fn gain_rendu_a(sortie: &LocalOutput, position: u32) -> u32 {
    sortie
        .set_volume(gain_perceptif(f64::from(position) / 100.0))
        .await
        .expect("set_volume");
    sortie.gain_de_rendu().load(Ordering::SeqCst)
}

/// LE témoin de l'issue : positions 1 à 10 % sur `db60`, gains strictement
/// croissants, aucun nul hors 0, et chacun fidèle à la loi à 1 % près.
#[tokio::test]
async fn positions_1_a_10_du_curseur_db60_donnent_des_gains_strictement_croissants_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    assert_eq!(gain_rendu_a(&sortie, 0).await, 0, "0 % : le silence exact");

    let mut precedent = 0;
    for position in 1..=10 {
        let rendu = gain_rendu_a(&sortie, position).await;
        assert!(
            rendu > 0,
            "{position} % du curseur db60 rendu muet : le gain a été arrondi à 0"
        );
        assert!(
            rendu > precedent,
            "{position} % du curseur db60 rend {rendu}, pas plus que {} % ({precedent}) : \
             deux positions au même niveau",
            position - 1
        );
        let attendu = gain_perceptif(f64::from(position) / 100.0);
        let lu = f64::from(rendu) / f64::from(UNITE_DU_GAIN_DE_RENDU);
        assert!(
            ((lu - attendu) / attendu).abs() < 0.01,
            "{position} % du curseur db60 : gain rendu {lu}, la loi demande {attendu}"
        );
        precedent = rendu;
    }
}

/// Le gain réellement appliqué au mot par le rendu natif (#7264) suit : à
/// 1 % du curseur, `GainAuRendu` multiplie par ≈ 0,0002, pas par 0. Et une
/// atténuation ReplayGain de −6 dB par-dessus ne rend pas la sortie muette.
#[tokio::test]
async fn a_un_pour_cent_le_rendu_natif_applique_un_gain_non_nul_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    gain_rendu_a(&sortie, 1).await;
    let gain = GainAuRendu::nouveau(sortie.gain_de_rendu(), 48_000, 2);
    let attendu = gain_perceptif(0.01);
    assert!(
        ((gain.courant() - attendu) / attendu).abs() < 0.01,
        "1 % du curseur db60 : le rendu applique {}, la loi demande {attendu}",
        gain.courant()
    );

    sortie.set_replaygain_factor(0.5);
    assert!(
        sortie.gain_de_rendu().load(Ordering::SeqCst) > 0,
        "1 % du curseur et ReplayGain −6 dB : sortie muette"
    );
}

/// La sourdine, puis son retrait, rendent le niveau exact d'avant : le bas
/// de course retrouvé n'est pas perdu par un aller-retour.
#[tokio::test]
async fn la_sourdine_rend_le_niveau_fin_d_avant_7968() {
    let sortie = LocalOutput::new("DAC test".to_string());
    let avant = gain_rendu_a(&sortie, 3).await;
    sortie.set_mute(true).await.expect("set_mute");
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), 0);
    sortie.set_mute(false).await.expect("set_mute");
    assert_eq!(sortie.gain_de_rendu().load(Ordering::SeqCst), avant);
    assert!(avant > 0, "3 % du curseur db60 rendu muet");
}

/// La conversion commune des rappels : 1 000 000 unités valent 1,0, 1 % du
/// curseur `db60` reste ≈ 0,0002, et rien n'est rendu au-delà de l'unité.
#[test]
fn la_conversion_commune_lit_le_volume_au_millionieme_7968() {
    assert_eq!(gain_des_unites(UNITE_DU_GAIN_DE_RENDU), 1.0);
    assert_eq!(gain_des_unites(0), 0.0);
    assert_eq!(gain_des_unites(200), 0.0002);
    assert_eq!(
        gain_des_unites(u32::MAX),
        1.0,
        "plus fort que la pleine échelle"
    );
}

/// Le texte d'un fichier, sans aucun blanc : une lecture répartie sur
/// plusieurs lignes se lit comme une seule.
fn sans_blancs(texte: &str) -> String {
    texte.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Les lectures du volume partagé encore divisées par 1000 dans `texte` :
/// `<…vol…>.load(Ordering::…) as f32|f64 / 1000` ou
/// `f64::from(<…vol…>.load(Ordering::…)) / 1000`. Le ReplayGain, lui, reste
/// en millièmes : il ne porte pas « vol » dans son nom.
fn lectures_au_millieme(texte: &str) -> usize {
    let compact = sans_blancs(texte);
    let charge = ".load(Ordering::";
    let mut trouvees = 0;
    let mut depart = 0;
    while let Some(i) = compact[depart..].find(charge) {
        let debut = depart + i;
        depart = debut + charge.len();
        // Le receveur de `.load`, lu à rebours : `self.volume`, `vol_cb`…
        let recepteur_a_rebours: String = compact[..debut]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        let recepteur: String = recepteur_a_rebours.chars().rev().collect();
        if !recepteur.to_lowercase().contains("vol") {
            continue;
        }
        let Some(fin) = compact[depart..].find(')') else {
            continue;
        };
        let suite = &compact[depart + fin + 1..];
        if suite.starts_with("asf32/1000")
            || suite.starts_with("asf64/1000")
            || suite.starts_with(")/1000")
        {
            trouvees += 1;
        }
    }
    trouvees
}

/// Tous les fichiers `.rs` sous `dossier`, sous-dossiers compris.
fn sources_de(dossier: &std::path::Path, vus: &mut Vec<std::path::PathBuf>) {
    for entree in std::fs::read_dir(dossier).expect("lire le dossier des sorties") {
        let chemin = entree.expect("entrée du dossier").path();
        if chemin.is_dir() {
            sources_de(&chemin, vus);
        } else if chemin.extension().is_some_and(|e| e == "rs") {
            vus.push(chemin);
        }
    }
}

/// LA garde des lecteurs : aucun fichier de `outputs/` — compilé ou non sur
/// cette machine (`cfg` macOS, Windows + ASIO) — ne lit plus le volume
/// partagé en millièmes. Sinon le son part ×1000 (+60 dB), comme le faisaient
/// CoreAudio exclusif et les quatre transports ASIO « Processed ».
#[test]
fn aucun_lecteur_du_volume_partage_ne_le_lit_encore_en_milliemes_7968() {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/outputs");
    let mut fichiers = Vec::new();
    sources_de(&racine, &mut fichiers);
    assert!(fichiers.len() > 50, "sources des sorties introuvables");

    let fautifs: Vec<String> = fichiers
        .iter()
        .filter(|f| !f.ends_with("bas_de_course_db60_7968_tests.rs"))
        .filter_map(|f| {
            let texte = std::fs::read_to_string(f).expect("lire une source");
            let n = lectures_au_millieme(&texte);
            (n > 0).then(|| format!("{} ({n})", f.display()))
        })
        .collect();
    assert!(
        fautifs.is_empty(),
        "volume partagé encore lu en millièmes (÷1000 au lieu de \
         UNITE_DU_GAIN_DE_RENDU) : {fautifs:?}"
    );

    // Et les deux rappels hors du rendu natif passent par la conversion
    // commune : CoreAudio exclusif une fois, ASIO « Processed » quatre fois
    // (I32, I24, I16, F32). La définition est ailleurs, dans `volume_scale`.
    for (fichier, attendu) in [("coreaudio_exclusive.rs", 1), ("asio_exclusive.rs", 4)] {
        let texte = std::fs::read_to_string(racine.join(fichier)).expect("lire une source");
        let appels = sans_blancs(&texte).matches("gain_des_unites(").count();
        assert_eq!(
            appels, attendu,
            "{fichier} : {appels} lecture(s) du volume par gain_des_unites, {attendu} attendue(s)"
        );
    }
}

/// La garde sait voir les formes fautives, y compris réparties sur deux
/// lignes, et laisse le ReplayGain en millièmes.
#[test]
fn la_garde_reconnait_une_lecture_au_millieme_7968() {
    assert_eq!(
        lectures_au_millieme("let v = vol_for_callback.load(Ordering::Relaxed) as f32 / 1000.0;"),
        1
    );
    assert_eq!(
        lectures_au_millieme(
            "let volume = volume.load(Ordering::Relaxed)\n    as f64\n    / 1000.0;"
        ),
        1
    );
    assert_eq!(
        lectures_au_millieme("f64::from(self.volume.load(Ordering::SeqCst)) / 1000.0"),
        1
    );
    assert_eq!(
        lectures_au_millieme("f64::from(self.replaygain_seul.load(Ordering::SeqCst)) / 1000.0"),
        0
    );
    assert_eq!(
        lectures_au_millieme(
            "let v = volume.load(Ordering::Relaxed) as f32 / UNITE_DU_GAIN_DE_RENDU as f32;"
        ),
        0
    );
}
