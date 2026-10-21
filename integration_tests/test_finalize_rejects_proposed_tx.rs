//! bip300301_enforcer#634: a mempool tx can pass admission and later fail the
//! enforcer's dry run at `finalize_block_template`. That must not fail every
//! `getblocktemplate` until a restart: the template is rebuilt without the tx
//! and its descendants, and unrelated txs stay in.

use std::{net::SocketAddr, time::Duration};

use bitcoin::Network;
use bitcoin_jsonrpsee::{
    MainClient,
    client::{BlockTemplate, BlockTemplateRequest},
    jsonrpsee,
};
use cusf_enforcer_mempool::server::{RpcClient, RpcServer as _, Server};

use crate::{
    setup::TestSetup,
    util::{
        get_new_address, send_to_address, spend_wallet_output,
        wait_for_mempool_pred,
    },
};

async fn get_template(
    client: &jsonrpsee::http_client::HttpClient,
) -> anyhow::Result<BlockTemplate> {
    RpcClient::get_block_template(client, BlockTemplateRequest::default())
        .await?
        .into_template()
        .map(|template| *template)
        .ok_or_else(|| {
            anyhow::anyhow!("expected a template, got a proposal verdict")
        })
}

pub async fn test_finalize_rejects_proposed_tx(
    setup: TestSetup,
) -> anyhow::Result<()> {
    let TestSetup {
        node,
        mempool_sync,
        enforcer,
        task_errors: _task_errors,
    } = setup;
    let rpc = &node.rpc_client;

    // C first, so that it cannot spend A's change: C stays unrelated to A.
    let unrelated =
        send_to_address(rpc, &get_new_address(rpc).await?, 500_000).await?;
    let rejected_dest = get_new_address(rpc).await?;
    let rejected = send_to_address(rpc, &rejected_dest, 1_000_000).await?;
    enforcer.reject_at_finalize(rejected);
    let child = spend_wallet_output(rpc, rejected, &rejected_dest, 1_000).await?;
    wait_for_mempool_pred(
        rpc,
        Duration::from_secs(5),
        |txids| {
            txids.contains(&unrelated)
                && txids.contains(&rejected)
                && txids.contains(&child)
        },
        "all three txs in bitcoind's mempool",
    )
    .await?;

    let network_info = rpc.get_network_info().await?;
    let sample_block_template =
        MainClient::get_block_template(rpc, BlockTemplateRequest::default())
            .await?;
    let server = Server::new(
        node.mining_address.script_pubkey(),
        mempool_sync,
        Network::Regtest,
        network_info,
        rpc.clone(),
        None,
        sample_block_template,
    )?;
    let rpc_server = jsonrpsee::server::Server::builder()
        .build("127.0.0.1:0".parse::<SocketAddr>()?)
        .await?;
    let rpc_addr = rpc_server.local_addr()?;
    // Dropping the handle stops the server — keep it alive for the test.
    let _rpc_server_handle = rpc_server.start(server.into_rpc());
    let client = jsonrpsee::http_client::HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(30))
        .build(format!("http://{rpc_addr}"))?;

    // The sync task picks the txs up asynchronously: ask until the unrelated
    // tx is proposed. Without the fix every template that proposes the
    // rejected tx fails, so this never succeeds.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let template = loop {
        let last = match get_template(&client).await {
            Ok(template)
                if template
                    .transactions
                    .iter()
                    .any(|tx| tx.txid == unrelated) =>
            {
                break template;
            }
            Ok(_) => anyhow::anyhow!("template without the unrelated tx"),
            Err(err) => err,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(last.context(
                "no template with the unrelated tx before the deadline",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    for (txid, label) in [(rejected, "rejected tx"), (child, "its child")] {
        anyhow::ensure!(
            !template.transactions.iter().any(|tx| tx.txid == txid),
            "template still proposes the {label} `{txid}`"
        );
    }
    Ok(())
}
