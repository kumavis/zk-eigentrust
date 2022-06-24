//! The module for the peer related functionalities, like:
//! - Adding/removing neighbors
//! - Calculating the global trust score
//! - Calculating local scores toward neighbors for a given epoch
//! - Keeping track of neighbors scores towards us

pub mod opinion;

use crate::{epoch::Epoch, EigenError};
use eigen_trust_circuit::{
	halo2wrong::{
		curves::{
			bn256::{Bn256, G1Affine},
			secp256k1::Secp256k1Affine,
		},
		halo2::{plonk::ProvingKey, poly::kzg::commitment::ParamsKZG},
	},
	utils::{keygen, random_circuit},
};
use libp2p::{core::PublicKey, identity::Keypair, PeerId};
use opinion::Opinion;
use rand::thread_rng;
use std::collections::HashMap;

/// The number of neighbors the peer can have.
/// This is also the maximum number of peers that can be connected to the
/// node.
pub const MAX_NEIGHBORS: usize = 256;

/// The peer struct.
pub struct Peer {
	pub(crate) neighbors: [Option<PeerId>; MAX_NEIGHBORS],
	pubkeys: HashMap<PeerId, PublicKey>,
	neighbor_scores: HashMap<PeerId, u32>,
	cached_neighbor_opinion: HashMap<(PeerId, Epoch), Opinion<MAX_NEIGHBORS>>,
	cached_local_opinion: HashMap<(PeerId, Epoch), Opinion<MAX_NEIGHBORS>>,
	keypair: Keypair,
	params: ParamsKZG<Bn256>,
	proving_key: ProvingKey<G1Affine>,
}

impl Peer {
	/// Creates a new peer.
	pub fn new(keypair: Keypair, params: ParamsKZG<Bn256>) -> Self {
		let mut rng = thread_rng();
		let random_circuit = random_circuit::<Bn256, Secp256k1Affine, _, MAX_NEIGHBORS>(&mut rng);
		let pk = keygen(&params, &random_circuit).unwrap();
		Peer {
			neighbors: [None; MAX_NEIGHBORS],
			pubkeys: HashMap::new(),
			neighbor_scores: HashMap::new(),
			cached_neighbor_opinion: HashMap::new(),
			cached_local_opinion: HashMap::new(),
			keypair,
			params,
			proving_key: pk,
		}
	}

	/// Adds a neighbor in the first available spot.
	pub fn add_neighbor(&mut self, peer_id: PeerId) -> Result<(), EigenError> {
		if self.neighbors.contains(&Some(peer_id)) {
			return Ok(());
		}
		let index = self
			.neighbors
			.iter()
			.position(|&x| x.is_none())
			.ok_or(EigenError::MaxNeighboursReached)?;
		self.neighbors[index] = Some(peer_id);
		Ok(())
	}

	/// Identifies a neighbor, by saving its public key.
	pub fn identify_neighbor(&mut self, peer_id: PeerId, pubkey: PublicKey) {
		self.pubkeys.insert(peer_id, pubkey);
	}

	/// Removes a neighbor, if found.
	pub fn remove_neighbor(&mut self, peer_id: PeerId) {
		let index_res = self.neighbors.iter().position(|&x| x == Some(peer_id));
		if let Some(index) = index_res {
			self.neighbors[index] = None;
		}
	}

	/// Returns the neighbors of the peer.
	pub fn neighbors(&self) -> Vec<PeerId> {
		self.neighbors.iter().filter_map(|&x| x).collect()
	}

	/// Set the local score towards a neighbor.
	pub fn set_score(&mut self, peer_id: PeerId, score: u32) {
		self.neighbor_scores.insert(peer_id, score);
	}

	/// Calculate the local trust score toward all neighbors in the specified
	/// epoch.
	pub fn calculate_local_opinions(&mut self, k: Epoch) {
		let op_ji = self.get_neighbor_opinions_at(k);

		for peer_id in self.neighbors() {
			let score = self.neighbor_scores.get(&peer_id).unwrap_or(&0);
			let normalized_score = self.get_normalized_score(*score);
			let pubkey = self.pubkeys.get(&peer_id).unwrap();
			let opinion = Opinion::generate(
				&self.keypair,
				pubkey,
				k.next(),
				op_ji,
				normalized_score,
				&self.params,
				&self.proving_key
			).unwrap();

			self.cache_local_opinion((peer_id, opinion.k), opinion);
		}
	}

	/// Returns all of the opinions of the neighbors in the specified epoch.
	pub fn get_neighbor_opinions_at(&self, k: Epoch) -> [f64; MAX_NEIGHBORS] {
		let op_ji = self.neighbors.map(|peer| {
			if peer.is_none() {
				return 0.
			}
			let peer_id = peer.unwrap();
			let opinion = self.get_neighbor_opinion(&(peer_id, k));
			let pubkey_p = self.get_pub_key(peer_id);
			let pubkey_v = self.keypair.public();
			let vk = self.proving_key.get_vk();

			let res = opinion.verify(&pubkey_p, &pubkey_v, &self.params, &vk).unwrap();
			
			if res { return opinion.op };
			0.
		});

		op_ji
	}

	/// Calculate the global trust score at the specified epoch.
	pub fn global_trust_score_at(&self, at: Epoch) -> f64 {
		let op_ji = self.get_neighbor_opinions_at(at);
		let t_i = op_ji.iter().fold(0., |acc, t| acc + t);
		
		t_i
	}

	/// Returns sum of local scores.
	pub fn get_sum_of_scores(&self) -> u32 {
		let mut sum = 0;
		for peer_id in self.neighbors() {
			let score = self.neighbor_scores.get(&peer_id).unwrap_or(&0);
			sum += score;
		}
		sum
	}

	/// Returns the normalized score.
	pub fn get_normalized_score(&self, score: u32) -> f64 {
		let sum = self.get_sum_of_scores();
		let f_raw_score = f64::from(score);
		let f_sum = f64::from(sum);
		f_raw_score / f_sum
	}

	/// Returns the local score towards a neighbor in a specified epoch.
	pub fn get_local_opinion(&self, key: &(PeerId, Epoch)) -> Opinion<MAX_NEIGHBORS> {
		self
			.cached_local_opinion
			.get(key)
			.unwrap_or(&Opinion::empty())
			.clone()
	}

	/// Caches the local opinion towards a peer in a specified epoch.
	pub fn cache_local_opinion(&mut self, key: (PeerId, Epoch), opinion: Opinion<MAX_NEIGHBORS>) {
		self.cached_local_opinion.insert(key, opinion);
	}

	/// Returns the neighbor's opinion towards us in a specified epoch.
	pub fn get_neighbor_opinion(&self, key: &(PeerId, Epoch)) -> Opinion<MAX_NEIGHBORS> {
		self
			.cached_neighbor_opinion
			.get(key)
			.unwrap_or(&Opinion::empty())
			.clone()
	}

	/// Caches the neighbor opinion towards us in specified epoch.
	pub fn cache_neighbor_opinion(&mut self, key: (PeerId, Epoch), opinion: Opinion<MAX_NEIGHBORS>) {
		self.cached_neighbor_opinion.insert(key, opinion);
	}

	/// Get the public key of a neighbor.
	pub fn get_pub_key(&self, peer_id: PeerId) -> PublicKey {
		self.pubkeys.get(&peer_id).cloned().unwrap()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use eigen_trust_circuit::{
		ecdsa::SigData,
		halo2wrong::{
			curves::{secp256k1::Fq as Secp256k1Scalar}, halo2::poly::commitment::ParamsProver,
		},
	};
	use libp2p::core::identity::Keypair;

	#[test]
	fn should_create_peer() {
		let kp = Keypair::generate_secp256k1();
		let params = ParamsKZG::new(18);
		let peer = Peer::new(kp, params);
		assert_eq!(peer.get_sum_of_scores(), 0);
	}

	#[test]
	fn should_cache_local_and_global_opinion() {
		let kp = Keypair::generate_secp256k1();
		let params = ParamsKZG::new(18);
		let mut peer = Peer::new(kp, params);

		let epoch = Epoch(0);
		let neighbor_id = PeerId::random();
		let sig = SigData::<Secp256k1Scalar>::empty();

		let pubkey = Keypair::generate_secp256k1().public();
		peer.identify_neighbor(neighbor_id, pubkey);

		let opinion = Opinion::new(epoch, sig, 0.5, Vec::new());
		peer.cache_local_opinion((neighbor_id, epoch), opinion.clone());
		peer.cache_neighbor_opinion((neighbor_id, epoch), opinion.clone());

		assert_eq!(peer.get_local_opinion(&(neighbor_id, epoch)), opinion);
		assert_eq!(peer.get_neighbor_opinion(&(neighbor_id, epoch)), opinion);
	}

	#[test]
	fn should_add_and_remove_neghbours() {
		let kp = Keypair::generate_secp256k1();
		let params = ParamsKZG::new(18);
		let mut peer = Peer::new(kp, params);
		let neighbor_id = PeerId::random();

		peer.add_neighbor(neighbor_id).unwrap();
		let num_neighbors = peer.neighbors().len();
		assert_eq!(num_neighbors, 1);

		peer.remove_neighbor(neighbor_id);
		let num_neighbors = peer.neighbors().len();
		assert_eq!(num_neighbors, 0);
	}

	#[test]
	fn should_add_neighbors_and_calculate_global_score() {
		let rng = &mut thread_rng();
		let local_keypair = Keypair::generate_secp256k1();
		let local_pubkey = local_keypair.public();

		let params = ParamsKZG::<Bn256>::new(18);
		let random_circuit = random_circuit::<Bn256, Secp256k1Affine, _, MAX_NEIGHBORS>(&mut rng.clone());
		let pk = keygen(&params, &random_circuit).unwrap();

		let mut peer = Peer::new(local_keypair, params.clone());

		let epoch = Epoch(0);
		for _ in 0..4 {
			let kp = Keypair::generate_secp256k1();
			let pubkey = kp.public();
			let peer_id = pubkey.to_peer_id();

			peer.add_neighbor(peer_id).unwrap();
			peer.identify_neighbor(peer_id, pubkey.clone());
			peer.set_score(peer_id, 5);

			// Create neighbor opinion.
			let mut op_ji = [0.; MAX_NEIGHBORS];
			op_ji[0] = 0.1;
			let c_v = 1.;
			let opinion = Opinion::generate(&kp, &local_pubkey, epoch, op_ji, c_v, &params, &pk).unwrap();

			// Sanity check
			assert!(opinion.verify(&pubkey, &local_pubkey, &params, &pk.get_vk()).unwrap());

			// Cache neighbor opinion.
			peer.cache_neighbor_opinion((peer_id, epoch), opinion);
		}

		peer.calculate_local_opinions(epoch);

		let op_ji = peer.neighbors.map(|p| {
			if let Some(peer_id) = p {
				peer.get_neighbor_opinion(&(peer_id, epoch)).op	
			} else {
				0.
			}
		});
		let t_i = op_ji.iter().fold(0., |acc, t| acc + t);
		let true_global_score = 0.4;

		assert_eq!(true_global_score, t_i);

		let c_v = true_global_score * 0.25;

		for peer_id in peer.neighbors() {
			let opinion = peer.get_local_opinion(&(peer_id, epoch.next()));
			assert_eq!(opinion.op, c_v);
		}
	}
}
