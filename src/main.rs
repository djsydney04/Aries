use clap::Parser;
use codex_mux::app::run_app;
use codex_mux::model::Args;

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    run_app(args)
}
