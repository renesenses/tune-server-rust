//! Preuve sur un VRAI disque (#4863) : `cargo run -p tune-cd --example
//! preuve_cd -- <volume> <sortie>`. Lit la TOC par le lecteur de volume,
//! calcule l'identifiant de disque, puis écrit dans `<sortie>` :
//! `piste2_10s.pcm` (les 10 premières secondes de la piste 2) et
//! `jonction_2_3.pcm` (24 secteurs de part et d'autre du début de la piste 3),
//! en secteurs bruts petit-boutistes — à comparer, hors de Tune, aux octets
//! des fichiers AIFF.

use std::path::PathBuf;

use tune_cd::cddafs::{LecteurVolume, VolumeCdda};
use tune_cd::discid::disc_id;
use tune_cd::lecteur::LecteurDisque;
use tune_cd::toc::OCTETS_PAR_SECTEUR;

fn main() {
    let mut a = std::env::args().skip(1);
    let volume = PathBuf::from(a.next().expect("volume"));
    let sortie = PathBuf::from(a.next().expect("dossier de sortie"));
    #[cfg(target_os = "macos")]
    println!(
        "volumes cddafs : {:?} ; lecteur optique branché : {}",
        tune_cd::macos::volumes_cddafs(),
        tune_cd::macos::lecteur_optique_branche()
    );
    let v = VolumeCdda::ouvrir(&volume).expect("volume");
    for f in &v.fichiers {
        println!(
            "fichier piste {} : début LBA {}, {} secteurs, PCM à l'octet {}, petit-boutiste {}",
            f.numero, f.debut, f.secteurs, f.debut_donnees, f.petit_boutiste
        );
    }
    let l = LecteurVolume::sur_dossier(volume);
    println!("présence : {:?}", l.presence());
    let toc = l.lire_toc().expect("toc");
    println!("TOC : {toc:?}");
    for p in &toc.pistes {
        println!(
            "piste {} : LBA {}, {} secteurs, {} ms",
            p.numero,
            p.debut,
            toc.secteurs(p.numero).unwrap_or(0),
            toc.duree_ms(p.numero).unwrap_or(0)
        );
    }
    println!("disc_id : {}", disc_id(&toc));

    let lire = |lba: u32, n: u32| {
        let mut o = vec![0u8; n as usize * OCTETS_PAR_SECTEUR];
        for (i, bloc) in o.chunks_mut(24 * OCTETS_PAR_SECTEUR).enumerate() {
            let k = (bloc.len() / OCTETS_PAR_SECTEUR) as u32;
            l.lire_secteurs(lba + i as u32 * 24, k, bloc)
                .expect("lecture");
        }
        o
    };
    let d2 = toc.piste(2).expect("piste 2").debut;
    let t = std::time::Instant::now();
    std::fs::write(sortie.join("piste2_10s.pcm"), lire(d2, 750)).unwrap();
    println!("piste 2, 750 secteurs (10 s) lus en {:?}", t.elapsed());
    let d3 = toc.piste(3).expect("piste 3").debut;
    std::fs::write(sortie.join("jonction_2_3.pcm"), lire(d3 - 24, 48)).unwrap();
    println!("jonction 2→3 : secteurs {}..{}", d3 - 24, d3 + 24);
}
