//! Binary entry point.
//!
//! Deliberately empty of logic: everything lives in [`tune_server::run`] so a
//! downstream binary can compose the same startup with its own plugins. See
//! `bootstrap.rs`.

#[tokio::main]
async fn main() {
    tune_server::run(None).await;
}
