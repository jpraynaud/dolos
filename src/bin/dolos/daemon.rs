use miette::{Context, IntoDiagnostic};

#[derive(Debug, clap::Args)]
pub struct Args {}

#[tokio::main]
pub async fn run(config: super::Config, _args: &Args) -> miette::Result<()> {
    crate::common::setup_tracing(&config.logging)?;

    let (wal, chain, ledger) = crate::common::open_data_stores(&config)?;

    let byron_genesis = pallas::ledger::configs::byron::from_file(&config.byron.path)
        .into_diagnostic()
        .context("loading byron genesis config")?;

    let shelley_genesis = pallas::ledger::configs::shelley::from_file(&config.shelley.path)
        .into_diagnostic()
        .context("loading shelley genesis config")?;

    let server = tokio::spawn(dolos::serve::serve(
        config.serve,
        wal.clone(),
        chain.clone(),
    ));

    dolos::sync::pipeline(
        &config.upstream,
        wal,
        chain,
        ledger,
        byron_genesis,
        shelley_genesis,
        &config.retries,
    )
    .into_diagnostic()
    .context("bootstrapping sync pipeline")?
    .block();

    server.abort();

    Ok(())
}
