//! Déterminisme de la mesure ReplayGain / DR d'une plateforme à l'autre (#5594, lot 0).
//!
//! Le partage communautaire des mesures suppose qu'un même PCM donne la même
//! sonie, les mêmes pics et le même DR sur toutes les instances. Ce binaire
//! sort, pour chaque fichier, ce que la passe ReplayGain écrirait — par le
//! chemin de production `mesurer_intensite_plage_et_empreinte` — sous une
//! forme comparable octet par octet entre deux machines :
//!
//! ```text
//! cargo run --release -p tune-core --example mesure_determinisme_5594 -- <fichier>... > mesures-<plateforme>.tsv
//! ```
//!
//! Colonnes : nom de fichier, LUFS, pic d'échantillon, true peak (linéaires),
//! DR, puis les mêmes trois flottants en bits IEEE 754 (hexadécimal). Le
//! `Debug` d'un `f64` fait l'aller-retour sans perte : deux sorties égales au
//! `cmp` près sont égales au bit près.

fn main() {
    let fichiers: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    println!("fichier\tlufs\tpeak\ttrue_peak\tdr\tlufs_bits\tpeak_bits\ttp_bits");
    for f in &fichiers {
        let nom = std::path::Path::new(f)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| f.clone());
        let m = rt.block_on(tune_core::audio::analyzer::mesurer_intensite_plage_et_empreinte(f));
        match m.mesure {
            Some((lufs, peak, tp, dr)) => println!(
                "{nom}\t{lufs:?}\t{peak:?}\t{tp:?}\t{}\t{:016x}\t{:016x}\t{:016x}",
                dr.map(|d| d.to_string()).unwrap_or_else(|| "-".into()),
                lufs.to_bits(),
                peak.to_bits(),
                tp.to_bits()
            ),
            None => println!("{nom}\t-\t-\t-\t-\t-\t-\t-"),
        }
    }
}
