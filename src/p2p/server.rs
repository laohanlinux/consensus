use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net;
use std::str::FromStr;
use std::time::{Duration, Instant};

use ::actix::prelude::*;
use actix_broker::BrokerSubscribe;
use cryptocurrency_kit::storage::values::StorageValue;
use cryptocurrency_kit::crypto::{CryptoHash, Hash};
use futures::prelude::*;
use libp2p::{
    core::nodes::swarm::NetworkBehaviour,
    core::upgrade::{self, OutboundUpgradeExt},
    floodsub::FloodsubMessage,
    mplex,
    multiaddr::Protocol,
    secio, Multiaddr, PeerId, Transport,
};
use tokio::{timer::Delay, codec::FramedRead, io::AsyncRead, io::WriteHalf, net::TcpListener, net::TcpStream};
use uuid::Uuid;
use lru_time_cache::LruCache;
use chrono::Local;

use super::codec::MsgPacketCodec;
use super::protocol::{BoundType, RawMessage, Header as RawHeader, P2PMsgCode, Payload, Handshake};
use super::session::Session;
use crate::{
    types::block::Blocks,
    common::{multiaddr_to_ipv4, random_uuid},
    error::P2PError,
    subscriber::P2PEvent,
    subscriber::events::{BroadcastEvent, ChainEvent},
};

pub const MAX_OUTBOUND_CONNECTION_MAILBOX: usize = 1 << 10;
pub const MAX_INBOUND_CONNECTION_MAILBOX: usize = 1 << 9;

lazy_static! {
    pub static ref ZERO_PEER: PeerId =
        { PeerId::from_str("QmX5e9hkQf7B45e2MZf38vhsC2wfA5aKQrrBuLujwaUBGw").unwrap() };
}

pub type AuthorFn = Fn(Handshake) -> bool;
pub type HandleMsgFn = Fn(PeerId, RawMessage) -> Result<(), String>;

pub type HandshakePacketFn = Fn() -> Handshake;

pub fn author_handshake(genesis: Hash) -> impl Fn(Handshake) -> bool {
    move |handshake: Handshake| {
        if *handshake.genesis() != genesis {
            return false;
        }
        true
    }
}

pub enum ServerEvent {
    Connected(PeerId, BoundType, Addr<Session>, RawMessage),
    Disconnected(PeerId),
    Message(PeerId, RawMessage),
    Ping(PeerId),
}

impl Message for ServerEvent {
    type Result = Result<PeerId, P2PError>;
}

pub enum SessionEvent {
    Stop,
}

impl Message for SessionEvent {
    type Result = ();
}

pub struct TcpServer {
    pid: Addr<TcpServer>,
    key: Option<secio::SecioKeyPair>,
    node_info: (PeerId, Multiaddr),
    peers: HashMap<PeerId, ConnectInfo>,
    genesis: Hash,
    cache: LruCache<Hash, bool>,
    author_fn: Box<AuthorFn>,
    handles: Box<HandleMsgFn>,
}

struct ConnectInfo {
    connect_time: chrono::DateTime<chrono::Utc>,
    bound_type: BoundType,
    pid: Addr<Session>,
}

impl ConnectInfo {
    fn new(connect_time: chrono::DateTime<chrono::Utc>, bound_type: BoundType, pid: Addr<Session>) -> Self {
        ConnectInfo {
            connect_time: connect_time,
            bound_type: bound_type,
            pid: pid,
        }
    }
}

fn node_info(peers: &HashMap<PeerId, ConnectInfo>) -> String {
    let mut info: Vec<String> = vec![];
    for peer in peers {
        info.push(format!(
            "{}----> [bound: {:?}, connect_time: {:?}]",
            peer.0.to_base58(),
            peer.1.bound_type,
            peer.1.connect_time
        ));
    }
    info.join("\n")
}

impl Actor for TcpServer {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!(
            "[{:?}] Server start, listen on: {:?}",
            self.node_info.0, self.node_info.1
        );
        self.subscribe_async::<BroadcastEvent>(ctx);
        ctx.run_interval(::std::time::Duration::from_secs(2), |act, _| {
            debug!(
                "Connect clients: {}\nlocal-id:{}, \n{}",
                act.peers.len(),
                act.node_info.0.to_base58(),
                node_info(&act.peers)
            );
        });

        ctx.run_interval(Duration::from_secs(3), |act, _| {
            let mut peers = vec![];
            act.peers.iter().for_each(|kv| {
                let sub = chrono::Utc::now().timestamp() - kv.1.connect_time.timestamp();
                if sub > 3 {
                    peers.push(kv.0.clone());
                }
            });

            for peer in peers {
                debug!("Remove peer {}", peer.to_base58());
                if let Some(connect_info) = act.peers.remove(&peer) {
                    connect_info.pid.do_send(SessionEvent::Stop);
                }
            }
        });
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        info!(
            "[{:?}] Server stopped, listen on: {:?}",
            self.node_info.0.to_base58(),
            self.node_info.1
        );
    }
}

impl Handler<P2PEvent> for TcpServer {
    type Result = ();

    /// handle p2p event
    fn handle(&mut self, msg: P2PEvent, _: &mut Self::Context) -> Self::Result {
        match msg {
            P2PEvent::AddPeer(remote_peer, remote_addresses) => {
                self.add_peer(remote_peer, remote_addresses);
            }
            P2PEvent::DropPeer(remote_peer, remote_addresses) => {
                self.drop_peer(remote_peer, remote_addresses);
            }
        }
        ()
    }
}

impl Handler<BroadcastEvent> for TcpServer {
    type Result = ();

    /// handle p2p event
    fn handle(&mut self, msg: BroadcastEvent, _ctx: &mut Self::Context) -> Self::Result {
        debug!("TcpServer[e:BroadcastEvent]");
        match msg {
            BroadcastEvent::Consensus(msg) => {
                let header = RawHeader::new(P2PMsgCode::Consensus, 10, chrono::Local::now().timestamp_millis() as u64, None);
                let payload = msg.into_payload();
                let msg = RawMessage::new(header, payload);
                self.broadcast(&msg);
            }
            BroadcastEvent::Blocks(peer_id, blocks) => {
                let mut header = RawHeader::new(P2PMsgCode::Block, 10, chrono::Local::now().timestamp_millis() as u64, None);
                if let Some(peer_id) = peer_id {
                    header.peer_id = Some(peer_id.as_bytes().to_vec());
                }
                let payload = blocks.into_bytes();
                let msg = RawMessage::new(header, payload);
                self.broadcast(&msg);
            }
            BroadcastEvent::Sync(height) => {
                self.peers.keys().take(1).for_each(|peer_id| {
                    let header = RawHeader::new(P2PMsgCode::Sync, 10, chrono::Local::now().timestamp_millis() as u64, Some(peer_id.as_bytes().to_vec()));
                    let payload = height.into_bytes();
                    let msg = RawMessage::new(header, payload);
                    self.broadcast(&msg);
                });
            }
            _ => unimplemented!()
        }
        ()
    }
}

//FIXME
impl Handler<ChainEvent> for TcpServer {
    type Result = ();

    /// handle p2p event
    fn handle(&mut self, msg: ChainEvent, ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ChainEvent::NewBlock(block) => {
                ctx.notify(BroadcastEvent::Blocks(None, Blocks(vec![block])));
            }
            ChainEvent::NewHeader(_) => {}
            ChainEvent::SyncBlock(height) => {
                ctx.notify(BroadcastEvent::Sync(height))
            }
            ChainEvent::PostBlock(peer_id, blocks) => {
                ctx.notify(BroadcastEvent::Blocks(peer_id, blocks))
            }
        }
        ()
    }
}

impl Handler<ServerEvent> for TcpServer {
    type Result = Result<PeerId, P2PError>;
    fn handle(&mut self, msg: ServerEvent, _ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ServerEvent::Connected(ref peer_id, ref bound_type, ref pid, ref raw_msg) => {
                debug!("Connected peer: {:?}", peer_id);
                return self.handle_handshake(bound_type.clone(), pid.clone(), raw_msg.payload());
            }
            ServerEvent::Disconnected(ref peer_id) => {
                debug!("Disconnected peer: {:?}", peer_id);
                self.peers.remove(&peer_id);
                return Ok(peer_id.clone());
            }
            ServerEvent::Ping(ref peer_id) => {
                let mut info = self.peers.get_mut(peer_id).unwrap();
                info.connect_time = chrono::Utc::now();
                return Ok(peer_id.clone());
            }

            // 接收端
            ServerEvent::Message(ref peer_id, ref raw_msg) => {
                let hash: Hash = raw_msg.hash();
                let now = Local::now().timestamp_millis() as u64;
                if now < raw_msg.header().create_time {
                    trace!("Skip message({:?}) cause of timeout", hash.short());
                    return Ok(peer_id.clone());
                }
                if self.cache.get(&hash).is_some() {
                    trace!("Skip message({:?}) cause of received", hash.short());
                    return Ok(peer_id.clone());
                } else {
                    (self.handles)(peer_id.clone(), raw_msg.clone());
                    return Ok(peer_id.clone());
                }
            }
        }
        Err(P2PError::InvalidMessage)
    }
}

impl TcpServer {
    pub fn new(
        peer_id: PeerId,
        mul_addr: Multiaddr,
        key: Option<secio::SecioKeyPair>,
        genesis: Hash,
        author: Box<Fn(Handshake) -> bool>,
        handles: Box<Fn(PeerId, RawMessage) -> Result<(), String>>,
    ) -> Addr<TcpServer> {
        let mut addr: String = String::new();
        mul_addr.iter().for_each(|item| match &item {
            Protocol::Ip4(ref ip4) => {
                addr.push_str(&format!("{}:", ip4));
            }
            Protocol::Tcp(ref port) => {
                addr.push_str(&format!("{}", port));
            }
            _ => {}
        });
        let socket_addr = net::SocketAddr::from_str(&addr).unwrap();

        // bind tcp listen address
        let lis = TcpListener::bind(&socket_addr).unwrap();
        // create tcp server and dispatch coming connection to self handle
        TcpServer::create(move |ctx| {
            ctx.set_mailbox_capacity(MAX_INBOUND_CONNECTION_MAILBOX);
            ctx.add_message_stream(lis.incoming().map_err(|_| ()).map(move |s| {
                trace!("New connection are comming");
                TcpConnectInBound(s)
            }));
            TcpServer {
                pid: ctx.address().clone(),
                key: key,
                node_info: (peer_id.clone(), mul_addr.clone()),
                peers: HashMap::new(),
                cache: LruCache::with_expiry_duration_and_capacity(Duration::from_secs(5), 100_000),
                genesis: genesis,
                author_fn: author,
                handles: handles,
            }
        })
    }

    fn add_peer(&mut self, remote_id: PeerId, remote_addresses: Vec<Multiaddr>) {
        if self.peers.contains_key(&remote_id) {
            return;
        }

        let mul_addr = remote_addresses[0].clone();
        let local_id = self.node_info.0.clone();
        let server_id = self.pid.clone();
        let genesis = self.genesis.clone();
        let delay = rand::random::<u64>() % 100;
        let timer_fut = Delay::new(Instant::now() + Duration::from_millis(delay));
        tokio::spawn(timer_fut.and_then(move |_| {
            // try to connect, dial it
            TcpDial::new(
                remote_id,
                local_id,
                mul_addr,
                genesis,
                server_id,
            );
            futures::future::ok(())
        }).map_err(|err| panic!(err)));
    }

    // TODO
    fn drop_peer(&mut self, _remote_id: PeerId, _remote_addresses: Vec<Multiaddr>) {}

    fn handle_handshake(
        &mut self,
        bound_type: BoundType,
        pid: Addr<Session>,
        payload: &Vec<u8>,
    ) -> Result<PeerId, P2PError> {
        use std::borrow::Cow;
        let handshake: Handshake = Handshake::from_bytes(Cow::from(payload));
        let peer_id = handshake.peer_id();
        if self.peers.contains_key(&peer_id) {
            return Err(P2PError::DumpConnected);
        }
        if self.node_info.0 == handshake.peer_id() {
            return Err(P2PError::HandShakeFailed);
        }

        if !(self.author_fn)(handshake.clone()) {
            return Err(P2PError::DifferentGenesis);
        }

        match bound_type {
            BoundType::InBound => {}
            BoundType::OutBound => {}
        }
        let connect_info = ConnectInfo::new(chrono::Utc::now(), BoundType::InBound, pid);
        self.peers.entry(peer_id.clone()).or_insert(connect_info);
        Ok(peer_id)
    }

    fn broadcast(&self, msg: &RawMessage) {
        if let Some(ref peer) = msg.header().peer_id {
            let peer = PeerId::from_bytes(peer.clone()).unwrap();
            debug!("Broadcast message, code: {:?}, peer: {:?}", msg.header(), peer.to_base58());
            if let Some(info) = self.peers.get(&peer) {
                info.pid.do_send(msg.clone());
            }
        } else {
            for (peer, info) in &self.peers {
                debug!("Broadcast message, code: {:?}, peer: {:?}", msg.header(), peer.to_base58());
                info.pid.do_send(msg.clone());
            }
        }
    }
}

#[derive(Message)]
struct TcpConnectOutBound(TcpStream, PeerId);

/// Handle stream of TcpStream's
impl Handler<TcpConnectOutBound> for TcpServer {
    type Result = ();

    fn handle(&mut self, msg: TcpConnectOutBound, _ctx: &mut Context<Self>) {
        trace!("TcpServer receive tcp connect event, peerid: {:?}", msg.1);
        // For each incoming connection we create `session` actor with out chat server
        if self.peers.contains_key(&msg.1) {
            msg.0.shutdown(net::Shutdown::Both).unwrap();
            return;
        }

        let peer_id = msg.1.clone();
        let server_id = self.pid.clone();
        let local_id = self.node_info.0.clone();
        let genesis = self.genesis.clone();
        Session::create(move |ctx| {
            let (r, w) = msg.0.split();
            Session::add_stream(FramedRead::new(r, MsgPacketCodec), ctx);
            Session::new(
                ctx.address().clone(),
                peer_id,
                local_id,
                server_id,
                actix::io::FramedWrite::new(w, MsgPacketCodec, ctx),
                BoundType::OutBound,
                genesis,
            )
        });
    }
}

#[derive(Message)]
struct TcpConnectInBound(TcpStream);

impl Handler<TcpConnectInBound> for TcpServer {
    type Result = ();

    fn handle(&mut self, msg: TcpConnectInBound, _: &mut Context<Self>) {
        let server_id = self.pid.clone();
        let local_id = self.node_info.0.clone();
        let genesis = self.genesis.clone();
        Session::create(move |ctx| {
            let (r, w) = msg.0.split();
            Session::add_stream(FramedRead::new(r, MsgPacketCodec), ctx);
            Session::new(
                ctx.address().clone(),
                ZERO_PEER.clone(),
                local_id,
                server_id,
                actix::io::FramedWrite::new(w, MsgPacketCodec, ctx),
                BoundType::InBound,
                genesis,
            )
        });
    }
}

pub struct TcpDial {
    server: Addr<TcpServer>,
}

impl Actor for TcpDial {
    type Context = Context<Self>;
}

impl TcpDial {
    pub fn new(
        peer_id: PeerId,
        local_id: PeerId,
        mul_addr: Multiaddr,
        genesis: Hash,
        tcp_server: Addr<TcpServer>,
    ) {
        let socket_addr = multiaddr_to_ipv4(&mul_addr).unwrap();
        trace!(
            "Try to dial remote peer, peer_id:{:?}, network: {:?}",
            &peer_id,
            &socket_addr
        );
        Arbiter::spawn(
            TcpStream::connect(&socket_addr)
                .and_then(move |stream| {
                    trace!("Dialing remote peer: {:?}", peer_id);
                    let peer_id = peer_id.clone();
                    let local_id = local_id.clone();
                    let genesis = genesis.clone();
                    let tcp_server = tcp_server.clone();
                    Session::create(move |ctx| {
                        let (r, w) = stream.split();
                        Session::add_stream(FramedRead::new(r, MsgPacketCodec), ctx);
                        Session::new(
                            ctx.address().clone(),
                            peer_id,
                            local_id,
                            tcp_server,
                            actix::io::FramedWrite::new(w, MsgPacketCodec, ctx),
                            BoundType::OutBound,
                            genesis,
                        )
                    });

                    futures::future::ok(())
                })
                .map_err(|e| {
                    error!("Dial tcp connect fail, err: {}", e);
                    ()
                }),
        );
    }
}
