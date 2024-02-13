use std::{
    cmp::min,
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::{arg, Parser, Subcommand};
use distrans_fileindex::Index;
use flume::{unbounded, Receiver, Sender};
use tracing::{info, trace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use veilid_core::{
    CryptoKey, CryptoTyped, DHTRecordDescriptor, DHTSchema, DHTSchemaDFLT, KeyPair, RouteId,
    RoutingContext, Sequencing, VeilidUpdate,
};

use distrans::{encode_index, other_err, veilid_config, Error, Result};

#[derive(Parser, Debug)]
#[command(name = "distrans")]
#[command(bin_name = "distrans")]
struct Cli {
    #[arg(long, env)]
    pub state_dir: String,

    #[command(subcommand)]
    pub commands: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Get { dht_key: String, file: String },
    Post { file: String },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            EnvFilter::builder()
                .with_default_directive("stress=debug".parse().unwrap())
                .from_env_lossy(),
        )
        .init();

    let cli = Cli::parse();

    let mut app = App::new(PathBuf::from(cli.state_dir).as_path())
        .await
        .expect("new app");
    app.wait_for_network().await.expect("network");

    match cli.commands {
        Commands::Get { dht_key, file } => {
            todo!("get")
            // Open the DHT key
            // Fetch the packed index
            // Decode the index
            // Fetch the pieces, write the file
        }
        Commands::Post { file } => {
            app.post(&file).await.expect("post");
        }
    }
}

#[derive(Clone)]
struct App {
    routing_context: RoutingContext,
    updates: Receiver<VeilidUpdate>,
    dht_record: Option<DHTRecordDescriptor>,
    route_id: Option<RouteId>,
}

impl App {
    pub async fn new(path: &Path) -> Result<App> {
        let (routing_context, updates) = Self::new_routing_context(path).await?;
        Ok(App {
            routing_context,
            updates,
            dht_record: None,
            route_id: None,
        })
    }

    pub async fn post(&mut self, file: &str) -> Result<()> {
        // Index the file
        let index = Index::from_file(file.into()).await?;

        // Encode the index
        let index_bytes = encode_index(&index)?;

        // Create / open a DHT record for the payload digest
        let dht_rec = self
            .open_or_create_dht_record(index.payload().digest(), index_bytes.len())
            .await?;

        // Set the subkey values
        self.set_index(dht_rec.key(), index_bytes.as_slice())
            .await?;

        todo!("post")
    }

    async fn new_routing_context(
        state_path: &Path,
    ) -> Result<(RoutingContext, Receiver<VeilidUpdate>)> {
        // Veilid API state channel
        let (node_sender, updates): (Sender<VeilidUpdate>, Receiver<VeilidUpdate>) = unbounded();

        // Start up Veilid core
        let update_callback = Arc::new(move |change: VeilidUpdate| {
            let _ = node_sender.send(change);
        });
        let config_state_path = Arc::new(state_path.to_owned());
        let config_callback = Arc::new(move |key| {
            veilid_config::callback(config_state_path.to_str().unwrap().to_string(), key)
        });

        let api: veilid_core::VeilidAPI =
            veilid_core::api_startup(update_callback, config_callback).await?;
        api.attach().await?;

        let routing_context = api
            .routing_context()?
            .with_sequencing(Sequencing::EnsureOrdered)
            .with_default_safety()?;
        Ok((routing_context, updates))
    }

    pub async fn wait_for_network(&mut self) -> Result<()> {
        // Wait for network to be up
        loop {
            let res = self.updates.recv_async().await;
            match res {
                Ok(VeilidUpdate::Attachment(attachment)) => {
                    if attachment.public_internet_ready {
                        info!(
                            state = attachment.state.to_string(),
                            public_internet_ready = attachment.public_internet_ready,
                            "Connected"
                        );
                        break;
                    }
                    info!(
                        state = attachment.state.to_string(),
                        public_internet_ready = attachment.public_internet_ready,
                        "Waiting for network"
                    );
                }
                Ok(u) => {
                    trace!(update = format!("{:?}", u));
                }
                Err(e) => {
                    return Err(Error::Other(e.to_string()));
                }
            };
        }
        Ok(())
    }

    async fn open_or_create_dht_record(
        &mut self,
        digest: &[u8],
        index_length: usize,
    ) -> Result<DHTRecordDescriptor> {
        let ts = self.routing_context.api().table_store()?;
        let db = ts.open("distrans_payload_dht", 2).await?;
        let maybe_dht_key = db.load_json(0, digest).await?;
        let maybe_dht_owner_keypair = db.load_json(1, &[]).await?;
        if let (Some(dht_key), Some(dht_owner_keypair)) = (maybe_dht_key, maybe_dht_owner_keypair) {
            return Ok(self
                .routing_context
                .open_dht_record(dht_key, dht_owner_keypair)
                .await?);
        }
        let dht_rec = self
            .routing_context
            .create_dht_record(
                DHTSchema::DFLT(DHTSchemaDFLT {
                    o_cnt: (index_length / 32768 + 1) as u16,
                }),
                None,
            )
            .await?;
        let dht_owner = KeyPair::new(
            dht_rec.owner().to_owned(),
            dht_rec
                .owner_secret()
                .ok_or(other_err("expected dht owner secret"))?
                .to_owned(),
        );
        db.store_json(0, &[], dht_rec.key()).await?;
        db.store_json(1, &[], &dht_owner).await?;
        Ok(dht_rec)
    }

    async fn set_index(&self, dht_key: &CryptoTyped<CryptoKey>, index_bytes: &[u8]) -> Result<()> {
        let mut subkey = 0;
        let mut i: usize = 0;
        while index_bytes.len() > 0 {
            let take = min(32768, index_bytes.len() - i);
            self.routing_context
                .set_dht_value(dht_key.clone(), subkey, index_bytes[i..i + take].to_vec())
                .await?;
            i += take;
        }
        Ok(())
    }
}
