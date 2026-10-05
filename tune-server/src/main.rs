//! Binary entry point.
//!
//! Deliberately empty of logic: everything lives in [`tune_server::run`] so a
//! downstream binary can compose the same startup with its own plugins. See
//! `bootstrap.rs`.
//!
//! Le moteur n'est plus `#[tokio::main]` : il prend au moins 4 fils de travail
//! (cf. [`tune_server::fils_de_travail`]).

fn main() {
    tune_server::fils_de_travail::construire_le_moteur().block_on(tune_server::run(None));
}
