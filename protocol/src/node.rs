//! The module for the node setup, running the main loop, and handling network
//! events.

use crate::{
	behaviour::{
		req_res::{Request, Response},
		EigenEvent, EigenTrustBehaviour,
	},
	constants::{EPOCH_INTERVAL, ITER_INTERVAL, NUM_ITERATIONS},
	epoch::Epoch,
	peer::Peer,
	utils::create_iter,
	EigenError,
};
use futures::{stream, StreamExt};
use libp2p::{
	core::{either::EitherError, upgrade::Version},
	identify::IdentifyEvent,
	identity::Keypair,
	noise::{Keypair as NoiseKeypair, NoiseConfig, X25519Spec},
	request_response::{RequestResponseEvent, RequestResponseMessage},
	swarm::{ConnectionHandlerUpgrErr, Swarm, SwarmBuilder, SwarmEvent},
	tcp::TcpConfig,
	yamux::YamuxConfig,
	Multiaddr, PeerId, Transport,
};
use std::io::Error as IoError;
use tokio::{
	select,
	time::{Duration, Instant},
};

/// The Node struct.
pub struct Node {
	/// Swarm object.
	pub(crate) swarm: Swarm<EigenTrustBehaviour>,
	pub(crate) peer: Peer,
}

impl Node {
	/// Create a new node, given the local keypair, local address, and bootstrap
	/// nodes.
	pub fn new(
		local_key: Keypair, local_address: Multiaddr, peer: Peer,
	) -> Result<Self, EigenError> {
		let noise_keys =
			NoiseKeypair::<X25519Spec>::new().into_authentic(&local_key).map_err(|e| {
				log::error!("NoiseKeypair.into_authentic {}", e);
				EigenError::InvalidKeypair
			})?;
		// 30 years in seconds
		// Basically, we want connections to be open for a long time.
		let connection_duration = Duration::from_secs(86400 * 365 * 30);
		let iter_interval_duration = Duration::from_secs(ITER_INTERVAL);
		let transport = TcpConfig::new()
			.nodelay(true)
			.upgrade(Version::V1)
			.authenticate(NoiseConfig::xx(noise_keys).into_authenticated())
			.multiplex(YamuxConfig::default())
			.timeout(connection_duration)
			.boxed();

		let beh = EigenTrustBehaviour::new(
			connection_duration,
			iter_interval_duration,
			local_key.public(),
		);
		// Setting up the transport and swarm.
		let local_peer_id = PeerId::from(local_key.public());
		let mut swarm = SwarmBuilder::new(transport, beh, local_peer_id).build();

		swarm.listen_on(local_address).map_err(|e| {
			log::debug!("swarm.listen_on {:?}", e);
			EigenError::ListenFailed
		})?;

		Ok(Self { swarm, peer })
	}

	/// Handle the request response event.
	fn handle_req_res_events(&mut self, event: RequestResponseEvent<Request, Response>) {
		use RequestResponseEvent::*;
		use RequestResponseMessage::{Request as Req, Response as Res};
		match event {
			Message {
				peer,
				message: Req { request: Request::Opinion(epoch, iter), channel, .. },
			} => {
				// We send the local opinion to the peer.
				let opinion = self.peer.get_local_opinion(&(peer, epoch, iter));
				let response = Response::Opinion(opinion);
				let res = self.swarm.behaviour_mut().send_response(channel, response);
				if let Err(e) = res {
					log::error!("Failed to send the response {:?}", e);
				}
			},
			Message { peer, message: Req { request: Request::Identify(pub_key), channel, .. } } => {
				self.peer.identify_neighbor(peer, pub_key);
				let response = Response::Identify(self.peer.pubkey.clone());
				let res = self.swarm.behaviour_mut().send_response(channel, response);
				if let Err(e) = res {
					log::error!("Failed to send the response {:?}", e);
				}
			},
			Message { peer, message: Res { response: Response::Opinion(opinion), .. } } => {
				// If we receive a response, we update the neighbors's opinion about us.
				self.peer.cache_neighbor_opinion((peer, opinion.epoch, opinion.iter), opinion);
			},
			Message { peer, message: Res { response: Response::Identify(pub_key), .. } } => {
				self.peer.identify_neighbor(peer, pub_key);
			},
			Message { message: Res { response, .. }, .. } => {
				log::error!("Received error response {:?}", response)
			},
			OutboundFailure { peer, request_id, error } => {
				log::error!("Outbound failure {:?} from {:?}: {:?}", request_id, peer, error);
			},
			InboundFailure { peer, request_id, error } => {
				log::error!("Inbound failure {:?} from {:?}: {:?}", request_id, peer, error);
			},
			ResponseSent { peer, request_id } => {
				log::debug!("Response sent {:?} to {:?}", request_id, peer);
			},
		};
	}

	/// Handle the identify protocol events.
	fn handle_identify_events(&mut self, event: IdentifyEvent) {
		match event {
			IdentifyEvent::Received { peer_id, info } => {
				self.peer.identify_neighbor_native(peer_id, info.public_key);
				log::info!("Neighbor identified {:?}", peer_id);
			},
			IdentifyEvent::Sent { peer_id } => {
				log::debug!("Identify request sent to {:?}", peer_id);
			},
			IdentifyEvent::Pushed { peer_id } => {
				log::debug!("Identify request pushed to {:?}", peer_id);
			},
			IdentifyEvent::Error { peer_id, error } => {
				log::error!("Identify error {:?} from {:?}", error, peer_id);
			},
		}
	}

	/// A method for handling the swarm events.
	pub fn handle_swarm_events(
		&mut self,
		event: SwarmEvent<
			EigenEvent,
			EitherError<ConnectionHandlerUpgrErr<IoError>, std::io::Error>,
		>,
	) {
		match event {
			SwarmEvent::Behaviour(EigenEvent::RequestResponse(event)) => {
				self.handle_req_res_events(event);
			},
			SwarmEvent::Behaviour(EigenEvent::Identify(event)) => {
				self.handle_identify_events(event);
			},
			SwarmEvent::NewListenAddr { address, .. } => {
				log::info!("Listening on {:?}", address);
			},
			// When we connect to a peer, we automatically add him as a neighbor.
			SwarmEvent::ConnectionEstablished { peer_id, .. } => {
				let res = self.peer.add_neighbor(peer_id);
				if let Err(e) = res {
					log::error!("Failed to add neighbor {:?}", e);
				}
				log::info!("Connection established with {:?}", peer_id);
			},
			// When we disconnect from a peer, we automatically remove him from the neighbors list.
			SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
				self.peer.remove_neighbor(peer_id);
				log::info!("Connection closed with {:?} ({:?})", peer_id, cause);
			},
			SwarmEvent::Dialing(peer_id) => {
				log::info!("Dialing {:?}", peer_id);
			},
			e => log::debug!("{:?}", e),
		}
	}

	/// Dial the neighbor directly.
	pub fn dial_neighbor(&mut self, addr: Multiaddr) {
		let res = self.swarm.dial(addr).map_err(|_| EigenError::DialError);
		log::debug!("swarm.dial {:?}", res);
	}

	/// Send the request for an opinion to all neighbors, in the passed epoch.
	pub fn send_epoch_requests(&mut self, epoch: Epoch, k: u32) {
		for peer_id in self.peer.neighbors() {
			let request = Request::Opinion(epoch, k);
			self.swarm.behaviour_mut().send_request(&peer_id, request);
		}
	}

	/// Start the main loop of the program. This function has two main tasks:
	/// - To start an interval timer for sending the request for opinions.
	/// - To handle the swarm + request/response events.
	/// The amount of intervals/epochs is determined by the `interval_limit`
	/// parameter.
	pub async fn main_loop(mut self, interval_limit: usize) {
		let now = Instant::now();
		// Set up epoch interval
		let epoch_interval = Duration::from_secs(EPOCH_INTERVAL);
		// Set up iter interval
		let iter_interval = Duration::from_secs(ITER_INTERVAL);
		let secs_until_next_epoch = Epoch::secs_until_next_epoch(epoch_interval.as_secs());
		log::info!("Epoch starts in: {} seconds", secs_until_next_epoch);
		// Figure out when the next epoch will start.
		let start = now + Duration::from_secs(secs_until_next_epoch);
		// Setup the epoch interval timer.
		let mut outer_interval = create_iter(start, epoch_interval, interval_limit);
		// Setup iteration interval timer
		let mut inner_interval = stream::pending::<u32>().boxed().fuse();

		loop {
			select! {
				biased;
				// The interval timer tick. This is where we request opinions from the neighbors.
				epoch_opt = outer_interval.next() => if let Some(epoch) = epoch_opt {
					log::info!("Epoch({}) has started", epoch);
					inner_interval = create_iter(Instant::now(), iter_interval, NUM_ITERATIONS as usize);
				} else {
					break;
				},
				iter_opt = inner_interval.next() => if let Some(iter) = iter_opt {
					let epoch = Epoch::current_epoch(epoch_interval.as_secs());
					let score = self.peer.global_trust_score_at(epoch, iter);
					log::info!("iter({}) score: {}", iter, score);
					// First we calculate the local opinions for the this iter.
					for peer in self.peer.neighbors() {
						self.peer.calculate_local_opinion(peer, epoch, iter);
					}
					// Send the request for opinions to all neighbors.
					self.send_epoch_requests(epoch, iter);
				},
				// The swarm event.
				event = self.swarm.select_next_some() => self.handle_swarm_events(event),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		constants::{MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS},
		peer::pubkey::Pubkey,
		utils::keypair_from_sk_bytes,
	};
	use eigen_trust_circuit::{
		halo2wrong::{
			curves::bn256::Bn256,
			halo2::poly::{commitment::ParamsProver, kzg::commitment::ParamsKZG},
		},
		poseidon::params::bn254_5x5::Params5x5Bn254,
		utils::{keygen, random_circuit},
	};
	use rand::thread_rng;
	use std::str::FromStr;

	const ADDR_1: &str = "/ip4/127.0.0.1/tcp/56706";
	const ADDR_2: &str = "/ip4/127.0.0.1/tcp/58601";
	const SK_1: &str = "AF4yAqwCPzpBcit4FtTrHso4BBR9onk7qS9Q1SWSLSaV";
	const SK_2: &str = "7VoQFngkSo36s5yzZtnjtZ5SLe1VGukCZdb5Uc9tSDNC";

	#[tokio::test]
	async fn should_emit_connection_event_on_bootstrap() {
		let sk_bytes1 = bs58::decode(SK_1).into_vec().unwrap();
		let sk_bytes2 = bs58::decode(SK_2).into_vec().unwrap();

		let local_key1 = keypair_from_sk_bytes(sk_bytes1).unwrap();
		let peer_id1 = local_key1.public().to_peer_id();

		let local_key2 = keypair_from_sk_bytes(sk_bytes2).unwrap();
		let peer_id2 = local_key2.public().to_peer_id();

		let local_address1 = Multiaddr::from_str(ADDR_1).unwrap();
		let local_address2 = Multiaddr::from_str(ADDR_2).unwrap();

		let params = ParamsKZG::new(9);

		let rng = &mut thread_rng();
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params5x5Bn254>(rng);
		let pk = keygen(&params, &random_circuit).unwrap();

		let peer1 = Peer::new(local_key1.clone(), params.clone(), pk.clone()).unwrap();
		let peer2 = Peer::new(local_key2.clone(), params, pk).unwrap();

		let mut node1 = Node::new(local_key1, local_address1.clone(), peer1).unwrap();
		let mut node2 = Node::new(local_key2, local_address2.clone(), peer2).unwrap();

		node1.dial_neighbor(local_address2);

		// For node 2
		// 1. New listen addr
		// 2. Incoming connection
		// 3. Connection established
		// For node 1
		// 1. New listen addr
		// 2. Connection established
		for _ in 0..5 {
			select! {
				event2 = node2.swarm.select_next_some() => {
					if let SwarmEvent::ConnectionEstablished { peer_id, .. } = event2 {
						assert_eq!(peer_id, peer_id1);
					}
				},
				event1 = node1.swarm.select_next_some() => {
					if let SwarmEvent::ConnectionEstablished { peer_id, .. } = event1 {
						assert_eq!(peer_id, peer_id2);
					}
				},

			}
		}
	}

	#[tokio::test]
	async fn should_identify_neighbors() {
		let sk_bytes1 = bs58::decode(SK_1).into_vec().unwrap();
		let sk_bytes2 = bs58::decode(SK_2).into_vec().unwrap();

		let local_key1 = keypair_from_sk_bytes(sk_bytes1).unwrap();
		let peer_id1 = local_key1.public().to_peer_id();

		let local_key2 = keypair_from_sk_bytes(sk_bytes2).unwrap();
		let peer_id2 = local_key2.public().to_peer_id();

		let local_address1 = Multiaddr::from_str(ADDR_1).unwrap();
		let local_address2 = Multiaddr::from_str(ADDR_2).unwrap();

		let params = ParamsKZG::new(9);

		let rng = &mut thread_rng();
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params5x5Bn254>(rng);
		let pk = keygen(&params, &random_circuit).unwrap();

		let peer1 = Peer::new(local_key1.clone(), params.clone(), pk.clone()).unwrap();
		let peer2 = Peer::new(local_key2.clone(), params, pk).unwrap();

		let mut node1 = Node::new(local_key1.clone(), local_address1, peer1).unwrap();

		let mut node2 = Node::new(local_key2.clone(), local_address2.clone(), peer2).unwrap();

		node1.dial_neighbor(local_address2);

		// For node 2
		// 1. New listen addr
		// 2. Incoming connection
		// 3. Connection established
		// For node 1
		// 1. New listen addr
		// 2. Connection established
		for _ in 0..9 {
			select! {
				event2 = node2.swarm.select_next_some() => node2.handle_swarm_events(event2),
				event1 = node1.swarm.select_next_some() => node1.handle_swarm_events(event1),

			}
		}

		let neighbors1: Vec<PeerId> = node1.peer.neighbors();
		let neighbors2: Vec<PeerId> = node2.peer.neighbors();
		let expected_neighbor1 = vec![peer_id2];
		let expected_neighbor2 = vec![peer_id1];
		assert_eq!(neighbors1, expected_neighbor1);
		assert_eq!(neighbors2, expected_neighbor2);

		let pubkey1 = node2.peer.get_pub_key_native(peer_id1).unwrap();
		let pubkey2 = node1.peer.get_pub_key_native(peer_id2).unwrap();
		assert_eq!(pubkey1, local_key1.public());
		assert_eq!(pubkey2, local_key2.public());
	}

	#[tokio::test]
	async fn should_handle_request_for_opinion() {
		let sk_bytes1 = bs58::decode(SK_1).into_vec().unwrap();
		let sk_bytes2 = bs58::decode(SK_2).into_vec().unwrap();

		let local_key1 = keypair_from_sk_bytes(sk_bytes1).unwrap();
		let peer_id1 = local_key1.public().to_peer_id();
		let pubkey1 = Pubkey::from_keypair(&local_key1).unwrap();

		let local_key2 = keypair_from_sk_bytes(sk_bytes2).unwrap();
		let peer_id2 = local_key2.public().to_peer_id();
		let pubkey2 = Pubkey::from_keypair(&local_key2).unwrap();

		let local_address1 = Multiaddr::from_str(ADDR_1).unwrap();
		let local_address2 = Multiaddr::from_str(ADDR_2).unwrap();

		let params = ParamsKZG::new(9);

		let rng = &mut thread_rng();
		let random_circuit =
			random_circuit::<Bn256, _, MAX_NEIGHBORS, NUM_BOOTSTRAP_PEERS, Params5x5Bn254>(rng);
		let pk = keygen(&params, &random_circuit).unwrap();

		let peer1 = Peer::new(local_key1.clone(), params.clone(), pk.clone()).unwrap();
		let peer2 = Peer::new(local_key2.clone(), params, pk).unwrap();

		let mut node1 = Node::new(local_key1, local_address1, peer1).unwrap();
		let mut node2 = Node::new(local_key2, local_address2.clone(), peer2).unwrap();

		node1.dial_neighbor(local_address2);

		// For node 2
		// 1. New listen addr
		// 2. Incoming connection
		// 3. Connection established
		// For node 1
		// 1. New listen addr
		// 2. Connection established
		for _ in 0..9 {
			select! {
				event2 = node2.swarm.select_next_some() => {
					node2.handle_swarm_events(event2)
				},
				event1 = node1.swarm.select_next_some() => {
					node1.handle_swarm_events(event1)
				},
			}
		}

		node1.peer.identify_neighbor(peer_id2, pubkey2);
		node2.peer.identify_neighbor(peer_id1, pubkey1);

		node1.peer.set_score(peer_id2, 5);
		node2.peer.set_score(peer_id1, 5);

		let epoch = Epoch(3);
		let iter = 0;

		for peer in node1.peer.neighbors() {
			node1.peer.calculate_local_opinion(peer, epoch, iter);
		}

		for peer in node2.peer.neighbors() {
			node2.peer.calculate_local_opinion(peer, epoch, iter);
		}

		node1.send_epoch_requests(epoch, iter);
		node2.send_epoch_requests(epoch, iter);

		// Expecting 2 request messages
		// Expecting 2 response sent messages
		// Expecting 2 response received messages
		// Total of 6 messages
		for _ in 0..6 {
			select! {
				event1 = node1.swarm.select_next_some() => {
					println!("{:?}", event1);
					node1.handle_swarm_events(event1);
				},
				event2 = node2.swarm.select_next_some() => {
					println!("{:?}", event2);
					node2.handle_swarm_events(event2);
				},
			}
		}

		let peer1_neighbor_opinion = node1.peer.get_neighbor_opinion(&(peer_id2, epoch, iter));
		let peer2_neighbor_opinion = node2.peer.get_neighbor_opinion(&(peer_id1, epoch, iter));

		assert_eq!(peer1_neighbor_opinion.epoch, Epoch(3));
		assert_eq!(peer1_neighbor_opinion.op, 0.5);

		assert_eq!(peer2_neighbor_opinion.epoch, Epoch(3));
		assert_eq!(peer2_neighbor_opinion.op, 0.5);
	}
}