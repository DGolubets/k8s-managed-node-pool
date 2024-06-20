mod digital_ocean;

use anyhow::Error;
use digital_ocean::DigitalOcean;
use k8s_managed_node_pool::{client::DefaultKubeClient, operator::Operator};
use kube::Client;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();
    tracing::debug!("Starting..");

    let client = Client::try_default().await?;
    let client = DefaultKubeClient { client };
    let client = Arc::new(client);

    let cloud_provider = DigitalOcean::new(de_env::from_env()?);
    let cloud_provider = Arc::new(cloud_provider);

    let operator = Operator {
        client,
        cloud_provider,
    };

    operator.run().await?;

    tracing::debug!("Terminating..");

    Ok(())
}
