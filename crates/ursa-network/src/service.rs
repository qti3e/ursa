//! # Ursa libp2p implementation.
//!
//! The service is bootstrapped with the following premises:
//!
//! - Load or create a new [`Keypair`] by checking the local storage.
//! - Instantiate the [`UrsaTransport`] module with quic.or(tcp) and relay support.
//! - A custom ['NetworkBehaviour'] is implemented based on [`NetworkConfig`] provided by node runner.
//! - Using the [`UrsaTransport`] and [`Behaviour`] a new [`Swarm`] is built.
//! - Two channels are created to serve (send/receive) both the network [`NetworkCommand`]'s and [`UrsaEvent`]'s.
//!
//! The [`Swarm`] events are processed in the main event loop. This loop handles dispatching [`NetworkCommand`]'s and
//! receiving [`UrsaEvent`]'s using the respective channels.

use anyhow::{anyhow, Error, Result};
use bytes::Bytes;
use db::Store;
use fnv::FnvHashMap;
use futures_util::stream::StreamExt;
use fvm_ipld_blockstore::Blockstore;
use graphsync::{GraphSyncEvent, Request};
use ipld_traversal::{selector::RecursionLimit, Selector};
use libipld::{Cid, DefaultParams};
use libp2p::{
    autonat::{Event as AutonatEvent, NatStatus},
    gossipsub::{
        error::{PublishError, SubscriptionError},
        IdentTopic as Topic, MessageId, TopicHash,
    },
    identify::Event as IdentifyEvent,
    identity::Keypair,
    kad::{BootstrapOk, KademliaEvent, QueryResult},
    mdns::Event as MdnsEvent,
    multiaddr::Protocol,
    ping::Event as PingEvent,
    relay::v2::client::Client as RelayClient,
    request_response::{RequestId, RequestResponseEvent, RequestResponseMessage, ResponseChannel},
    swarm::{ConnectionHandler, IntoConnectionHandler, NetworkBehaviour},
    swarm::{ConnectionLimits, SwarmBuilder, SwarmEvent},
    Multiaddr, PeerId, Swarm,
};
use libp2p_bitswap::{BitswapEvent, QueryId};
use rand::prelude::SliceRandom;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
    time::Duration,
};
use tokio::{
    select,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender},
        oneshot,
    },
    time::{sleep, Instant},
};
use tracing::{debug, error, info, trace, warn};
use ursa_metrics::Recorder;
use ursa_store::{BitswapStorage, GraphSyncStorage, UrsaStore};

use crate::behaviour::KAD_PROTOCOL;
use crate::codec::protocol::{RequestType, ResponseType};
use crate::transport::build_transport;
use crate::utils::cache_summary::CacheSummary;
use crate::{
    behaviour::{Behaviour, BehaviourEvent},
    codec::protocol::{UrsaExchangeRequest, UrsaExchangeResponse},
    config::NetworkConfig,
};

pub const URSA_GLOBAL: &str = "/ursa/global";
pub const MESSAGE_PROTOCOL: &[u8] = b"/ursa/message/0.0.1";

type BlockOneShotSender<T> = oneshot::Sender<Result<T, Error>>;
type SwarmEventType<S> = SwarmEvent<
<Behaviour<DefaultParams, S> as NetworkBehaviour>::OutEvent,
<
    <
        <
            Behaviour<DefaultParams, S> as NetworkBehaviour>::ConnectionHandler as IntoConnectionHandler
        >::Handler as ConnectionHandler
    >::Error
>;

#[derive(Debug)]
pub enum GossipsubMessage {
    /// A subscribe message.
    Subscribe {
        peer_id: PeerId,
        topic: TopicHash,
        sender: oneshot::Sender<Result<bool, SubscriptionError>>,
    },
    /// A subscribe message.
    Unsubscribe {
        peer_id: PeerId,
        topic: TopicHash,
        sender: oneshot::Sender<Result<bool, PublishError>>,
    },
    /// Publish a message to a specific topic.
    Publish {
        topic: TopicHash,
        data: Bytes,
        sender: oneshot::Sender<Result<MessageId, PublishError>>,
    },
}

#[derive(Debug)]
pub enum GossipsubEvent {
    /// A message has been received.
    Message {
        /// The peer that forwarded us this message.
        peer_id: PeerId,
        /// The [`MessageId`] of the message. This should be referenced by the application when
        /// validating a message (if required).
        message_id: MessageId,
        /// The decompressed message itself.
        message: libp2p::gossipsub::GossipsubMessage,
    },
    /// A remote subscribed to a topic.
    Subscribed {
        /// Remote that has subscribed.
        peer_id: PeerId,
        /// The topic it has subscribed to.
        topic: TopicHash,
    },
    /// A remote unsubscribed from a topic.
    Unsubscribed {
        /// Remote that has unsubscribed.
        peer_id: PeerId,
        /// The topic it has subscribed from.
        topic: TopicHash,
    },
}

/// [network]'s events
/// Requests and failure events emitted by the `NetworkBehaviour`.
#[derive(Debug)]
pub enum NetworkEvent {
    /// An event trigger when remote peer connects.
    PeerConnected(PeerId),
    /// An event trigger when remote peer disconnects.
    PeerDisconnected(PeerId),
    /// A Gossip message request was received from a peer.
    Gossipsub(GossipsubEvent),
    /// A message request was received from a peer.
    RequestMessage { request_id: RequestId },
    /// A bitswap HAVE event generated by the service.
    BitswapHave { cid: Cid, query_id: QueryId },
    /// A bitswap WANT event generated by the service.
    BitswapWant { cid: Cid, query_id: QueryId },
}

#[derive(Debug)]
pub enum NetworkCommand {
    GetBitswap {
        cid: Cid,
        sender: BlockOneShotSender<()>,
    },

    Put {
        cid: Cid,
        sender: oneshot::Sender<Result<()>>,
    },

    GetPeers {
        sender: oneshot::Sender<HashSet<PeerId>>,
    },

    GetListenerAddresses {
        sender: oneshot::Sender<Vec<Multiaddr>>,
    },

    SendRequest {
        peer_id: PeerId,
        request: Box<UrsaExchangeRequest>,
        channel: oneshot::Sender<Result<UrsaExchangeResponse>>,
    },

    GossipsubMessage {
        peer_id: PeerId,
        message: GossipsubMessage,
    },

    #[cfg(test)]
    GetPeerContent {
        sender: oneshot::Sender<HashMap<PeerId, CacheSummary>>,
    },
}

pub struct UrsaService<S>
where
    S: Blockstore + Clone + Store + Send + Sync + 'static,
{
    /// Store.
    pub store: Arc<UrsaStore<S>>,
    /// The main libp2p swarm emitting events.
    swarm: Swarm<Behaviour<DefaultParams, S>>,
    /// Handles outbound messages to peers.
    command_sender: Sender<NetworkCommand>,
    /// Handles inbound messages from peers.
    command_receiver: Receiver<NetworkCommand>,
    /// Handles events emitted by the ursa network.
    event_sender: Sender<NetworkEvent>,
    /// Handles events received by the ursa network.
    _event_receiver: Receiver<NetworkEvent>,
    /// Bitswap pending queries.
    bitswap_queries: FnvHashMap<QueryId, Cid>,
    /// hashmap for keeping track of rpc response channels.
    response_channels: FnvHashMap<Cid, Vec<BlockOneShotSender<()>>>,
    /// Pending requests.
    _pending_requests: HashMap<RequestId, ResponseChannel<UrsaExchangeResponse>>,
    /// Pending responses.
    pending_responses: HashMap<RequestId, oneshot::Sender<Result<UrsaExchangeResponse>>>,
    /// Connected peers.
    peers: HashSet<PeerId>,
    /// Bootstrap multiaddrs.
    bootstraps: Vec<Multiaddr>,
    /// Summarizes the cached content.
    cached_content: CacheSummary,
    /// Content summaries from other nodes.
    peer_cached_content: HashMap<PeerId, CacheSummary>,
    /// Interval for random Kademlia walks.
    kad_walk_interval: u64,
}

impl<S> UrsaService<S>
where
    S: Blockstore + Clone + Store + Send + Sync + 'static,
{
    /// Init a new [`UrsaService`] based on [`NetworkConfig`]
    ///
    /// For ursa `keypair` we use ed25519 either
    /// checking for a local store or creating a new keypair.
    ///
    /// For ursa `transport` we build a default QUIC layer and
    /// fail over to tcp.
    ///
    /// For ursa behaviour we use [`Behaviour`].
    ///
    /// We construct a [`Swarm`] with [`UrsaTransport`] and [`Behaviour`]
    /// listening on [`NetworkConfig`] `swarm_addr`.
    ///
    pub fn new(keypair: Keypair, config: &NetworkConfig, store: Arc<UrsaStore<S>>) -> Result<Self> {
        let local_peer_id = PeerId::from(keypair.public());

        let (relay_transport, relay_client) = if config.relay_client {
            if !config.autonat {
                error!("Relay client requires autonat to know if we are behind a NAT");
            }

            let (relay_transport, relay_behavior) =
                RelayClient::new_transport_and_behaviour(keypair.public().into());
            (Some(relay_transport), Some(relay_behavior))
        } else {
            (None, None)
        };

        let bitswap_store = BitswapStorage(store.clone());
        let graphsync_store = GraphSyncStorage(store.clone());
        let transport = build_transport(&keypair, config, relay_transport);
        let mut peers = HashSet::new();
        let behaviour = Behaviour::new(
            &keypair,
            config,
            bitswap_store,
            graphsync_store,
            relay_client,
            &mut peers,
        );

        let limits = ConnectionLimits::default()
            .with_max_pending_incoming(Some(2 << 9))
            .with_max_pending_outgoing(Some(2 << 9))
            .with_max_established_incoming(Some(2 << 9))
            .with_max_established_outgoing(Some(2 << 9))
            .with_max_established_per_peer(Some(8));

        let mut swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id)
            .notify_handler_buffer_size(NonZeroUsize::new(2 << 7).unwrap())
            .connection_event_buffer_size(2 << 7)
            .dial_concurrency_factor(NonZeroU8::new(8).unwrap())
            .connection_limits(limits)
            .build();

        for to_dial in &config.bootstrap_nodes {
            swarm.dial(to_dial.clone())?;
        }

        for addr in &config.swarm_addrs {
            Swarm::listen_on(&mut swarm, addr.clone())
                .map_err(|err| anyhow!("{}", err))
                .unwrap();
        }

        // subscribe to topic
        let topic = Topic::new(URSA_GLOBAL);
        if let Err(error) = swarm.behaviour_mut().subscribe(&topic) {
            warn!("Failed to subscribe to topic: {}", error);
        }

        let (event_sender, _event_receiver) = unbounded_channel();
        let (command_sender, command_receiver) = unbounded_channel();

        Ok(UrsaService {
            swarm,
            store,
            command_sender,
            command_receiver,
            event_sender,
            _event_receiver,
            response_channels: Default::default(),
            bitswap_queries: Default::default(),
            _pending_requests: HashMap::default(),
            pending_responses: HashMap::default(),
            peers,
            bootstraps: config.bootstrap_nodes.clone(),
            cached_content: CacheSummary::default(),
            peer_cached_content: HashMap::default(),
            kad_walk_interval: config.kad_walk_interval,
        })
    }

    pub fn close_command_receiver(&mut self) {
        self.command_receiver.close();
    }

    pub fn command_sender(&self) -> Sender<NetworkCommand> {
        self.command_sender.clone()
    }

    fn emit_event(&mut self, event: NetworkEvent) {
        let sender = self.event_sender.clone();
        tokio::task::spawn(async move {
            if let Err(error) = sender.send(event) {
                warn!("[emit_event] - failed to emit network event: {:?}.", error);
            };
        });
    }

    fn handle_ping(&mut self, ping_event: PingEvent) -> Result<()> {
        match ping_event.result {
            Ok(libp2p::ping::Success::Ping { rtt }) => {
                trace!(
                    "[PingSuccess::Ping] - with rtt {} from {} in ms",
                    rtt.as_millis(),
                    ping_event.peer.to_base58(),
                );
            }
            Ok(libp2p::ping::Success::Pong) => {
                trace!(
                    "PingSuccess::Pong] - received a ping and sent back a pong to {}",
                    ping_event.peer.to_base58()
                );
            }
            Err(libp2p::ping::Failure::Other { error }) => {
                debug!(
                    "[PingFailure::Other] - the ping failed with {} for reasons {}",
                    ping_event.peer.to_base58(),
                    error
                );
            }
            Err(libp2p::ping::Failure::Timeout) => {
                warn!(
                    "[PingFailure::Timeout] - no response was received from {}",
                    ping_event.peer.to_base58()
                );
            }
            Err(libp2p::ping::Failure::Unsupported) => {
                debug!(
                    "[PingFailure::Unsupported] - the peer {} does not support the ping protocol",
                    ping_event.peer.to_base58()
                );
            }
        }
        Ok(())
    }

    fn handle_identify(&mut self, identify_event: IdentifyEvent) -> Result<(), Error> {
        match identify_event {
            IdentifyEvent::Received { peer_id, info } => {
                trace!(
                    "[IdentifyEvent::Received] - with version {} has been received from a peer {}.",
                    info.protocol_version,
                    peer_id
                );

                if self.peers.contains(&peer_id) {
                    trace!(
                        "[IdentifyEvent::Received] - peer {} already known!",
                        peer_id
                    );
                }

                // check if received identify is from a peer on the same network
                if info
                    .protocols
                    .iter()
                    .any(|name| name.as_bytes() == KAD_PROTOCOL)
                {
                    let behaviour = self.swarm.behaviour_mut();

                    behaviour.gossipsub.add_explicit_peer(&peer_id);

                    for address in info.listen_addrs {
                        behaviour.add_address(&peer_id, address);
                    }
                }
            }
            IdentifyEvent::Sent { .. }
            | IdentifyEvent::Pushed { .. }
            | IdentifyEvent::Error { .. } => {}
        }
        Ok(())
    }

    fn handle_autonat(&mut self, autonat_event: AutonatEvent) -> Result<(), Error> {
        match autonat_event {
            AutonatEvent::StatusChanged { old, new } => match (old, new) {
                (NatStatus::Unknown, NatStatus::Private) => {
                    if self.swarm.behaviour().relay_client.is_enabled() {
                        if let Some(addr) = self.bootstraps.choose(&mut rand::thread_rng()) {
                            let circuit_addr = addr.clone().with(Protocol::P2pCircuit);
                            warn!(
                                "Private NAT detected. Establishing public relay address on peer {}",
                                circuit_addr
                                    .clone()
                                    .with(
                                        Protocol::P2p(
                                            self.swarm.local_peer_id().to_owned().into()
                                        )
                                    )
                            );
                            self.swarm
                                .listen_on(circuit_addr)
                                .expect("failed to listen on relay");
                        }
                    }
                }
                (_, NatStatus::Public(addr)) => {
                    info!("Public Nat verified! Public listening address: {}", addr);
                }
                (old, new) => {
                    warn!("NAT status changed from {:?} to {:?}", old, new);
                }
            },
            AutonatEvent::InboundProbe(_) | AutonatEvent::OutboundProbe(_) => (),
        }
        Ok(())
    }

    fn handle_bitswap(&mut self, bitswap_event: BitswapEvent) -> Result<()> {
        match bitswap_event {
            BitswapEvent::Progress(query_id, _) => {
                trace!(
                    "[BitswapEvent::Progress] - bitswap request in progress with, id: {}",
                    query_id
                );
            }
            BitswapEvent::Complete(query_id, result) => {
                if let Some(cid) = self.bitswap_queries.remove(&query_id) {
                    if let Some(chans) = self.response_channels.remove(&cid) {
                        for chan in chans.into_iter() {
                            match result {
                                Ok(()) => {
                                    if chan.send(Ok(())).is_err() {
                                        error!("[BitswapEvent::Complete] - Bitswap response channel send failed");
                                    }
                                }
                                Err(_) => {
                                    if chan.send(Err(anyhow!("The requested block with cid {cid:?} is not found with any peers"))).is_err() {
                                        error!("[BitswapEvent::Complete] - Bitswap response channel send failed");
                                    }
                                }
                            }
                        }
                    } else {
                        debug!("[BitswapEvent::Complete] - Received Bitswap response, but response channel cannot be found");
                    }
                } else {
                    error!("[BitswapEvent::Complete] - Query Id {query_id:?} not found in the hash map");
                }
            }
        }
        Ok(())
    }

    fn handle_gossip(&mut self, gossip_event: libp2p::gossipsub::GossipsubEvent) -> Result<()> {
        match gossip_event {
            libp2p::gossipsub::GossipsubEvent::Message {
                propagation_source,
                message_id,
                message,
            } => {
                self.emit_event(NetworkEvent::Gossipsub(GossipsubEvent::Message {
                    peer_id: propagation_source,
                    message_id,
                    message,
                }));
            }
            libp2p::gossipsub::GossipsubEvent::Subscribed { peer_id, topic } => {
                self.emit_event(NetworkEvent::Gossipsub(GossipsubEvent::Subscribed {
                    peer_id,
                    topic,
                }));
            }
            libp2p::gossipsub::GossipsubEvent::Unsubscribed { peer_id, topic } => {
                self.emit_event(NetworkEvent::Gossipsub(GossipsubEvent::Unsubscribed {
                    peer_id,
                    topic,
                }));
            }
            libp2p::gossipsub::GossipsubEvent::GossipsubNotSupported { .. } => (),
        }
        Ok(())
    }

    pub fn handle_kad(&mut self, event: KademliaEvent) -> Result<()> {
        match event {
            KademliaEvent::OutboundQueryProgressed { id, result, .. } => match result {
                QueryResult::Bootstrap(result) => match result {
                    Ok(BootstrapOk {
                        peer,
                        num_remaining,
                    }) => {
                        debug!(
                            "[KademliaEvent::Bootstrap] - Received peer: {peer:?}, {}",
                            match num_remaining {
                                0 => "bootstrap complete!".into(),
                                n => format!("{n} peers remaining."),
                            }
                        );
                    }
                    Err(e) => {
                        warn!("[KademliaEvent::Bootstrap] - Bootstrap failed: {e:?}");
                    }
                },
                other => debug!("[KademliaEvent::OutboundQueryProgressed] - {id:?}: {other:?}"),
            },
            _ => debug!("[KademliaEvent] - {event:?}"),
        }
        Ok(())
    }

    pub fn handle_mdns(&mut self, event: MdnsEvent) -> Result<()> {
        match event {
            MdnsEvent::Discovered(discovered_peers) => {
                for (peer_id, address) in discovered_peers {
                    self.swarm
                        .behaviour_mut()
                        .add_address(&peer_id, address.clone());

                    if self.peers.insert(peer_id) {
                        match self.swarm.dial(address) {
                            Ok(_) => info!("Dialed new local peer: {peer_id:?}"),
                            Err(e) => error!("Failed to dial new local peer: {e:?}"),
                        }
                    }
                }
            }
            MdnsEvent::Expired(_) => {}
        }
        Ok(())
    }

    fn handle_req_res(
        &mut self,
        req_res_event: RequestResponseEvent<UrsaExchangeRequest, UrsaExchangeResponse>,
    ) -> Result<()> {
        match req_res_event {
            RequestResponseEvent::Message { peer, message } => match message {
                RequestResponseMessage::Request {
                    request_id,
                    request,
                    channel,
                } => {
                    match request.0 {
                        RequestType::CarRequest(_) => (),
                        RequestType::CacheRequest(cid) => {
                            info!("[BehaviourEvent::RequestMessage] cache request from {peer} for {cid}");

                            let selector = Selector::ExploreRecursive {
                                limit: RecursionLimit::None,
                                sequence: Box::new(Selector::ExploreAll {
                                    next: Box::new(Selector::ExploreRecursiveEdge),
                                }),
                                current: None,
                            };

                            let req = Request::builder()
                                .root(cid.to_bytes())
                                .selector(selector)
                                .build()
                                .unwrap();
                            let swarm = self.swarm.behaviour_mut();
                            swarm.graphsync.request(peer, req);
                            if swarm
                                .request_response
                                .send_response(
                                    channel,
                                    UrsaExchangeResponse(ResponseType::CacheResponse),
                                )
                                .is_err()
                            {
                                error!("[BehaviourEvent::RequestMessage] failed to send response")
                            }
                        }
                        RequestType::StoreSummary(cache_summary) => {
                            self.peer_cached_content.insert(peer, *cache_summary);
                            if self
                                .swarm
                                .behaviour_mut()
                                .request_response
                                .send_response(
                                    channel,
                                    UrsaExchangeResponse(ResponseType::StoreSummaryRequest),
                                )
                                .is_err()
                            {
                                error!(
                                        "[BehaviourEvent::RequestMessage] failed to send StoreSummaryRequest response"
                                    )
                            }
                        }
                    }
                    trace!("[BehaviourEvent::RequestMessage] {} ", peer);
                    self.emit_event(NetworkEvent::RequestMessage { request_id });
                }
                RequestResponseMessage::Response {
                    request_id,
                    response,
                } => {
                    trace!(
                        "[RequestResponseMessage::Response] - {} {}: {:?}",
                        request_id,
                        peer,
                        response
                    );

                    if let Some(request) = self.pending_responses.remove(&request_id) {
                        if request.send(Ok(response)).is_err() {
                            warn!("[RequestResponseMessage::Response] - failed to send request: {request_id:?}");
                        }
                    }

                    debug!("[RequestResponseMessage::Response] - failed to remove channel for: {request_id:?}");
                }
            },
            RequestResponseEvent::OutboundFailure { .. }
            | RequestResponseEvent::InboundFailure { .. }
            | RequestResponseEvent::ResponseSent { .. } => (),
        }
        Ok(())
    }

    fn handle_graphsync(&mut self, event: GraphSyncEvent) -> Result<()> {
        match event {
            GraphSyncEvent::Completed {
                id,
                peer_id,
                received,
            } => {
                info!("[GraphSyncEvent::Completed]: {id} {peer_id} {received}");
                Ok(())
            }
            event => {
                info!("[GraphSyncEvent]: {event:?}");
                Ok(())
            }
        }
    }

    /// Handle swarm events
    pub fn handle_swarm_event(&mut self, event: SwarmEventType<S>) -> Result<()> {
        // record basic swarm metrics

        event.record();
        match event {
            SwarmEvent::Behaviour(event) => match event {
                BehaviourEvent::Identify(identify_event) => {
                    identify_event.record();
                    self.handle_identify(identify_event)
                }
                BehaviourEvent::Autonat(autonat_event) => self.handle_autonat(autonat_event),
                BehaviourEvent::Ping(ping_event) => {
                    ping_event.record();
                    self.handle_ping(ping_event)
                }
                BehaviourEvent::Bitswap(bitswap_event) => {
                    // bitswap metrics are internal
                    self.handle_bitswap(bitswap_event)
                }
                BehaviourEvent::Gossipsub(gossip_event) => {
                    gossip_event.record();
                    self.handle_gossip(gossip_event)
                }
                BehaviourEvent::Mdns(mdns_event) => self.handle_mdns(mdns_event),
                BehaviourEvent::Kad(kad_event) => {
                    kad_event.record();
                    self.handle_kad(kad_event)
                }
                BehaviourEvent::RequestResponse(req_res_event) => {
                    req_res_event.record();
                    self.handle_req_res(req_res_event)
                }
                BehaviourEvent::RelayServer(relay_event) => {
                    relay_event.record();
                    Ok(())
                }
                BehaviourEvent::RelayClient(_) => Ok(()),
                BehaviourEvent::Dcutr(_) => Ok(()),
                BehaviourEvent::Graphsync(event) => self.handle_graphsync(event),
            },
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if self.peers.insert(peer_id) {
                    debug!("Peer connected: {peer_id}");
                    self.emit_event(NetworkEvent::PeerConnected(peer_id));
                };
                Ok(())
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => {
                if num_established == 0 && self.peers.remove(&peer_id) {
                    self.peer_cached_content.remove(&peer_id);
                    debug!("Peer disconnected: {peer_id}");
                    self.emit_event(NetworkEvent::PeerDisconnected(peer_id));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Handle commands
    pub fn handle_command(&mut self, command: NetworkCommand) -> Result<()> {
        match command {
            NetworkCommand::GetBitswap { cid, sender } => {
                info!("Getting cid {cid} via bitswap");

                let peers = self.peers.clone();

                if peers.is_empty() {
                    error!(
                        "There were no peers provided and the block does not exist in local store"
                    );
                    return sender
                        .send(Err(anyhow!(
                        "There were no peers provided and the block does not exist in local store"
                    )))
                        .map_err(|_| anyhow!("Failed to get a bitswap block!"));
                } else {
                    if let Some(chans) = self.response_channels.get_mut(&cid) {
                        chans.push(sender);
                    } else {
                        self.response_channels.insert(cid, vec![sender]);
                    }

                    let peers = peers
                        .iter()
                        .filter(|peer| {
                            if let Some(cache_summary) = self.peer_cached_content.get(*peer) {
                                return cache_summary.contains(cid.to_bytes());
                            }
                            true
                        })
                        .copied()
                        .collect();

                    let query = self.swarm.behaviour_mut().sync_block(cid, peers);

                    if let Ok(query_id) = query {
                        self.bitswap_queries.insert(query_id, cid);
                        self.emit_event(NetworkEvent::BitswapWant { cid, query_id });
                    } else {
                        error!(
                            "[NetworkCommand::BitswapWant] - no block found for cid {:?}.",
                            cid
                        )
                    }
                }
            }
            NetworkCommand::Put { cid, sender } => {
                // replicate content
                let swarm = self.swarm.behaviour_mut();
                for peer in &self.peers {
                    info!("[NetworkCommand::Put] - sending cache request to peer {peer} for {cid}");
                    swarm
                        .request_response
                        .send_request(peer, UrsaExchangeRequest(RequestType::CacheRequest(cid)));
                }
                // update cache summary and share it with the connected peers
                self.cached_content.insert(&cid.to_bytes());
                let swarm = self.swarm.behaviour_mut();
                for peer in &self.peers {
                    let request = UrsaExchangeRequest(RequestType::StoreSummary(Box::new(
                        self.cached_content.clone(),
                    )));
                    swarm.request_response.send_request(peer, request);
                }

                sender
                    .send(Ok(()))
                    .map_err(|e| anyhow!("PUT failed: {e:?}."))?;
            }
            NetworkCommand::GetPeers { sender } => {
                sender
                    .send(self.peers.clone())
                    .map_err(|_| anyhow!("Failed to get Libp2p peers!"))?;
            }
            NetworkCommand::GetListenerAddresses { sender } => {
                let mut addresses: Vec<&Multiaddr> = self.swarm.listeners().collect();
                if let Some(value) = self.swarm.behaviour().public_address() {
                    addresses.push(value);
                }
                sender
                    .send(addresses.into_iter().cloned().collect())
                    .map_err(|_| anyhow!("Failed to get listener addresses from network"))?;
            }
            NetworkCommand::SendRequest {
                peer_id,
                request,
                channel,
            } => {
                let request_id = self
                    .swarm
                    .behaviour_mut()
                    .request_response
                    .send_request(&peer_id, *request);
                self.pending_responses.insert(request_id, channel);

                self.emit_event(NetworkEvent::RequestMessage { request_id });
            }
            NetworkCommand::GossipsubMessage {
                peer_id: _,
                message,
            } => match message {
                GossipsubMessage::Subscribe {
                    peer_id: _,
                    topic,
                    sender,
                } => {
                    let subscribe = self
                        .swarm
                        .behaviour_mut()
                        .gossipsub
                        .subscribe(&Topic::new(topic.into_string()));

                    sender
                        .send(subscribe)
                        .map_err(|_| anyhow!("Failed to subscribe!"))?;
                }
                GossipsubMessage::Unsubscribe {
                    peer_id: _,
                    topic,
                    sender,
                } => {
                    let unsubscribe = self
                        .swarm
                        .behaviour_mut()
                        .gossipsub
                        .unsubscribe(&Topic::new(topic.into_string()));

                    sender
                        .send(unsubscribe)
                        .map_err(|_| anyhow!("Failed to unsubscribe!"))?;
                }
                GossipsubMessage::Publish {
                    topic,
                    data,
                    sender,
                } => {
                    let publish = self
                        .swarm
                        .behaviour_mut()
                        .publish(Topic::new(topic.into_string()), data.to_vec());

                    if let Err(e) = &publish {
                        warn!("Publish error: {e:?}");
                    }

                    sender
                        .send(publish)
                        .map_err(|_| anyhow!("Failed to publish message!"))?;
                }
            },
            #[cfg(test)]
            NetworkCommand::GetPeerContent { sender } => {
                sender
                    .send(self.peer_cached_content.clone())
                    .map_err(|_| anyhow!("Failed to send peer content."))?;
            }
        }
        Ok(())
    }

    /// Dial remote peer `peer_id` at `address`
    pub fn dial(
        &mut self,
        peer_id: PeerId,
        address: Multiaddr,
        response: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        trace!("dial peer ({peer_id}) at address {address}");

        match self.swarm.dial(address.clone()) {
            Ok(_) => {
                self.swarm
                    .behaviour_mut()
                    .kad
                    .add_address(&peer_id, address);
                response
                    .send(Ok(()))
                    .map_err(|_| anyhow!("{}", "Channel Dropped"))
            }
            Err(err) => response
                .send(Err(err.into()))
                .map_err(|_| anyhow!("{}", "DialError")),
        }
    }

    /// Start the ursa network service loop.
    ///
    /// Poll `swarm` and `command_receiver` from [`UrsaService`].
    /// - `swarm` handles the network events [Event].
    /// - `command_receiver` handles inbound commands [Command].
    pub async fn start(mut self) -> Result<()> {
        info!(
            "Node starting up with peerId {:?}",
            self.swarm.local_peer_id()
        );

        let kad_walk_delay = sleep(Duration::from_secs(self.kad_walk_interval));
        tokio::pin!(kad_walk_delay);

        loop {
            select! {
                event = self.swarm.next() => {
                    let event = event.ok_or_else(|| anyhow!("Swarm Event invalid!"))?;
                    self.handle_swarm_event(event).expect("Handle swarm event.");
                },
                command = self.command_receiver.recv() => {
                    let command = command.ok_or_else(|| anyhow!("Command invalid!"))?;
                    self.handle_command(command).expect("Handle rpc command.");
                },
                _ = &mut kad_walk_delay => {
                    info!("Starting random kademlia walk");
                    self.swarm.behaviour_mut().kad.get_closest_peers(PeerId::random());
                    kad_walk_delay.as_mut().reset(Instant::now() + Duration::from_secs(self.kad_walk_interval));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/service_tests.rs"]
mod service_tests;
