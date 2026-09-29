//! Fabrique d'images de test : ISO 9660 (+ Joliet, + Rock Ridge) et UDF.
//!
//! Écrite d'après ECMA-119 et ECMA-167, indépendamment du lecteur : c'est ce
//! qui donne un sens aux épreuves. Aucun outil externe (`genisoimage`,
//! `xorriso`) n'est requis — la CI macOS et Windows n'en a pas.

use std::collections::BTreeMap;

const S: usize = 2048;

/// Les fichiers de l'image : chemin interne → octets.
pub type Contenu = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Noms {
    /// Noms ISO 9660 seuls (majuscules, tronqués).
    Nus,
    /// Arborescence Joliet en plus.
    Joliet,
    /// Entrées Rock Ridge `NM` dans l'arborescence primaire.
    RockRidge,
}

fn both16(v: u16) -> [u8; 4] {
    let mut o = [0u8; 4];
    o[..2].copy_from_slice(&v.to_le_bytes());
    o[2..].copy_from_slice(&v.to_be_bytes());
    o
}

fn both32(v: u32) -> [u8; 8] {
    let mut o = [0u8; 8];
    o[..4].copy_from_slice(&v.to_le_bytes());
    o[4..].copy_from_slice(&v.to_be_bytes());
    o
}

/// Nom ISO 9660 de niveau 1 dérivé d'un nom long : majuscules, 8.3.
fn nom_83(nom: &str, dossier: bool) -> String {
    let filtre = |s: &str, n: usize| -> String {
        s.chars()
            .map(|c| c.to_ascii_uppercase())
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(n)
            .collect()
    };
    if dossier {
        return filtre(nom, 8);
    }
    match nom.rsplit_once('.') {
        Some((base, ext)) => format!("{}.{};1", filtre(base, 8), filtre(ext, 3)),
        None => format!("{};1", filtre(nom, 8)),
    }
}

fn enregistrement(nom: &[u8], extent: u32, longueur: u32, dossier: bool, su: &[u8]) -> Vec<u8> {
    let mut base = 33 + nom.len();
    if nom.len() % 2 == 0 {
        base += 1;
    }
    let mut total = base + su.len();
    if total % 2 == 1 {
        total += 1;
    }
    let mut r = vec![0u8; total];
    r[0] = total as u8;
    r[2..10].copy_from_slice(&both32(extent));
    r[10..18].copy_from_slice(&both32(longueur));
    r[25] = if dossier { 2 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = nom.len() as u8;
    r[33..33 + nom.len()].copy_from_slice(nom);
    r[base..base + su.len()].copy_from_slice(su);
    r
}

/// Range des enregistrements dans des secteurs sans jamais en couper un.
fn en_secteurs(enregistrements: &[Vec<u8>]) -> Vec<u8> {
    let mut sortie: Vec<u8> = Vec::new();
    for r in enregistrements {
        let dans = sortie.len() % S;
        if dans + r.len() > S {
            sortie.resize(sortie.len() + (S - dans), 0);
        }
        sortie.extend_from_slice(r);
    }
    let reste = sortie.len() % S;
    if reste != 0 {
        sortie.resize(sortie.len() + (S - reste), 0);
    }
    sortie
}

struct Arbre {
    /// Dossier → (enfants dossiers, enfants fichiers indices).
    dossiers: BTreeMap<String, (Vec<String>, Vec<usize>)>,
}

fn arbre(contenu: &Contenu) -> Arbre {
    let mut dossiers: BTreeMap<String, (Vec<String>, Vec<usize>)> = BTreeMap::new();
    dossiers.insert(String::new(), (Vec::new(), Vec::new()));
    for (i, (chemin, _)) in contenu.iter().enumerate() {
        let parties: Vec<&str> = chemin.split('/').collect();
        let mut courant = String::new();
        for p in &parties[..parties.len() - 1] {
            let suivant = if courant.is_empty() {
                (*p).to_string()
            } else {
                format!("{courant}/{p}")
            };
            if !dossiers.contains_key(&suivant) {
                dossiers.insert(suivant.clone(), (Vec::new(), Vec::new()));
                dossiers.get_mut(&courant).unwrap().0.push(suivant.clone());
            }
            courant = suivant;
        }
        dossiers.get_mut(&courant).unwrap().1.push(i);
    }
    Arbre { dossiers }
}

fn dernier(chemin: &str) -> &str {
    chemin.rsplit('/').next().unwrap_or(chemin)
}

/// Une image ISO 9660 contenant `contenu`.
pub fn iso(contenu: &Contenu, noms: Noms) -> Vec<u8> {
    let a = arbre(contenu);
    let rr = noms == Noms::RockRidge;
    let joliet = noms == Noms::Joliet;

    let nom_brut = |chemin: &str, dossier: bool, jol: bool| -> Vec<u8> {
        let n = dernier(chemin);
        if jol {
            let mut t = n.to_string();
            if !dossier {
                t.push_str(";1");
            }
            t.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
        } else {
            nom_83(n, dossier).into_bytes()
        }
    };
    let su_nm = |chemin: &str| -> Vec<u8> {
        if !rr {
            return Vec::new();
        }
        let n = dernier(chemin).as_bytes();
        let mut e = vec![b'N', b'M', (5 + n.len()) as u8, 1, 0];
        e.extend_from_slice(n);
        e
    };
    let su_sp: Vec<u8> = if rr {
        vec![b'S', b'P', 7, 1, 0xBE, 0xEF, 0]
    } else {
        Vec::new()
    };

    // Tailles des répertoires (elles ne dépendent que des noms).
    let taille_dossier = |d: &str, jol: bool| -> usize {
        let (sous, fichiers) = &a.dossiers[d];
        let mut recs = vec![
            enregistrement(&[0], 0, 0, true, if d.is_empty() { &su_sp } else { &[] }),
            enregistrement(&[1], 0, 0, true, &[]),
        ];
        for s in sous {
            recs.push(enregistrement(
                &nom_brut(s, true, jol),
                0,
                0,
                true,
                &if jol { Vec::new() } else { su_nm(s) },
            ));
        }
        for &i in fichiers {
            let c = &contenu[i].0;
            recs.push(enregistrement(
                &nom_brut(c, false, jol),
                0,
                0,
                false,
                &if jol { Vec::new() } else { su_nm(c) },
            ));
        }
        en_secteurs(&recs).len()
    };

    let mut prochain: usize = 16 + 2 + usize::from(joliet);
    let mut lba_primaire: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for d in a.dossiers.keys() {
        let t = taille_dossier(d, false);
        lba_primaire.insert(d.clone(), (prochain, t));
        prochain += t / S;
    }
    let mut lba_joliet: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    if joliet {
        for d in a.dossiers.keys() {
            let t = taille_dossier(d, true);
            lba_joliet.insert(d.clone(), (prochain, t));
            prochain += t / S;
        }
    }
    let mut lba_fichiers = Vec::new();
    for (_, octets) in contenu {
        lba_fichiers.push(prochain);
        prochain += octets.len().div_ceil(S).max(1);
    }
    let total = prochain;
    let mut image = vec![0u8; total * S];

    let ecrire_dossiers =
        |image: &mut Vec<u8>, lbas: &BTreeMap<String, (usize, usize)>, jol: bool| {
            for (d, &(lba, t)) in lbas {
                let parent = if d.is_empty() {
                    String::new()
                } else {
                    d.rsplit_once('/')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_default()
                };
                let (plba, pt) = lbas[&parent];
                let (sous, fichiers) = &a.dossiers[d];
                let mut recs = vec![
                    enregistrement(
                        &[0],
                        lba as u32,
                        t as u32,
                        true,
                        if d.is_empty() && !jol { &su_sp } else { &[] },
                    ),
                    enregistrement(&[1], plba as u32, pt as u32, true, &[]),
                ];
                for s in sous {
                    let (sl, st) = lbas[s];
                    recs.push(enregistrement(
                        &nom_brut(s, true, jol),
                        sl as u32,
                        st as u32,
                        true,
                        &if jol { Vec::new() } else { su_nm(s) },
                    ));
                }
                for &i in fichiers {
                    let (c, octets) = &contenu[i];
                    recs.push(enregistrement(
                        &nom_brut(c, false, jol),
                        lba_fichiers[i] as u32,
                        octets.len() as u32,
                        false,
                        &if jol { Vec::new() } else { su_nm(c) },
                    ));
                }
                let bloc = en_secteurs(&recs);
                image[lba * S..lba * S + bloc.len()].copy_from_slice(&bloc);
            }
        };
    ecrire_dossiers(&mut image, &lba_primaire, false);
    if joliet {
        ecrire_dossiers(&mut image, &lba_joliet, true);
    }
    for (i, (_, octets)) in contenu.iter().enumerate() {
        let debut = lba_fichiers[i] * S;
        image[debut..debut + octets.len()].copy_from_slice(octets);
    }

    let descripteur = |type_vd: u8, racine: (usize, usize), esc: Option<&[u8]>| -> Vec<u8> {
        let mut v = vec![0u8; S];
        v[0] = type_vd;
        v[1..6].copy_from_slice(b"CD001");
        v[6] = 1;
        v[80..88].copy_from_slice(&both32(total as u32));
        if let Some(e) = esc {
            v[88..88 + e.len()].copy_from_slice(e);
        }
        v[120..124].copy_from_slice(&both16(1));
        v[124..128].copy_from_slice(&both16(1));
        v[128..132].copy_from_slice(&both16(S as u16));
        let r = enregistrement(&[0], racine.0 as u32, racine.1 as u32, true, &[]);
        v[156..156 + 34].copy_from_slice(&r[..34]);
        v[881] = 1;
        v
    };
    let pvd = descripteur(1, lba_primaire[""], None);
    image[16 * S..17 * S].copy_from_slice(&pvd);
    let mut suivant = 17;
    if joliet {
        let svd = descripteur(2, lba_joliet[""], Some(b"%/E"));
        image[17 * S..18 * S].copy_from_slice(&svd);
        suivant = 18;
    }
    let term = &mut image[suivant * S..(suivant + 1) * S];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    image
}

// ─── UDF ────────────────────────────────────────────────────────────────

fn tag(bloc: &mut [u8], id: u16, lieu: u32) {
    bloc[0..2].copy_from_slice(&id.to_le_bytes());
    bloc[2..4].copy_from_slice(&2u16.to_le_bytes());
    bloc[12..16].copy_from_slice(&lieu.to_le_bytes());
    let somme = bloc[..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |a, (_, v)| a.wrapping_add(*v));
    bloc[4] = somme;
}

/// Une image UDF 1.02 seule (aucune arborescence ISO 9660), partition physique.
pub fn udf(contenu: &Contenu) -> Vec<u8> {
    let a = arbre(contenu);
    const DEBUT_PARTITION: usize = 300;
    // Allocation dans la partition : lbn 0 = FSD, puis une entrée + données
    // par dossier, une entrée + données par fichier.
    let mut prochain = 1usize;
    let mut entree_dossier: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let fid = |nom: &str, dossier: bool, parent: bool, icb: usize| -> Vec<u8> {
        let ident: Vec<u8> = if parent {
            Vec::new()
        } else {
            let mut v = vec![8u8];
            v.extend(nom.bytes());
            v
        };
        let longueur = (38 + ident.len()).div_ceil(4) * 4;
        let mut f = vec![0u8; longueur];
        f[16..18].copy_from_slice(&1u16.to_le_bytes());
        f[18] = (if dossier { 2 } else { 0 }) | (if parent { 8 } else { 0 });
        f[19] = ident.len() as u8;
        f[20..24].copy_from_slice(&(S as u32).to_le_bytes());
        f[24..28].copy_from_slice(&(icb as u32).to_le_bytes());
        f[38..38 + ident.len()].copy_from_slice(&ident);
        tag(&mut f, 257, 0);
        f
    };
    let taille_dossier = |d: &str| -> usize {
        let (sous, fichiers) = &a.dossiers[d];
        let mut t = fid("", true, true, 0).len();
        for s in sous {
            t += fid(dernier(s), true, false, 0).len();
        }
        for &i in fichiers {
            t += fid(dernier(&contenu[i].0), false, false, 0).len();
        }
        t
    };
    for d in a.dossiers.keys() {
        let t = taille_dossier(d);
        entree_dossier.insert(d.clone(), (prochain, t));
        prochain += 1 + t.div_ceil(S);
    }
    let mut entree_fichier = Vec::new();
    for (_, octets) in contenu {
        entree_fichier.push(prochain);
        prochain += 1 + octets.len().div_ceil(S);
    }
    let longueur_partition = prochain;
    let total = DEBUT_PARTITION + longueur_partition + 1;
    let mut image = vec![0u8; total * S];
    let bloc = |image: &mut Vec<u8>, lbn: usize| -> std::ops::Range<usize> {
        let _ = image;
        (DEBUT_PARTITION + lbn) * S..(DEBUT_PARTITION + lbn + 1) * S
    };

    // Séquence de reconnaissance (sans CD001).
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let s = &mut image[(16 + i) * S..(17 + i) * S];
        s[1..6].copy_from_slice(*id);
        s[6] = 1;
    }
    // Ancre.
    {
        let s = &mut image[256 * S..257 * S];
        s[16..20].copy_from_slice(&((16 * S) as u32).to_le_bytes());
        s[20..24].copy_from_slice(&32u32.to_le_bytes());
        tag(s, 2, 256);
    }
    // Partition.
    {
        let s = &mut image[32 * S..33 * S];
        s[22..24].copy_from_slice(&0u16.to_le_bytes());
        s[188..192].copy_from_slice(&(DEBUT_PARTITION as u32).to_le_bytes());
        s[192..196].copy_from_slice(&(longueur_partition as u32).to_le_bytes());
        tag(s, 5, 32);
    }
    // Volume logique.
    {
        let s = &mut image[33 * S..34 * S];
        s[212..216].copy_from_slice(&(S as u32).to_le_bytes());
        s[248..252].copy_from_slice(&(S as u32).to_le_bytes());
        s[252..256].copy_from_slice(&0u32.to_le_bytes());
        s[256..258].copy_from_slice(&0u16.to_le_bytes());
        s[264..268].copy_from_slice(&6u32.to_le_bytes());
        s[268..272].copy_from_slice(&1u32.to_le_bytes());
        s[440] = 1;
        s[441] = 6;
        s[442..444].copy_from_slice(&1u16.to_le_bytes());
        s[444..446].copy_from_slice(&0u16.to_le_bytes());
        tag(s, 6, 33);
    }
    // Terminaison.
    tag(&mut image[34 * S..35 * S], 8, 34);
    // Ensemble de fichiers.
    {
        let r = bloc(&mut image, 0);
        let s = &mut image[r];
        s[400..404].copy_from_slice(&(S as u32).to_le_bytes());
        s[404..408].copy_from_slice(&(entree_dossier[""].0 as u32).to_le_bytes());
        tag(s, 256, 0);
    }
    let entree = |image: &mut Vec<u8>, lbn: usize, dossier: bool, taille: usize| {
        let r = bloc(image, lbn);
        let s = &mut image[r];
        s[27] = if dossier { 4 } else { 5 };
        s[34..36].copy_from_slice(&0u16.to_le_bytes());
        s[56..64].copy_from_slice(&(taille as u64).to_le_bytes());
        s[168..172].copy_from_slice(&0u32.to_le_bytes());
        s[172..176].copy_from_slice(&8u32.to_le_bytes());
        s[176..180].copy_from_slice(&(taille as u32).to_le_bytes());
        s[180..184].copy_from_slice(&((lbn + 1) as u32).to_le_bytes());
        tag(s, 261, lbn as u32);
    };
    for (d, &(lbn, t)) in &entree_dossier {
        entree(&mut image, lbn, true, t);
        let parent = d
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        let (sous, fichiers) = &a.dossiers[d];
        let mut donnees = fid("", true, true, entree_dossier[&parent].0);
        for s in sous {
            donnees.extend(fid(dernier(s), true, false, entree_dossier[s].0));
        }
        for &i in fichiers {
            donnees.extend(fid(dernier(&contenu[i].0), false, false, entree_fichier[i]));
        }
        let debut = (DEBUT_PARTITION + lbn + 1) * S;
        image[debut..debut + donnees.len()].copy_from_slice(&donnees);
    }
    for (i, (_, octets)) in contenu.iter().enumerate() {
        let lbn = entree_fichier[i];
        entree(&mut image, lbn, false, octets.len());
        let debut = (DEBUT_PARTITION + lbn + 1) * S;
        image[debut..debut + octets.len()].copy_from_slice(octets);
    }
    image
}
