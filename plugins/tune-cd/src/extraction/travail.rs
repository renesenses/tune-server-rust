//! Le travail d'une extraction (#2466), sur un fil BLOQUANT : lire, encoder,
//! baliser et ranger chaque piste, dans l'ordre.
//!
//! Chaque fichier est d'abord écrit sous un nom provisoire
//! (`NN - Titre.flac.part`, que le scan ne prend pas pour de l'audio), puis
//! renommé une fois complet et balisé : la bibliothèque ne voit jamais une
//! piste à moitié écrite. Une annulation ou une erreur retire le fichier
//! provisoire en cours ; les pistes déjà terminées restent.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_core::audio::encoder::AudioEncoder;

use super::accuraterip::{self, CalculAccurateRip};
use super::balises::{self, Balises};
use super::lecture_sure::{self, Bilan, SECTEURS_PAR_BLOC, Verification};
use super::{ErreurTravail, Format, StatutPiste, Travail};
use crate::lecteur::{ErreurCd, LecteurDisque};
use crate::toc::{OCTETS_PAR_SECTEUR, TRAMES_PAR_SECTEUR};

/// Au plus un évènement de progression par intervalle.
pub const INTERVALLE_PROGRESSION: Duration = Duration::from_secs(1);

/// Une piste à extraire, son chemin final et ses balises.
#[derive(Debug, Clone)]
pub struct PlanPiste {
    pub numero: u8,
    pub debut: u32,
    pub fin: u32,
    pub chemin: PathBuf,
    pub balises: Balises,
    /// Première et dernière piste AUDIO du disque (bords AccurateRip).
    pub premiere_audio: bool,
    pub derniere_audio: bool,
}

/// Tout ce que le fil bloquant doit savoir.
pub struct Plan {
    pub lecteur: Arc<dyn LecteurDisque>,
    /// La génération du lecteur au moment de la TOC : un autre lecteur (ou
    /// un autre disque) en cours de route arrête l'extraction.
    pub generation: u64,
    pub format: Format,
    pub verification: Verification,
    pub ecraser: bool,
    pub dossier: PathBuf,
    pub pistes: Vec<PlanPiste>,
    pub pochette: Option<Vec<u8>>,
}

/// Le fichier provisoire d'un chemin final.
pub fn provisoire(chemin: &Path) -> PathBuf {
    let mut s = chemin.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Extrait toutes les pistes du plan. `publier` reçoit chaque changement
/// notable (début et fin de piste, progression au plus une fois par
/// [`INTERVALLE_PROGRESSION`]).
pub fn executer(plan: &Plan, travail: &Travail, publier: &dyn Fn()) -> Result<(), ErreurTravail> {
    std::fs::create_dir_all(&plan.dossier).map_err(|e| {
        ErreurTravail::new(
            "ecriture",
            format!("Création du dossier {} : {e}", plan.dossier.display()),
        )
    })?;
    if let Some(p) = &plan.pochette {
        ecrire_pochette_du_dossier(&plan.dossier, p);
    }
    let mut derniere_publication = Instant::now();
    for (i, piste) in plan.pistes.iter().enumerate() {
        if travail.annulee() {
            return Err(annulee(travail, i));
        }
        travail.modifier(|e| {
            e.piste_courante = Some(piste.numero);
            e.pistes[i].statut = StatutPiste::Extraction;
        });
        publier();
        let resultat = extraire_piste(plan, piste, travail, i, &mut || {
            if derniere_publication.elapsed() >= INTERVALLE_PROGRESSION {
                derniere_publication = Instant::now();
                publier();
            }
        });
        if let Err(err) = resultat {
            let _ = std::fs::remove_file(provisoire(&piste.chemin));
            if err.code == "annulee" {
                return Err(annulee(travail, i));
            }
            travail.modifier(|e| {
                e.pistes[i].statut = StatutPiste::Echec;
                e.pistes[i].erreur = Some(err.message.clone());
            });
            return Err(err);
        }
        travail.modifier(|e| {
            e.pistes[i].statut = StatutPiste::Terminee;
            e.pistes[i].fichier = Some(piste.chemin.to_string_lossy().to_string());
            e.recalculer();
        });
        publier();
    }
    travail.modifier(|e| e.piste_courante = None);
    Ok(())
}

/// Marque annulées la piste `i` et les suivantes.
fn annulee(travail: &Travail, i: usize) -> ErreurTravail {
    travail.modifier(|e| {
        for p in e.pistes.iter_mut().skip(i) {
            p.statut = StatutPiste::Annulee;
        }
        e.piste_courante = None;
    });
    ErreurTravail::new("annulee", "Extraction annulée.")
}

enum Sortie {
    Flac(AudioEncoder),
    Wav(BufWriter<std::fs::File>),
}

fn erreur_ecriture(e: impl std::fmt::Display) -> ErreurTravail {
    ErreurTravail::new("ecriture", e.to_string())
}

/// L'en-tête d'un WAV PCM 16 bits stéréo 44,1 kHz de `octets` octets.
pub fn entete_wav(octets: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + octets).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&44_100u32.to_le_bytes());
    h[28..32].copy_from_slice(&176_400u32.to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&octets.to_le_bytes());
    h
}

fn extraire_piste(
    plan: &Plan,
    piste: &PlanPiste,
    travail: &Travail,
    i: usize,
    progression: &mut dyn FnMut(),
) -> Result<(), ErreurTravail> {
    if piste.chemin.exists() && !plan.ecraser {
        return Err(ErreurTravail::new(
            "fichier_existant",
            format!("{} existe déjà.", piste.chemin.display()),
        ));
    }
    if let Some(parent) = piste.chemin.parent() {
        std::fs::create_dir_all(parent).map_err(erreur_ecriture)?;
    }
    let part = provisoire(&piste.chemin);
    let secteurs = piste.fin - piste.debut;
    let octets = secteurs as u64 * OCTETS_PAR_SECTEUR as u64;
    let mut sortie = match plan.format {
        Format::Flac => {
            let mut enc = AudioEncoder::new("flac", 44_100, 16, 2);
            enc.start_sync().map_err(erreur_ecriture)?;
            Sortie::Flac(enc)
        }
        Format::Wav => {
            let taille = u32::try_from(octets)
                .ok()
                .filter(|o| *o <= u32::MAX - 36)
                .ok_or_else(|| erreur_ecriture("piste trop longue pour un WAV"))?;
            let mut w = BufWriter::new(std::fs::File::create(&part).map_err(erreur_ecriture)?);
            w.write_all(&entete_wav(taille)).map_err(erreur_ecriture)?;
            Sortie::Wav(w)
        }
    };
    let mut ar = CalculAccurateRip::new(
        secteurs * TRAMES_PAR_SECTEUR as u32,
        piste.premiere_audio,
        piste.derniere_audio,
    );
    let mut bilan = Bilan::default();
    let mut lba = piste.debut;
    while lba < piste.fin {
        if travail.annulee() {
            return Err(ErreurTravail::new("annulee", "Extraction annulée."));
        }
        if plan.lecteur.generation_lecteur() != plan.generation {
            return Err(disque_retire());
        }
        let n = (piste.fin - lba).min(SECTEURS_PAR_BLOC);
        let bloc = match lecture_sure::lire_bloc(
            plan.lecteur.as_ref(),
            lba,
            n,
            plan.verification,
            &mut bilan,
        ) {
            Ok(b) => b,
            Err(ErreurCd::AucunDisque) => return Err(disque_retire()),
            Err(e) => return Err(ErreurTravail::new("lecture", e.to_string())),
        };
        ar.ajouter(&bloc);
        match &mut sortie {
            Sortie::Flac(enc) => enc.write_sync(&bloc).map_err(erreur_ecriture)?,
            Sortie::Wav(w) => w.write_all(&bloc).map_err(erreur_ecriture)?,
        }
        lba += n;
        let lus = lba - piste.debut;
        travail.modifier(|e| {
            let p = &mut e.pistes[i];
            p.secteurs_lus = lus;
            p.pourcentage = super::arrondi(lus as f64 * 100.0 / secteurs.max(1) as f64);
            p.lectures_supplementaires = bilan.lectures_supplementaires;
            p.secteurs_illisibles = bilan.secteurs_illisibles;
            e.recalculer();
        });
        progression();
    }
    travail.modifier(|e| e.pistes[i].statut = StatutPiste::Ecriture);
    match sortie {
        Sortie::Flac(mut enc) => {
            let flac = enc.finish_sync().map_err(erreur_ecriture)?;
            std::fs::write(&part, flac).map_err(erreur_ecriture)?;
        }
        Sortie::Wav(w) => {
            w.into_inner()
                .map_err(|e| erreur_ecriture(e.error()))?
                .sync_all()
                .map_err(erreur_ecriture)?;
        }
    }
    balises::ecrire(&part, plan.format, &piste.balises, plan.pochette.as_deref())
        .map_err(|m| ErreurTravail::new("balises", m))?;
    if plan.ecraser && piste.chemin.exists() {
        std::fs::remove_file(&piste.chemin).map_err(erreur_ecriture)?;
    }
    std::fs::rename(&part, &piste.chemin).map_err(erreur_ecriture)?;
    let (v1, v2) = ar.resultat();
    travail.modifier(|e| {
        e.pistes[i].accuraterip_v1 = Some(accuraterip::hex(v1));
        e.pistes[i].accuraterip_v2 = Some(accuraterip::hex(v2));
    });
    if bilan.secteurs_illisibles > 0 {
        tracing::warn!(
            piste = piste.numero,
            secteurs = bilan.secteurs_illisibles,
            "cd_extraction_piste_avec_secteurs_illisibles"
        );
    }
    Ok(())
}

fn disque_retire() -> ErreurTravail {
    ErreurTravail::new(
        "disque_retire",
        "Le disque a été éjecté ou remplacé pendant l'extraction.",
    )
}

/// `cover.jpg` (ou `.png`) dans le dossier de l'album, s'il n'y en a pas :
/// la pochette de dossier que le scan sait lire.
fn ecrire_pochette_du_dossier(dossier: &Path, octets: &[u8]) {
    let ext = match balises::type_d_image(octets) {
        Some(lofty::picture::MimeType::Png) => "png",
        Some(_) => "jpg",
        None => return,
    };
    let chemin = dossier.join(format!("cover.{ext}"));
    if !chemin.exists()
        && let Err(e) = std::fs::write(&chemin, octets)
    {
        tracing::warn!(error = %e, "cd_extraction_pochette_du_dossier_non_ecrite");
    }
}
