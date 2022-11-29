use std::{io::Cursor, sync::Arc};

use anyhow::Result;
use cid::Cid;
use libipld_cbor::DagCborCodec;
use noosphere::sphere::SphereContext;
use noosphere_core::{
    data::{ContentType, Did},
    view::{Sphere, Timeline},
};
use noosphere_storage::{
    BlockStore,
    block_deserialize, block_serialize,
    KeyValueStore,
    Storage,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncReadExt,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use ucan::crypto::KeyMaterial;
use url::Url;

use iroh_car::{CarHeader, CarWriter};
use wnfs::private::BloomFilter;

use crate::native::commands::{config::COUNTERPART, serve::ipfs::IpfsClient};

use super::KuboClient;

/// A [SyndicationJob] is a request to syndicate the blocks of a _counterpart_
/// sphere to the broader IPFS network.
pub struct SyndicationJob<K, S>
where
    K: KeyMaterial + Clone + 'static,
    S: Storage,
{
    /// The revision of the _local_ sphere to discover the _counterpart_ sphere
    /// from; the counterpart sphere's revision will need to be derived using
    /// this checkpoint in local sphere history.
    pub revision: Cid,
    /// The [SphereContext] that corresponds to the _local_ sphere.
    pub context: Arc<Mutex<SphereContext<K, S>>>,
}

/// A [SyndicationCheckpoint] represents the last spot in the history of a
/// sphere that was successfully syndicated to an IPFS node. It records a Bloom
/// filter populated by the CIDs of all blocks that have been syndicated, which
/// gives us a short-cut to determine if a block should be added.
#[derive(Serialize, Deserialize)]
pub struct SyndicationCheckpoint {
    pub revision: Cid,
    pub syndicated_blocks: BloomFilter<256, 30>,
}

/// Start a Tokio task that waits for [SyndicationJob] messages and then
/// attempts to syndicate to the configured IPFS RPC. Currently only Kubo IPFS
/// backends are supported.
pub fn start_ipfs_syndication<K, S>(
    ipfs_api: Url,
) -> (
    UnboundedSender<SyndicationJob<K, S>>,
    JoinHandle<Result<()>>,
)
where
    K: KeyMaterial + Clone + 'static,
    S: Storage + 'static,
{
    let (tx, rx) = unbounded_channel();

    (tx, tokio::task::spawn(ipfs_syndication_task(ipfs_api, rx)))
}

async fn ipfs_syndication_task<K, S>(
    ipfs_api: Url,
    mut receiver: UnboundedReceiver<SyndicationJob<K, S>>,
) -> Result<()>
where
    K: KeyMaterial + Clone + 'static,
    S: Storage,
{
    debug!("Syndicating sphere revisions to IPFS API at {}", ipfs_api);

    let kubo_client = Arc::new(KuboClient::new(&ipfs_api)?);

    while let Some(SyndicationJob { revision, context }) = receiver.recv().await {
        let kubo_identity = match kubo_client.server_identity().await {
            Ok(id) => id,
            Err(error) => {
                warn!(
                    "Failed to identify an IPFS Kubo node at {}: {}",
                    ipfs_api, error
                );
                continue;
            }
        };
        let checkpoint_key = format!("syndication/kubo/{kubo_identity}");

        // Take a lock on the `SphereContext` and look up the most recent
        // syndication checkpoint for this Kubo node
        let (sphere_revision, ancestor_revision, mut syndicated_blocks, db) = {
            let context = context.lock().await;
            let db = context.db().clone();
            let counterpart_identity = context.db().require_key::<_, Did>(COUNTERPART).await?;

            let sphere = Sphere::at(&revision, context.db());
            let links = sphere.try_get_links().await?;

            let counterpart_revision = *links.require(&counterpart_identity).await?;

            let fs = context.fs().await?;

            let (last_syndicated_revision, syndicated_blocks) =
                match fs.read(&checkpoint_key).await? {
                    Some(mut file) => match file.memo.content_type() {
                        Some(ContentType::Cbor) => {
                            let mut bytes = Vec::new();
                            file.contents.read_to_end(&mut bytes).await?;
                            let SyndicationCheckpoint {
                                revision,
                                syndicated_blocks,
                            } = block_deserialize::<DagCborCodec, _>(&bytes)?;
                            (Some(revision), syndicated_blocks)
                        }
                        _ => (None, BloomFilter::default()),
                    },
                    None => (None, BloomFilter::default()),
                };

            (
                counterpart_revision,
                last_syndicated_revision,
                syndicated_blocks,
                db,
            )
        };

        let timeline = Timeline::new(&db)
            .slice(&sphere_revision, ancestor_revision.as_ref())
            .try_to_chronological()
            .await?;

        // For all CIDs since the last historical checkpoint, syndicate a CAR
        // of blocks that are unique to that revision to the backing IPFS
        // implementation
        for (cid, _) in timeline {
            // TODO(#175): At each increment, if there are sub-graphs of a
            // sphere that should *not* be syndicated (e.g., other spheres
            // referenced by this sphere that are probably syndicated
            // elsewhere), we should add them to the bloom filter at this spot.

            let stream = db.query_links(&cid, {
                let filter = Arc::new(syndicated_blocks.clone());
                let kubo_client = kubo_client.clone();

                move |cid| {
                    let filter = filter.clone();
                    let kubo_client = kubo_client.clone();
                    let cid = *cid;

                    async move {
                        // The Bloom filter probabilistically tells us if we
                        // have syndicated a block; it is probabilistic because
                        // `contains` may give us false positives. But, all
                        // negatives are guaranteed to not have been added. So,
                        // we can rely on it as a short cut to find unsyndicated
                        // blocks, and for positives we can verify the pin
                        // status with the IPFS node.
                        if !filter.contains(&cid.to_bytes()) {
                            return Ok(true);
                        }

                        // This will probably end up being rather noisy for the
                        // IPFS node, but hopefully checking for a pin is not
                        // overly costly. We may have to come up with a
                        // different strategy if this turns out to be too noisy.
                        Ok(!kubo_client.block_is_pinned(&cid).await?)
                    }
                }
            });

            // TODO(#2): It would be cool to make reading from storage and
            // writing to an HTTP request body concurrent / streamed; this way
            // we could send over CARs of arbitrary size (within the limits of
            // whatever the IPFS receiving implementation can support).
            let mut car = Vec::new();
            let car_header = CarHeader::new_v1(vec![cid]);
            let mut car_writer = CarWriter::new(car_header, &mut car);

            tokio::pin!(stream);

            while let Some(cid) = stream.try_next().await? {
                // TODO(#176): We need to build-up a list of blocks that aren't
                // able to be loaded so that we can be resilient to incomplete
                // data when syndicating to IPFS
                syndicated_blocks.add(&cid.to_bytes());

                let block = db.require_block(&cid).await?;

                car_writer.write(cid, block).await?;
            }

            kubo_client.syndicate_blocks(Cursor::new(car)).await?;

            debug!("Syndicated sphere revision {} to IPFS", cid);
        }

        // At the end, take another lock on the `SphereContext` in order to
        // update the syndication checkpoint for this particular IPFS server
        {
            let context = context.lock().await;
            let mut fs = context.fs().await?;
            let (_, bytes) = block_serialize::<DagCborCodec, _>(&SyndicationCheckpoint {
                revision,
                syndicated_blocks,
            })?;

            fs.write(
                &kubo_identity.to_string(),
                &ContentType::Cbor.to_string(),
                Cursor::new(bytes),
                None,
            )
            .await?;

            fs.save(None).await?;
        }
    }

    Ok(())
}
