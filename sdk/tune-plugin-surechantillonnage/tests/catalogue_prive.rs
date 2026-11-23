//! The public host stores arbitrary plugin selections without enumerating them.
#[test]
fn aucun_identifiant_du_catalogue_dans_le_contrat_public() {
    let sources = [
        include_str!("../src/sdk.rs"),
        include_str!("greffon.rs"),
        include_str!("../../../tune-core/src/audio/surechantillonnage.rs"),
        include_str!("../../../tune-core/src/outputs/local/surechantillonnage_7614.rs"),
        include_str!("../../../tune-core/src/tess/chaine.rs"),
        include_str!("../../../tune-core/src/tess/migration.rs"),
        include_str!("../../../tune-core/src/tess/migration_tests.rs"),
        include_str!("../../../tune-core/src/tess/resolution.rs"),
        include_str!("../../../tune-server/src/routes/upsampler.rs"),
        include_str!("../../../tune-server/src/routes/upsampler_tests.rs"),
    ];
    for source in sources {
        for line in source.lines() {
            let lower = line.to_ascii_lowercase();
            for id in ["natural", "impact", "texture"] {
                assert!(
                    !lower
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|word| word == id),
                    "{line}"
                );
            }
            for key in ["brand", "family", "device", "interface"] {
                // Identifiers and wire keys. Prose about sample-rate families
                // and the unrelated volume-control enum are legitimate here.
                for spelling in [
                    format!("\"{key}\""),
                    format!(" {key}:"),
                    format!(".{key}"),
                    format!(" {key},"),
                ] {
                    assert!(!line.contains(&spelling), "{line}");
                }
            }
        }
    }
}
