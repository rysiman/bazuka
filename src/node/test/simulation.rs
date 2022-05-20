use super::*;

use super::api::messages::*;
use crate::blockchain::{KvStoreChain, ZkBlockchainPatch};
use crate::core::Block;
use crate::db::RamKvStore;
use crate::wallet::Wallet;

use std::sync::Arc;
use tokio::sync::RwLock;

struct Node {
    addr: PeerAddress,
    incoming: SenderWrapper,
    outgoing: mpsc::UnboundedReceiver<OutgoingRequest>,
}

pub struct NodeOpts {
    pub genesis: (Block, ZkBlockchainPatch),
    pub wallet: Option<Wallet>,
    pub addr: u16,
    pub bootstrap: Vec<u16>,
    pub timestamp_offset: i32,
}

fn create_test_node(
    opts: NodeOpts,
) -> (impl futures::Future<Output = Result<(), NodeError>>, Node) {
    let addr = PeerAddress(SocketAddr::from(([127, 0, 0, 1], opts.addr)));
    let chain = KvStoreChain::new(RamKvStore::new(), opts.genesis).unwrap();
    let (inc_send, inc_recv) = mpsc::unbounded_channel::<IncomingRequest>();
    let (out_send, out_recv) = mpsc::unbounded_channel::<OutgoingRequest>();
    let node = node_create(
        addr,
        opts.bootstrap
            .iter()
            .map(|p| PeerAddress(SocketAddr::from(([127, 0, 0, 1], *p))))
            .collect(),
        chain,
        opts.timestamp_offset,
        opts.wallet,
        inc_recv,
        out_send,
    );
    (
        node,
        Node {
            addr,
            incoming: SenderWrapper {
                peer: addr,
                chan: Arc::new(inc_send),
            },
            outgoing: out_recv,
        },
    )
}

async fn route(
    enabled: Arc<RwLock<bool>>,
    mut outgoing: mpsc::UnboundedReceiver<OutgoingRequest>,
    incs: HashMap<PeerAddress, SenderWrapper>,
) -> Result<(), NodeError> {
    while let Some(req) = outgoing.recv().await {
        if !*enabled.read().await {
            continue;
        }
        let s = PeerAddress(
            req.body
                .uri()
                .authority()
                .unwrap()
                .to_string()
                .parse()
                .unwrap(),
        );
        let (resp_snd, mut resp_rcv) = mpsc::channel::<Result<Response<Body>, NodeError>>(1);
        let inc_req = IncomingRequest {
            socket_addr: s.0,
            body: req.body,
            resp: resp_snd,
        };
        incs[&s]
            .chan
            .send(inc_req)
            .map_err(|_| NodeError::NotListeningError)?;
        req.resp
            .send(resp_rcv.recv().await.ok_or(NodeError::NotAnsweringError)?)
            .await
            .map_err(|_| NodeError::NotListeningError)?;
    }

    Ok(())
}

#[derive(Clone)]
pub struct SenderWrapper {
    peer: PeerAddress,
    chan: Arc<mpsc::UnboundedSender<IncomingRequest>>,
}

impl SenderWrapper {
    pub async fn raw(&self, body: Request<Body>) -> Result<Body, NodeError> {
        let (resp_snd, mut resp_rcv) = mpsc::channel::<Result<Response<Body>, NodeError>>(1);
        let req = IncomingRequest {
            socket_addr: "0.0.0.0:0".parse().unwrap(),
            body,
            resp: resp_snd,
        };
        self.chan
            .send(req)
            .map_err(|_| NodeError::NotListeningError)?;

        let body = resp_rcv
            .recv()
            .await
            .ok_or(NodeError::NotAnsweringError)??
            .into_body();

        Ok(body)
    }
    #[allow(dead_code)]
    pub async fn json_get<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        req: Req,
    ) -> Result<Resp, NodeError> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "{}/{}?{}",
                self.peer,
                url,
                serde_qs::to_string(&req)?
            ))
            .body(Body::empty())?;
        let body = self.raw(req).await?;
        let resp: Resp = serde_json::from_slice(&hyper::body::to_bytes(body).await?)?;
        Ok(resp)
    }
    pub async fn json_post<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        req: Req,
    ) -> Result<Resp, NodeError> {
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("{}/{}", self.peer, url))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&req)?))?;
        let body = self.raw(req).await?;
        let resp: Resp = serde_json::from_slice(&hyper::body::to_bytes(body).await?)?;
        Ok(resp)
    }
    pub async fn shutdown(&self) -> Result<(), NodeError> {
        self.json_post::<ShutdownRequest, ShutdownResponse>("shutdown", ShutdownRequest {})
            .await?;
        Ok(())
    }
    pub async fn stats(&self) -> Result<GetStatsResponse, NodeError> {
        self.json_get::<GetStatsRequest, GetStatsResponse>("stats", GetStatsRequest {})
            .await
    }
    pub async fn peers(&self) -> Result<GetPeersResponse, NodeError> {
        self.json_get::<GetPeersRequest, GetPeersResponse>("peers", GetPeersRequest {})
            .await
    }

    pub async fn set_miner(
        &self,
        webhook: Option<String>,
    ) -> Result<RegisterMinerResponse, NodeError> {
        self.json_post::<RegisterMinerRequest, RegisterMinerResponse>(
            "miner",
            RegisterMinerRequest { webhook },
        )
        .await
    }

    pub async fn mine(&self) -> Result<PostMinerSolutionResponse, NodeError> {
        let puzzle = self
            .json_get::<GetMinerPuzzleRequest, Puzzle>("miner/puzzle", GetMinerPuzzleRequest {})
            .await?;
        let sol = mine_puzzle(&puzzle);
        self.json_post::<PostMinerSolutionRequest, PostMinerSolutionResponse>("miner/solution", sol)
            .await
    }
}

fn mine_puzzle(puzzle: &Puzzle) -> PostMinerSolutionRequest {
    let key = hex::decode(&puzzle.key).unwrap();
    let mut blob = hex::decode(&puzzle.blob).unwrap();
    let mut nonce = 0u64;
    loop {
        blob[puzzle.offset..puzzle.offset + puzzle.size].copy_from_slice(&nonce.to_le_bytes());
        let hash = crate::consensus::pow::hash(&key, &blob);
        if hash.meets_difficulty(rust_randomx::Difficulty::new(puzzle.target)) {
            return PostMinerSolutionRequest {
                nonce: hex::encode(nonce.to_le_bytes()),
            };
        }

        nonce += 1;
    }
}

pub fn test_network(
    enabled: Arc<RwLock<bool>>,
    node_opts: Vec<NodeOpts>,
) -> (
    impl futures::Future,
    impl futures::Future,
    Vec<SenderWrapper>,
) {
    let (node_futs, nodes): (Vec<_>, Vec<Node>) = node_opts
        .into_iter()
        .map(|node_opts| create_test_node(node_opts))
        .unzip();
    let incs: HashMap<_, _> = nodes.iter().map(|n| (n.addr, n.incoming.clone())).collect();
    let route_futs = nodes
        .into_iter()
        .map(|n| route(Arc::clone(&enabled), n.outgoing, incs.clone()))
        .collect::<Vec<_>>();

    (
        futures::future::join_all(node_futs),
        futures::future::join_all(route_futs),
        incs.into_values().collect(),
    )
}