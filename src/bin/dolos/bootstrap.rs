use std::{path::Path, sync::Arc};

use miette::{bail, Context, IntoDiagnostic};
use mithril_client::{ClientBuilder, MessageBuilder};
use pallas::ledger::traverse::MultiEraBlock;
use tracing::{debug, info, trace, warn};

use dolos::prelude::*;

use crate::common::Stores;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long)]
    mithril_aggregator: String,

    #[arg(long)]
    mithril_genesis_key: String,

    #[arg(long)]
    download_dir: String,

    /// Skip the bootstrap if there's already data in the stores
    #[arg(long, short, action)]
    skip_if_not_empty: bool,

    /// Delete any existing data and continue with bootstrap
    #[arg(long, short, action)]
    force: bool,

    /// Retain downloaded snapshot instead of deleting it
    #[arg(long, short, action)]
    retain_snapshot: bool,
}

struct Feedback {}

#[async_trait::async_trait]
impl mithril_client::feedback::FeedbackReceiver for Feedback {
    async fn handle_event(&self, event: mithril_client::feedback::MithrilEvent) {
        match event {
            mithril_client::feedback::MithrilEvent::SnapshotDownloadStarted { .. } => {
                info!("snapshot download started")
            }
            mithril_client::feedback::MithrilEvent::SnapshotDownloadProgress {
                downloaded_bytes,
                size,
                ..
            } => {
                let percent = downloaded_bytes as f64 / size as f64;
                debug!(percent, "snapshot download progress");
            }
            mithril_client::feedback::MithrilEvent::SnapshotDownloadCompleted { .. } => {
                trace!("snapshot download completed");
            }
            mithril_client::feedback::MithrilEvent::CertificateChainValidationStarted {
                ..
            } => info!("certificate chain validation started"),
            mithril_client::feedback::MithrilEvent::CertificateValidated {
                certificate_chain_validation_id: id,
                certificate_hash: hash,
            } => {
                info!(id, hash, "certificate validated")
            }
            mithril_client::feedback::MithrilEvent::CertificateChainValidated { .. } => {
                info!("certificate chain validated")
            }
        }
    }
}

async fn fetch_and_validate_snapshot(args: &Args) -> Result<(), mithril_client::MithrilError> {
    let feedback = Arc::new(Feedback {});

    let client = ClientBuilder::aggregator(&args.mithril_aggregator, &args.mithril_genesis_key)
        .add_feedback_receiver(feedback.clone())
        .build()?;

    let snapshots = client.snapshot().list().await?;

    let last_digest = snapshots.first().unwrap().digest.as_ref();
    let snapshot = client.snapshot().get(last_digest).await?.unwrap();

    let certificate = client
        .certificate()
        .verify_chain(&snapshot.certificate_hash)
        .await?;

    let target_directory = Path::new(&args.download_dir);

    client
        .snapshot()
        .download_unpack(&snapshot, target_directory)
        .await?;

    let message = MessageBuilder::new()
        .compute_snapshot_message(&certificate, target_directory)
        .await?;

    assert!(certificate.match_message(&message));

    Ok(())
}

fn open_empty_stores(config: &super::Config, force: bool) -> miette::Result<Option<Stores>> {
    let mut stores = crate::common::open_data_stores(config)?;

    let empty = stores.0.is_empty() && stores.1.is_empty() && stores.2.is_empty();

    match (empty, force) {
        (true, _) => Ok(Some(stores)),
        (false, true) => {
            drop(stores);

            crate::common::destroy_data_stores(config)
                .context("destroying existing data stored")?;

            stores = crate::common::open_data_stores(config)?;
            Ok(Some(stores))
        }
        (false, false) => Ok(None),
    }
}

pub fn run(config: &super::Config, args: &Args) -> miette::Result<()> {
    crate::common::setup_tracing(&config.logging)?;

    let empty_stores =
        open_empty_stores(config, args.force).context("opening empty data stored")?;

    if empty_stores.is_none() && args.skip_if_not_empty {
        warn!("data stores are not empty, skipping bootstrap");
        return Ok(());
    } else if empty_stores.is_none() {
        bail!("data stores must be empty to execute bootstrap");
    }

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fetch_and_validate_snapshot(args))
        .map_err(|err| miette::miette!(err.to_string()))
        .context("fetching and validating mithril snapshot")?;

    let byron_genesis =
        pallas::ledger::configs::byron::from_file(&config.byron.path).map_err(Error::config)?;

    let immutable_path = Path::new(&args.download_dir).join("immutable");

    let iter = pallas::storage::hardano::immutable::read_blocks(&immutable_path)
        .into_diagnostic()
        .context("reading immutable db")?;

    let (mut wal, mut chain, mut ledger) = empty_stores.unwrap();

    ledger
        .apply_origin(&byron_genesis)
        .into_diagnostic()
        .context("applying origin utxos")?;

    for block in iter {
        let block = match block {
            Ok(x) => x,
            Err(err) => {
                dbg!(err);
                warn!("can't continue reading from immutable db");
                break;
            }
        };

        let blockd = MultiEraBlock::decode(&block)
            .into_diagnostic()
            .context("decoding block cbor")?;

        debug!(slot = blockd.slot(), "importing block");

        chain
            .roll_forward(blockd.slot(), blockd.hash(), block.clone())
            .into_diagnostic()
            .context("adding chain entry")?;

        ledger
            .apply_block(&blockd)
            .into_diagnostic()
            .context("applyting ledger block")?;
    }

    let (tip, _) = chain
        .find_tip()
        .into_diagnostic()
        .context("reading chain tip")?
        .ok_or(miette::miette!("no tip found after bootstrap"))?;

    // TODO: apply real formula for volatile safe margin
    let volatile_start = tip - 1000;

    let volatile = chain.crawl_after(Some(volatile_start));

    for block in volatile {
        let (slot, hash) = block.into_diagnostic()?;

        debug!(slot, "filling up wal");

        let body = chain
            .get_block(hash)
            .into_diagnostic()?
            .ok_or(miette::miette!("block not found"))?;

        wal.roll_forward(slot, hash, body).into_diagnostic()?;
    }

    if !args.retain_snapshot {
        info!("deleting downloaded snapshot");

        std::fs::remove_dir_all(Path::new(&args.download_dir))
            .into_diagnostic()
            .context("removing downloaded snapshot")?;
    }

    Ok(())
}
