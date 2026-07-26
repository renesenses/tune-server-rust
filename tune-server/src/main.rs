//! Stock tune-server binary: the full bootstrap lives in `run::main_blocking`
//! so that composer binaries (private output modules, e.g. tune-diretta) can
//! run the same server with extra `OutputProvider`s — see tune-server/src/run.rs.

fn main() {
    tune_server::run::main_blocking(tune_server::run::RunOptions::default());
}
