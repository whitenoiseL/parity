// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, SocketAddrV6, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::str::FromStr;
use std::{fs, mem, slice};
use ethereum_types::H512;
use rlp::{UntrustedRlp, RlpStream, DecoderError};
use network::{Error, ErrorKind, AllowIP, IpFilter};
use discovery::{TableUpdates, NodeEntry};
use ip_utils::*;
use serde_json;

/// Node public key
pub type NodeId = H512;

#[derive(Debug, Clone)]
/// Node address info
pub struct NodeEndpoint {
	/// IP(V4 or V6) address
	pub address: SocketAddr,
	/// Conneciton port.
	pub udp_port: u16
}

impl NodeEndpoint {
	pub fn udp_address(&self) -> SocketAddr {
		match self.address {
			SocketAddr::V4(a) => SocketAddr::V4(SocketAddrV4::new(a.ip().clone(), self.udp_port)),
			SocketAddr::V6(a) => SocketAddr::V6(SocketAddrV6::new(a.ip().clone(), self.udp_port, a.flowinfo(), a.scope_id())),
		}
	}

	pub fn is_allowed(&self, filter: &IpFilter) -> bool {
		(self.is_allowed_by_predefined(&filter.predefined) || filter.custom_allow.iter().any(|ipnet| {
			self.address.ip().is_within(ipnet)
		}))
		&& !filter.custom_block.iter().any(|ipnet| {
			self.address.ip().is_within(ipnet)
		})
	}

	pub fn is_allowed_by_predefined(&self, filter: &AllowIP) -> bool {
		match filter {
			&AllowIP::All => true,
			&AllowIP::Private => self.address.ip().is_usable_private(),
			&AllowIP::Public => self.address.ip().is_usable_public(),
			&AllowIP::None => false,
		}
	}

	pub fn from_rlp(rlp: &UntrustedRlp) -> Result<Self, DecoderError> {
		let tcp_port = rlp.val_at::<u16>(2)?;
		let udp_port = rlp.val_at::<u16>(1)?;
		let addr_bytes = rlp.at(0)?.data()?;
		let address = match addr_bytes.len() {
			4 => Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(addr_bytes[0], addr_bytes[1], addr_bytes[2], addr_bytes[3]), tcp_port))),
			16 => unsafe {
				let o: *const u16 = mem::transmute(addr_bytes.as_ptr());
				let o = slice::from_raw_parts(o, 8);
				Ok(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::new(o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]), tcp_port, 0, 0)))
			},
			_ => Err(DecoderError::RlpInconsistentLengthAndData)
		}?;
		Ok(NodeEndpoint { address: address, udp_port: udp_port })
	}

	pub fn to_rlp(&self, rlp: &mut RlpStream) {
		match self.address {
			SocketAddr::V4(a) => {
				rlp.append(&(&a.ip().octets()[..]));
			}
			SocketAddr::V6(a) => unsafe {
				let o: *const u8 = mem::transmute(a.ip().segments().as_ptr());
				rlp.append(&slice::from_raw_parts(o, 16));
			}
		};
		rlp.append(&self.udp_port);
		rlp.append(&self.address.port());
	}

	pub fn to_rlp_list(&self, rlp: &mut RlpStream) {
		rlp.begin_list(3);
		self.to_rlp(rlp);
	}

	/// Validates that the port is not 0 and address IP is specified
	pub fn is_valid(&self) -> bool {
		self.udp_port != 0 && self.address.port() != 0 &&
		match self.address {
			SocketAddr::V4(a) => !a.ip().is_unspecified(),
			SocketAddr::V6(a) => !a.ip().is_unspecified()
		}
	}
}

impl FromStr for NodeEndpoint {
	type Err = Error;

	/// Create endpoint from string. Performs name resolution if given a host name.
	fn from_str(s: &str) -> Result<NodeEndpoint, Error> {
		let address = s.to_socket_addrs().map(|mut i| i.next());
		match address {
			Ok(Some(a)) => Ok(NodeEndpoint {
				address: a,
				udp_port: a.port()
			}),
			Ok(_) => Err(ErrorKind::AddressResolve(None).into()),
			Err(e) => Err(ErrorKind::AddressResolve(Some(e)).into())
		}
	}
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum PeerType {
	_Required,
	Optional
}

pub struct Node {
	pub id: NodeId,
	pub endpoint: NodeEndpoint,
	pub peer_type: PeerType,
	pub attempts: u32,
	pub failures: u32,
}

const DEFAULT_FAILURE_PERCENTAGE: usize = 50;

impl Node {
	pub fn new(id: NodeId, endpoint: NodeEndpoint) -> Node {
		Node {
			id: id,
			endpoint: endpoint,
			peer_type: PeerType::Optional,
			attempts: 0,
			failures: 0,
		}
	}

	/// Returns the node's failure percentage (0..100) in buckets of 5%. If there are 0 connection attempts for this
	/// node the default failure percentage is returned (50%).
	pub fn failure_percentage(&self) -> usize {
		if self.attempts == 0 {
			DEFAULT_FAILURE_PERCENTAGE
		} else {
			(self.failures * 100 / self.attempts / 5 * 5) as usize
		}
	}
}

impl Display for Node {
	fn fmt(&self, f: &mut Formatter) -> fmt::Result {
		if self.endpoint.udp_port != self.endpoint.address.port() {
			write!(f, "enode://{:x}@{}+{}", self.id, self.endpoint.address, self.endpoint.udp_port)?;
		} else {
			write!(f, "enode://{:x}@{}", self.id, self.endpoint.address)?;
		}
		Ok(())
	}
}

impl FromStr for Node {
	type Err = Error;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let (id, endpoint) = if s.len() > 136 && &s[0..8] == "enode://" && &s[136..137] == "@" {
			(s[8..136].parse().map_err(|_| ErrorKind::InvalidNodeId)?, NodeEndpoint::from_str(&s[137..])?)
		}
		else {
			(NodeId::new(), NodeEndpoint::from_str(s)?)
		};

		Ok(Node {
			id: id,
			endpoint: endpoint,
			peer_type: PeerType::Optional,
			attempts: 0,
			failures: 0,
		})
	}
}

impl PartialEq for Node {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id
	}
}
impl Eq for Node {}

impl Hash for Node {
	fn hash<H>(&self, state: &mut H) where H: Hasher {
		self.id.hash(state)
	}
}

const MAX_NODES: usize = 1024;
const NODES_FILE: &str = "nodes.json";

/// Node table backed by disk file.
pub struct NodeTable {
	nodes: HashMap<NodeId, Node>,
	useless_nodes: HashSet<NodeId>,
	path: Option<String>,
}

impl NodeTable {
	pub fn new(path: Option<String>) -> NodeTable {
		NodeTable {
			path: path.clone(),
			nodes: NodeTable::load(path),
			useless_nodes: HashSet::new(),
		}
	}

	/// Add a node to table
	pub fn add_node(&mut self, mut node: Node) {
		// preserve attempts and failure counter
		let (attempts, failures) =
			self.nodes.get(&node.id).map_or((0, 0), |n| (n.attempts, n.failures));

		node.attempts = attempts;
		node.failures = failures;

		self.nodes.insert(node.id.clone(), node);
	}

	fn ordered_entries(&self) -> Vec<&Node> {
		let mut refs: Vec<&Node> = self.nodes.values()
			.filter(|n| !self.useless_nodes.contains(&n.id))
			.collect();

		refs.sort_by(|a, b| {
			a.failure_percentage().cmp(&b.failure_percentage())
				.then_with(|| a.failures.cmp(&b.failures))
				.then_with(|| b.attempts.cmp(&a.attempts)) // we use reverse ordering for number of attempts
		});

		refs
	}

	/// Returns node ids sorted by failure percentage, for nodes with the same failure percentage the absolute number of
	/// failures is considered.
	pub fn nodes(&self, filter: IpFilter) -> Vec<NodeId> {
		self.ordered_entries().iter()
			.filter(|n| n.endpoint.is_allowed(&filter))
			.map(|n| n.id)
			.collect()
	}

	/// Ordered list of all entries by failure percentage, for nodes with the same failure percentage the absolute
	/// number of failures is considered.
	pub fn entries(&self) -> Vec<NodeEntry> {
		self.ordered_entries().iter().map(|n| NodeEntry {
			endpoint: n.endpoint.clone(),
			id: n.id.clone(),
		}).collect()
	}

	/// Get particular node
	pub fn get_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
		self.nodes.get_mut(id)
	}

	/// Check if a node exists in the table.
	pub fn contains(&self, id: &NodeId) -> bool {
		self.nodes.contains_key(id)
	}

	/// Apply table changes coming from discovery
	pub fn update(&mut self, mut update: TableUpdates, reserved: &HashSet<NodeId>) {
		for (_, node) in update.added.drain() {
			let entry = self.nodes.entry(node.id.clone()).or_insert_with(|| Node::new(node.id.clone(), node.endpoint.clone()));
			entry.endpoint = node.endpoint;
		}
		for r in update.removed {
			if !reserved.contains(&r) {
				self.nodes.remove(&r);
			}
		}
	}

	/// Increase failure counte for a node
	pub fn note_failure(&mut self, id: &NodeId) {
		if let Some(node) = self.nodes.get_mut(id) {
			node.failures += 1;
		}
	}

	/// Mark as useless, no further attempts to connect until next call to `clear_useless`.
	pub fn mark_as_useless(&mut self, id: &NodeId) {
		self.useless_nodes.insert(id.clone());
	}

	/// Atempt to connect to useless nodes again.
	pub fn clear_useless(&mut self) {
		self.useless_nodes.clear();
	}

	/// Save the nodes.json file.
	pub fn save(&self) {
		let mut path = match self.path {
			Some(ref path) => PathBuf::from(path),
			None => return,
		};
		if let Err(e) = fs::create_dir_all(&path) {
			warn!("Error creating node table directory: {:?}", e);
			return;
		}
		path.push(NODES_FILE);
		let node_ids = self.nodes(IpFilter::default());
		let nodes = node_ids.into_iter()
			.map(|id| self.nodes.get(&id).expect("self.nodes() only returns node IDs from self.nodes"))
			.take(MAX_NODES)
			.map(|node| node.clone())
			.map(Into::into)
			.collect();
		let table = json::NodeTable { nodes };

		match fs::File::create(&path) {
			Ok(file) => {
				if let Err(e) = serde_json::to_writer_pretty(file, &table) {
					warn!("Error writing node table file: {:?}", e);
				}
			},
			Err(e) => {
				warn!("Error creating node table file: {:?}", e);
			}
		}
	}

	fn load(path: Option<String>) -> HashMap<NodeId, Node> {
		let path = match path {
			Some(path) => PathBuf::from(path).join(NODES_FILE),
			None => return Default::default(),
		};

		let file = match fs::File::open(&path) {
			Ok(file) => file,
			Err(e) => {
				debug!("Error opening node table file: {:?}", e);
				return Default::default();
			},
		};
		let res: Result<json::NodeTable, _> = serde_json::from_reader(file);
		match res {
			Ok(table) => {
				table.nodes.into_iter()
					.filter_map(|n| n.into_node())
					.map(|n| (n.id.clone(), n))
					.collect()
			},
			Err(e) => {
				warn!("Error reading node table file: {:?}", e);
				Default::default()
			},
		}
	}
}

impl Drop for NodeTable {
	fn drop(&mut self) {
		self.save();
	}
}

/// Check if node url is valid
pub fn validate_node_url(url: &str) -> Option<Error> {
	match Node::from_str(url) {
		Ok(_) => None,
		Err(e) => Some(e)
	}
}

mod json {
	use super::*;

	#[derive(Serialize, Deserialize)]
	pub struct NodeTable {
		pub nodes: Vec<Node>,
	}

	#[derive(Serialize, Deserialize)]
	pub struct Node {
		pub url: String,
		pub attempts: u32,
		pub failures: u32,
	}

	impl Node {
		pub fn into_node(self) -> Option<super::Node> {
			match super::Node::from_str(&self.url) {
				Ok(mut node) => {
					node.attempts = self.attempts;
					node.failures = self.failures;
					Some(node)
				},
				_ => None,
			}
		}
	}

	impl<'a> From<&'a super::Node> for Node {
		fn from(node: &'a super::Node) -> Self {
			Node {
				url: format!("{}", node),
				attempts: node.attempts,
				failures: node.failures,
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr};
	use ethereum_types::H512;
	use std::str::FromStr;
	use tempdir::TempDir;
	use ipnetwork::IpNetwork;

	#[test]
	fn endpoint_parse() {
		let endpoint = NodeEndpoint::from_str("123.99.55.44:7770");
		assert!(endpoint.is_ok());
		let v4 = match endpoint.unwrap().address {
			SocketAddr::V4(v4address) => v4address,
			_ => panic!("should ve v4 address")
		};
		assert_eq!(SocketAddrV4::new(Ipv4Addr::new(123, 99, 55, 44), 7770), v4);
	}

	#[test]
	fn node_parse() {
		assert!(validate_node_url("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").is_none());
		let node = Node::from_str("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770");
		assert!(node.is_ok());
		let node = node.unwrap();
		let v4 = match node.endpoint.address {
			SocketAddr::V4(v4address) => v4address,
			_ => panic!("should ve v4 address")
		};
		assert_eq!(SocketAddrV4::new(Ipv4Addr::new(22, 99, 55, 44), 7770), v4);
		assert_eq!(
			H512::from_str("a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap(),
			node.id);
	}

	#[test]
	fn table_failure_percentage_order() {
		let node1 = Node::from_str("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let node2 = Node::from_str("enode://b979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let node3 = Node::from_str("enode://c979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let node4 = Node::from_str("enode://d979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let id1 = H512::from_str("a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		let id2 = H512::from_str("b979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		let id3 = H512::from_str("c979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		let id4 = H512::from_str("d979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		let mut table = NodeTable::new(None);

		table.add_node(node1);
		table.add_node(node2);
		table.add_node(node3);
		table.add_node(node4);

		// node 1 - failure percentage 100%
		table.get_mut(&id1).unwrap().attempts = 2;
		table.note_failure(&id1);
		table.note_failure(&id1);

		// node2 - failure percentage 33%
		table.get_mut(&id2).unwrap().attempts = 3;
		table.note_failure(&id2);

		// node3 - failure percentage 0%
		table.get_mut(&id3).unwrap().attempts = 1;

		// node4 - failure percentage 50% (default when no attempts)

		let r = table.nodes(IpFilter::default());

		assert_eq!(r[0][..], id3[..]);
		assert_eq!(r[1][..], id2[..]);
		assert_eq!(r[2][..], id4[..]);
		assert_eq!(r[3][..], id1[..]);
	}

	#[test]
	fn table_save_load() {
		let tempdir = TempDir::new("").unwrap();
		let node1 = Node::from_str("enode://a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let node2 = Node::from_str("enode://b979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c@22.99.55.44:7770").unwrap();
		let id1 = H512::from_str("a979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		let id2 = H512::from_str("b979fb575495b8d6db44f750317d0f4622bf4c2aa3365d6af7c284339968eef29b69ad0dce72a4d8db5ebb4968de0e3bec910127f134779fbcb0cb6d3331163c").unwrap();
		{
			let mut table = NodeTable::new(Some(tempdir.path().to_str().unwrap().to_owned()));
			table.add_node(node1);
			table.add_node(node2);

			table.get_mut(&id1).unwrap().attempts = 1;
			table.get_mut(&id2).unwrap().attempts = 1;
			table.note_failure(&id2);
		}

		{
			let table = NodeTable::new(Some(tempdir.path().to_str().unwrap().to_owned()));
			let r = table.nodes(IpFilter::default());
			assert_eq!(r[0][..], id1[..]);
			assert_eq!(r[1][..], id2[..]);
		}
	}

	#[test]
	fn custom_allow() {
		let filter = IpFilter {
			predefined: AllowIP::None,
			custom_allow: vec![IpNetwork::from_str(&"10.0.0.0/8").unwrap(), IpNetwork::from_str(&"1.0.0.0/8").unwrap()],
			custom_block: vec![],
		};
		assert!(!NodeEndpoint::from_str("123.99.55.44:7770").unwrap().is_allowed(&filter));
		assert!(NodeEndpoint::from_str("10.0.0.1:7770").unwrap().is_allowed(&filter));
		assert!(NodeEndpoint::from_str("1.0.0.55:5550").unwrap().is_allowed(&filter));
	}

	#[test]
	fn custom_block() {
		let filter = IpFilter {
			predefined: AllowIP::All,
			custom_allow: vec![],
			custom_block: vec![IpNetwork::from_str(&"10.0.0.0/8").unwrap(), IpNetwork::from_str(&"1.0.0.0/8").unwrap()],
		};
		assert!(NodeEndpoint::from_str("123.99.55.44:7770").unwrap().is_allowed(&filter));
		assert!(!NodeEndpoint::from_str("10.0.0.1:7770").unwrap().is_allowed(&filter));
		assert!(!NodeEndpoint::from_str("1.0.0.55:5550").unwrap().is_allowed(&filter));
	}

	#[test]
	fn custom_allow_ipv6() {
		let filter = IpFilter {
			predefined: AllowIP::None,
			custom_allow: vec![IpNetwork::from_str(&"fc00::/8").unwrap()],
			custom_block: vec![],
		};
		assert!(NodeEndpoint::from_str("[fc00::]:5550").unwrap().is_allowed(&filter));
		assert!(!NodeEndpoint::from_str("[fd00::]:5550").unwrap().is_allowed(&filter));
	}

	#[test]
	fn custom_block_ipv6() {
		let filter = IpFilter {
			predefined: AllowIP::All,
			custom_allow: vec![],
			custom_block: vec![IpNetwork::from_str(&"fc00::/8").unwrap()],
		};
		assert!(!NodeEndpoint::from_str("[fc00::]:5550").unwrap().is_allowed(&filter));
		assert!(NodeEndpoint::from_str("[fd00::]:5550").unwrap().is_allowed(&filter));
	}
}
