use ethers::{
    providers::Middleware,
    types::{
        Log,
        SyncingStatus,
        U256,
    },
};
use fuel_core::{
    database::Database,
    relayer,
    service::{
        Config,
        FuelService,
        ServiceTrait,
    },
};
use fuel_core_client::client::{
    pagination::{
        PageDirection,
        PaginationRequest,
    },
    types::TransactionStatus,
    FuelClient,
};
use fuel_core_relayer::{
    test_helpers::{
        middleware::MockMiddleware,
        EvtToLog,
        LogTestHelper,
    },
    H160,
};
use fuel_core_storage::{
    tables::Messages,
    StorageAsRef,
};
use fuel_core_types::{
    fuel_asm::*,
    fuel_crypto::*,
    fuel_tx::*,
    fuel_types::Nonce,
};
use hyper::{
    service::{
        make_service_fn,
        service_fn,
    },
    Body,
    Request,
    Response,
    Server,
};
use rand::{
    prelude::StdRng,
    Rng,
    SeedableRng,
};
use serde_json::json;
use std::{
    convert::Infallible,
    net::{
        Ipv4Addr,
        SocketAddr,
    },
    sync::Arc,
};
use tokio::sync::oneshot::Sender;

#[tokio::test(flavor = "multi_thread")]
async fn relayer_can_download_logs() {
    let mut config = Config::local_node();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];
    let message = |nonce, block_number: u64| {
        make_message_event(
            Nonce::from(nonce),
            block_number,
            contract_address,
            None,
            None,
            None,
            None,
        )
    };

    let logs = vec![message(1, 3), message(2, 5)];
    let expected_messages: Vec<_> = logs.iter().map(|l| l.to_msg()).collect();
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    );
    let db = Database::in_memory();

    let srv = FuelService::from_database(db.clone(), config)
        .await
        .unwrap();

    // wait for relayer to catch up
    srv.await_relayer_synced().await.unwrap();

    // check the db for downloaded messages
    for msg in expected_messages {
        assert_eq!(
            *db.storage::<Messages>().get(msg.id()).unwrap().unwrap(),
            msg
        );
    }
    srv.stop_and_await().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_are_spendable_after_relayer_is_synced() {
    let mut rng = StdRng::seed_from_u64(1234);
    let mut config = Config::local_node();
    config.relayer = Some(relayer::Config::default());
    let relayer_config = config.relayer.as_mut().expect("Expected relayer config");
    let eth_node = MockMiddleware::default();
    let contract_address = relayer_config.eth_v2_listening_contracts[0];

    // setup a real spendable message
    let secret_key: SecretKey = rng.gen();
    let pk = secret_key.public_key();
    let recipient = Input::owner(&pk);
    let sender = Address::zeroed();
    let amount = 100;
    let nonce = Nonce::from(2u64);
    let logs = vec![make_message_event(
        nonce,
        5,
        contract_address,
        Some(sender.into()),
        Some(recipient.into()),
        Some(amount),
        None,
    )];
    eth_node.update_data(|data| data.logs_batch = vec![logs.clone()]);
    // Setup the eth node with a block high enough that there
    // will be some finalized blocks.
    eth_node.update_data(|data| data.best_block.number = Some(200.into()));
    let eth_node = Arc::new(eth_node);
    let eth_node_handle = spawn_eth_node(eth_node).await;

    relayer_config.relayer = Some(
        format!("http://{}", eth_node_handle.address)
            .as_str()
            .try_into()
            .unwrap(),
    );

    config.utxo_validation = true;

    // setup fuel node with mocked eth url
    let db = Database::in_memory();

    let srv = FuelService::from_database(db.clone(), config)
        .await
        .unwrap();

    let client = FuelClient::from(srv.bound_address);

    // wait for relayer to catch up to eth node
    srv.await_relayer_synced().await.unwrap();

    // verify we have downloaded the message
    let query = client
        .messages(
            None,
            PaginationRequest {
                cursor: None,
                results: 1,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    // we should have one message before spending
    assert_eq!(query.results.len(), 1);

    // attempt to spend the message downloaded from the relayer
    let tx = TransactionBuilder::script(vec![op::ret(0)].into_iter().collect(), vec![])
        .gas_limit(10_000)
        .gas_price(0)
        .add_unsigned_message_input(secret_key, sender, nonce, amount, vec![])
        .add_output(Output::change(rng.gen(), 0, AssetId::BASE))
        .finalize();

    let status = client
        .submit_and_await_commit(&tx.clone().into())
        .await
        .unwrap();

    // verify transaction executed successfully
    assert!(
        matches!(&status, &TransactionStatus::Success { .. }),
        "{:?}",
        &status
    );

    // verify message state is spent
    let query = client
        .messages(
            None,
            PaginationRequest {
                cursor: None,
                results: 1,
                direction: PageDirection::Forward,
            },
        )
        .await
        .unwrap();
    // there should be no messages after spending
    assert_eq!(query.results.len(), 0);

    srv.stop_and_await().await.unwrap();
    eth_node_handle.shutdown.send(()).unwrap();
}

fn make_message_event(
    nonce: Nonce,
    block_number: u64,
    contract_address: H160,
    sender: Option<[u8; 32]>,
    recipient: Option<[u8; 32]>,
    amount: Option<u64>,
    data: Option<Vec<u8>>,
) -> Log {
    let message = fuel_core_relayer::bridge::MessageSentFilter {
        nonce: U256::from_big_endian(nonce.as_ref()),
        sender: sender.unwrap_or_default(),
        recipient: recipient.unwrap_or_default(),
        amount: amount.unwrap_or_default(),
        data: data.map(Into::into).unwrap_or_default(),
    };
    let mut log = message.into_log();
    log.address = contract_address;
    log.block_number = Some(block_number.into());
    log
}

async fn spawn_eth_node(eth_node: Arc<MockMiddleware>) -> EthNodeHandle {
    // Construct our SocketAddr to listen on...
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

    // And a MakeService to handle each connection...
    let make_service = make_service_fn(move |_conn| {
        let eth_node = eth_node.clone();
        async move {
            Ok::<_, Infallible>(service_fn({
                let eth_node = eth_node.clone();
                move |req| handle(eth_node.clone(), req)
            }))
        }
    });

    // Then bind and serve...
    let server = Server::bind(&addr).serve(make_service);
    let addr = server.local_addr();

    let (shutdown, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let graceful = server.with_graceful_shutdown(async {
            rx.await.ok();
        });
        // And run forever...
        if let Err(e) = graceful.await {
            eprintln!("server error: {e}");
        }
    });
    EthNodeHandle {
        shutdown,
        address: addr,
    }
}

pub(crate) struct EthNodeHandle {
    pub(crate) shutdown: Sender<()>,
    pub(crate) address: SocketAddr,
}

async fn handle(
    mock: Arc<MockMiddleware>,
    req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let body = hyper::body::to_bytes(req).await.unwrap();

    let v: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();
    let mut o = match v {
        serde_json::Value::Object(o) => o,
        _ => unreachable!(),
    };
    let id = o.get("id").unwrap().as_u64().unwrap();
    let method = o.get("method").unwrap().as_str().unwrap();
    let r = match method {
        "eth_blockNumber" => {
            let r = mock.get_block_number().await.unwrap();
            json!({ "id": id, "jsonrpc": "2.0", "result": r })
        }
        "eth_syncing" => {
            let r = mock.syncing().await.unwrap();
            match r {
                SyncingStatus::IsFalse => {
                    json!({ "id": id, "jsonrpc": "2.0", "result": false })
                }
                SyncingStatus::IsSyncing(status) => {
                    json!({ "id": id, "jsonrpc": "2.0", "result": {
                        "starting_block": status.starting_block, 
                        "current_block": status.current_block,
                        "highest_block": status.highest_block,
                    } })
                }
            }
        }
        "eth_getLogs" => {
            let params = o.remove("params").unwrap();
            let params: Vec<_> = serde_json::from_value(params).unwrap();
            let r = mock.get_logs(&params[0]).await.unwrap();
            json!({ "id": id, "jsonrpc": "2.0", "result": r })
        }
        _ => unreachable!(),
    };

    let r = serde_json::to_vec(&r).unwrap();

    Ok(Response::new(Body::from(r)))
}
