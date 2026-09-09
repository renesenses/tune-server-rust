//! Le porteur DoP survit-il au chemin RÉEL, de bout en bout ? (#1894, #2369)
//!
//! Les deux signalements de terrain — le WiiM Pro en réseau (#1894) et le SMSL
//! en USB local (#2369) — décrivent le MÊME symptôme, le bruit blanc, et le
//! bruit blanc n'est pas un symptôme vague : c'est la signature d'un train DoP
//! dont le marqueur `0x05`/`0xFA` n'est plus dans l'octet de poids fort. Le DAC
//! ne verrouille pas en DSD, lit le train de bits comme du PCM, et joue du
//! bruit.
//!
//! Ces deux chemins partagent EXACTEMENT un segment, et un seul :
//! [`crate::orchestrator`] les fait tous deux passer par `anticiper_le_dop`,
//! qui construit une session WAV 24 bits et confie la charge utile à
//! [`decode_dsd_to_dop_streaming`]. Le drapeau `is_local_output` n'y change
//! qu'un champ de journal (`sortie="locale"` ou `"réseau"`) : **les octets
//! émis sont les mêmes**. Ce qui diffère est en AVAL — le rappel cpal d'un
//! côté, HTTP et la DIDL de l'autre.
//!
//! Ce module mesure ce segment commun, sans matériel : il fabrique un fichier
//! DSD dont chaque octet est identifiable, le fait passer par le chemin de
//! production, et vérifie sur les octets rendus que
//!
//! 1. le détecteur DE PRODUCTION ([`crate::outputs::local::is_dop_pcm`], celui
//!    dont dépendent le contournement du volume et le refus des bras exclusifs
//!    Windows) reconnaît le flux comme du DoP ;
//! 2. le marqueur alterne sur TOUTE la piste, pas seulement sur les 32
//!    premières trames que sonde le détecteur, et il est identique sur tous les
//!    canaux d'une même trame ;
//! 3. le train DSD se reconstruit **octet pour octet** depuis les mots de
//!    24 bits — c'est-à-dire qu'aucun octet n'a été perdu, ni deux canaux
//!    échangés, ni la phase de trame rompue.
//!
//! Le point 3 est le seul qui distingue un porteur intact d'un porteur
//! plausible : une garde qui ne relit que les marqueurs reste verte face à un
//! encodeur qui aurait interverti les deux canaux.
//!
//! **Ce que ce module n'établit PAS** : que le cas de Marco Polo ou celui de
//! Didier s'expliquent ici. Il n'y a ni WiiM Pro, ni SMSL SU-1, ni SU-8, ni Mac
//! sur ce banc. Si toutes les gardes de ce fichier sont vertes, cela veut dire
//! que le porteur part intact de chez nous — et que la cause de leur bruit
//! blanc est en AVAL de ce point, pas dedans.

use crate::audio::decode::decode_dsd_to_dop_streaming;
use crate::audio::dsd_to_dop::DsdToDoP;

/// Un octet de charge utile identifiable : jamais nul (un zéro se confondrait
/// avec du remplissage), dépendant du canal ET de la position, donc un échange
/// de canaux ou un décalage d'un octet se voit.
fn octet_temoin(canal: usize, index: usize) -> u8 {
    (((canal * 97 + index * 31 + (index / 251) * 17) % 251) + 1) as u8
}

/// La charge utile attendue, canal par canal.
fn charge_utile(canaux: usize, octets_par_canal: usize) -> Vec<Vec<u8>> {
    (0..canaux)
        .map(|ch| (0..octets_par_canal).map(|i| octet_temoin(ch, i)).collect())
        .collect()
}

/// Miroir des bits d'un octet — DSF stocke le DSD LSB d'abord, le DoP l'attend
/// MSB d'abord. Copie volontaire du `reverse_bits` privé de
/// [`crate::audio::dsd_to_dop`] : une garde qui appellerait la fonction testée
/// pour calculer son attendu ne garderait rien.
fn miroir(b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        r |= ((b >> i) & 1) << (7 - i);
    }
    r
}

/// Écrit un DSF valide dont la charge utile est celle de [`charge_utile`].
///
/// DSF range le DSD en blocs entrelacés PAR CANAL (`block_size` octets du canal
/// 0, puis `block_size` du canal 1, …), le dernier bloc étant complété de zéros
/// jusqu'à `block_size`. C'est ce remplissage que `DsfStreamReader` doit
/// retrancher, et c'est pour l'éprouver que `octets_par_canal` n'est pas un
/// multiple de `taille_bloc` dans les gardes ci-dessous.
fn ecrire_dsf(chemin: &str, canaux: u32, taille_bloc: u32, octets_par_canal: usize) {
    let payload = charge_utile(canaux as usize, octets_par_canal);
    let bloc = taille_bloc as usize;
    let blocs = octets_par_canal.div_ceil(bloc);
    let mut data = Vec::with_capacity(blocs * bloc * canaux as usize);
    for b in 0..blocs {
        for canal in payload.iter() {
            for i in 0..bloc {
                let idx = b * bloc + i;
                data.push(if idx < octets_par_canal {
                    canal[idx]
                } else {
                    0
                });
            }
        }
    }
    let total_samples = (octets_par_canal as u64) * 8;
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&canaux.to_le_bytes());
    buf.extend_from_slice(&2_822_400u32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&taille_bloc.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

/// Écrit un DSDIFF (`.dff`) non compressé dont la charge utile est celle de
/// [`charge_utile`]. DFF entrelace les octets (`ch0 ch1 ch0 ch1 …`) et stocke
/// le DSD MSB d'abord — pas de miroir de bits à l'encodage.
fn ecrire_dff(chemin: &str, canaux: u32, octets_par_canal: usize) {
    let payload = charge_utile(canaux as usize, octets_par_canal);
    let mut data = Vec::with_capacity(octets_par_canal * canaux as usize);
    for i in 0..octets_par_canal {
        for canal in payload.iter() {
            data.push(canal[i]);
        }
    }
    let mut fver = Vec::new();
    fver.extend_from_slice(b"FVER");
    fver.extend_from_slice(&4u64.to_be_bytes());
    fver.extend_from_slice(&0x0105_0000u32.to_be_bytes());

    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&(2 + 4 * canaux as u64).to_be_bytes());
    prop.extend_from_slice(&(canaux as u16).to_be_bytes());
    for c in 0..canaux {
        prop.extend_from_slice(if c % 2 == 0 { b"SLFT" } else { b"SRGT" });
    }
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");

    let frm8_size = 4 + fver.len() + 12 + prop.len() + 12 + data.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FRM8");
    buf.extend_from_slice(&(frm8_size as u64).to_be_bytes());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&fver);
    buf.extend_from_slice(b"PROP");
    buf.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    buf.extend_from_slice(&prop);
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&(data.len() as u64).to_be_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

/// Fait tourner le chemin de PRODUCTION et rend les octets tels qu'ils partent
/// sur le fil (réseau) ou vers le rappel local — la charge utile de la session
/// WAV, en-tête exclu, exactement ce que `anticiper_le_dop` fait suivre.
///
/// L'encodeur appelle `Handle::block_on` : il doit donc tourner sur un fil
/// ORDINAIRE, hors contexte asynchrone, sans quoi tokio panique. C'est ce que
/// fait la production (`tokio::task::spawn_blocking`), et c'est reproduit ici.
fn octets_dop_du_chemin_reel(chemin: &str, ext: &str) -> Vec<u8> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let handle = rt.handle().clone();
    let p = chemin.to_string();
    let e = ext.to_string();
    let producteur = std::thread::spawn(move || {
        // Le contexte de runtime que `spawn_blocking` fournit en production :
        // sans lui, le `tokio::time::timeout` de l'encodeur panique avant même
        // d'atteindre le `block_on` ("there is no reactor running").
        let _garde = handle.enter();
        let mut premier = false;
        decode_dsd_to_dop_streaming(&p, &e, tx, 65536, &mut premier, &None, &handle)
    });
    let octets = rt.block_on(async move {
        let mut v: Vec<u8> = Vec::new();
        while let Some(bloc) = rx.recv().await {
            v.extend_from_slice(&bloc);
        }
        v
    });
    let (profondeur, cadence) = producteur.join().unwrap().unwrap();
    assert_eq!(profondeur, 24, "un porteur DoP n'a pas d'autre profondeur");
    assert_eq!(
        cadence,
        DsdToDoP::dop_rate(2_822_400),
        "DSD64 se transporte à 176 400 Hz en DoP"
    );
    octets
}

/// Le cœur de la mesure : les octets rendus portent-ils encore un porteur DoP
/// INTACT, et la charge utile DSD s'y relit-elle sans perte ?
///
/// `lsb_first` dit si la source stockait le DSD LSB d'abord (DSF) — auquel cas
/// l'encodeur doit avoir miroité chaque octet.
fn verifier_le_porteur(
    octets: &[u8],
    canaux: usize,
    octets_par_canal: usize,
    lsb_first: bool,
    source: &str,
) {
    let attendu_trames = octets_par_canal / 2;
    let octets_par_trame = 3 * canaux;
    assert_eq!(
        octets.len(),
        attendu_trames * octets_par_trame,
        "{source} : le chemin a rendu {} octets pour {} trames attendues — \
         des octets DSD ont été perdus en route",
        octets.len(),
        attendu_trames
    );

    let payload = charge_utile(canaux, octets_par_canal);
    for f in 0..attendu_trames {
        let base = f * octets_par_trame;
        let marqueur_attendu = if f % 2 == 0 { 0x05u8 } else { 0xFAu8 };
        for ch in 0..canaux {
            let mot = &octets[base + 3 * ch..base + 3 * ch + 3];
            assert_eq!(
                mot[2], marqueur_attendu,
                "{source} : trame {f}, canal {ch} — le marqueur DoP vaut {:#04x} \
                 au lieu de {marqueur_attendu:#04x} ; le DAC ne verrouillera pas",
                mot[2]
            );
            // Le mot de 24 bits est little-endian : [DSD n+1, DSD n, marqueur].
            let (b0, b1) = (payload[ch][2 * f], payload[ch][2 * f + 1]);
            let (d0, d1) = if lsb_first {
                (miroir(b0), miroir(b1))
            } else {
                (b0, b1)
            };
            assert_eq!(
                (mot[0], mot[1]),
                (d1, d0),
                "{source} : trame {f}, canal {ch} — la charge utile DSD ne se \
                 relit pas ; un octet a été perdu, deux canaux échangés ou la \
                 phase de trame rompue"
            );
        }
    }
}

/// TÉMOIN — un DSF stéréo DSD64 traverse le chemin de production et le porteur
/// DoP en ressort intact, du premier octet au dernier.
///
/// `octets_par_canal` n'est PAS un multiple de la taille de bloc : le dernier
/// super-bloc DSF est donc complété de zéros dans le fichier, et
/// `DsfStreamReader` doit les retrancher. Un remplissage qui passerait serait
/// du DSD constant à zéro — un continu pleine échelle, pas du silence.
#[test]
fn un_dsf_stereo_garde_son_porteur_dop_de_bout_en_bout() {
    let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    let chemin = dsf.path().to_str().unwrap().to_string();
    let octets_par_canal = 4096 * 2 + 1500;
    ecrire_dsf(&chemin, 2, 4096, octets_par_canal);
    let octets = octets_dop_du_chemin_reel(&chemin, "dsf");
    verifier_le_porteur(&octets, 2, octets_par_canal, true, "DSF stéréo");
}

/// TÉMOIN — le détecteur DE PRODUCTION reconnaît ce flux.
///
/// C'est lui, et lui seul, qui décide sur une sortie locale que le volume et le
/// DSP sont contournés (`LocalPcmProcessor`) et qu'un bras exclusif Windows
/// doit refuser plutôt que de livrer du DoP reconstruit depuis du flottant
/// (`WindowsExclusivePcmError::DopUnsupported`). Le vérifier ici relie la
/// mesure au code qui s'en sert.
#[cfg(feature = "local-audio")]
#[test]
fn le_detecteur_de_production_reconnait_ce_que_l_encodeur_produit() {
    let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    let chemin = dsf.path().to_str().unwrap().to_string();
    ecrire_dsf(&chemin, 2, 4096, 4096 * 2 + 1500);
    let octets = octets_dop_du_chemin_reel(&chemin, "dsf");
    assert!(
        crate::outputs::local::is_dop_pcm(&octets, 24, 2),
        "le détecteur de production ne voit pas le DoP que la production émet"
    );
    // CONTRE-ÉPREUVE du détecteur lui-même : un seul octet de poids fort
    // écrasé — ce que fait le moindre traitement d'échantillon — et il refuse.
    let mut abime = octets.clone();
    abime[2] = 0x00;
    assert!(
        !crate::outputs::local::is_dop_pcm(&abime, 24, 2),
        "un marqueur détruit doit être vu comme tel, sinon la garde ne garde rien"
    );
}

/// TÉMOIN — le même chemin, en DFF (MSB d'abord, octets entrelacés).
#[test]
fn un_dff_stereo_garde_son_porteur_dop_de_bout_en_bout() {
    let dff = tempfile::Builder::new().suffix(".dff").tempfile().unwrap();
    let chemin = dff.path().to_str().unwrap().to_string();
    let octets_par_canal = 40_000;
    ecrire_dff(&chemin, 2, octets_par_canal);
    let octets = octets_dop_du_chemin_reel(&chemin, "dff");
    verifier_le_porteur(&octets, 2, octets_par_canal, false, "DFF stéréo");
}

/// LE DÉFAUT — un DFF MULTICANAL perdait un octet DSD par canal et par bloc lu.
///
/// `decode_dsd_to_dop_streaming` lit le DFF par tranches de
/// `32768 / canaux * canaux` octets : un multiple du nombre de canaux, comme le
/// documente `DffStreamReader::open`. Mais [`DsdToDoP::feed`] consomme
/// **deux** octets par canal et par trame, et rendait la tranche restante :
/// pour six canaux, 32 766 = 2 730 × 12 + 6, soit six octets — un par canal —
/// jetés toutes les 2,7 ms de DSD64, et cela sur toute la piste.
///
/// Ce n'est pas le bruit blanc de #1894 ni celui de #2369, tous deux stéréo :
/// c'est un défaut voisin, trouvé par la mesure, et la garde reste utile
/// au-delà du multicanal — elle vaut pour toute taille de tranche à venir.
#[test]
fn un_dff_multicanal_ne_perd_plus_un_octet_par_bloc() {
    let dff = tempfile::Builder::new().suffix(".dff").tempfile().unwrap();
    let chemin = dff.path().to_str().unwrap().to_string();
    // Trois tranches de lecture pleines : sans le report, trois fois six
    // octets manquent, et le décalage se voit dès la première.
    let octets_par_canal = 32766 / 6 * 3;
    ecrire_dff(&chemin, 6, octets_par_canal);
    let octets = octets_dop_du_chemin_reel(&chemin, "dff");
    verifier_le_porteur(&octets, 6, octets_par_canal, false, "DFF 6 canaux");
}
