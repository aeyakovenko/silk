//! Crds Gossip
//! This module ties together Crds and the push and pull gossip overlays.  The interface is
//! designed to run with a simulator or over a UDP network connection with messages up to a
//! packet::BLOB_DATA_SIZE size.

use bloom::Bloom;
use crds::Crds;
use crds_gossip_error::CrdsGossipError;
use crds_gossip_pull::CrdsGossipPull;
use crds_gossip_push::CrdsGossipPush;
use crds_value::CrdsValue;
use hash::Hash;
use solana_sdk::pubkey::Pubkey;

pub struct CrdsGossip {
    pub crds: Crds,
    id: Pubkey,
    push: CrdsGossipPush,
    pull: CrdsGossipPull,
}

impl Default for CrdsGossip {
    fn default() -> Self {
        CrdsGossip {
            crds: Crds::default(),
            id: Pubkey::default(),
            push: CrdsGossipPush::default(),
            pull: CrdsGossipPull::default(),
        }
    }
}

impl CrdsGossip {
    pub fn set_self(&mut self, id: Pubkey) {
        self.id = id;
    }
    /// process a push message to the network
    pub fn process_push_message(
        &mut self,
        value: CrdsValue,
        now: u64,
    ) -> Result<(), CrdsGossipError> {
        let old = self.push.process_push_message(&mut self.crds, value, now)?;
        old.map(|val| {
            self.pull
                .record_old_hash(val.value_hash, val.local_timestamp)
        });
        Ok(())
    }

    pub fn new_push_messages(&mut self, now: u64) -> (Pubkey, Vec<Pubkey>, Vec<CrdsValue>) {
        let (peers, values) = self.push.new_push_messages(&self.crds, now);
        (self.id, peers, values)
    }

    /// add the `from` to the peer's filter of nodes
    pub fn process_prune_msg(&mut self, peer: Pubkey, from: Pubkey) {
        self.push.process_prune_msg(peer, from)
    }

    /// refresh the push active set
    /// * ratio - number of actives to rotate
    pub fn refresh_push_active_set(&mut self, ratio: usize) {
        self.push.refresh_push_active_set(
            &self.crds,
            self.id,
            self.pull.pull_request_time.len(),
            ratio,
        )
    }

    /// purge old pending push messages
    pub fn purge_old_pending_push_messages(&mut self, min_time: u64) {
        self.push
            .purge_old_pending_push_messages(&self.crds, min_time);
    }
    pub fn purge_old_pushed_once_messages(&mut self, min_time: u64) {
        self.push.purge_old_pushed_once_messages(min_time);
    }
    /// generate a random request
    pub fn new_pull_request(
        &self,
        now: u64,
    ) -> Result<(Pubkey, Bloom<Hash>, CrdsValue), CrdsGossipError> {
        self.pull.new_pull_request(&self.crds, self.id, now)
    }

    /// time when a request to `from` was initiated
    /// This is used for weighted random selection durring `new_pull_request`
    /// It's important to use the local nodes request creation time as the weight
    /// instaad of the response received time otherwise failed nodes will increase their weight.
    pub fn mark_pull_request_creation_time(&mut self, from: Pubkey, now: u64) {
        self.pull.mark_pull_request_creation_time(from, now)
    }
    /// process a pull request and create a response
    pub fn process_pull_request(
        &mut self,
        caller: CrdsValue,
        filter: Bloom<Hash>,
        now: u64,
    ) -> Vec<CrdsValue> {
        self.pull
            .process_pull_request(&mut self.crds, caller, filter, now)
    }
    /// process a pull response
    pub fn process_pull_response(
        &mut self,
        from: Pubkey,
        response: Vec<CrdsValue>,
        now: u64,
    ) -> usize {
        self.pull
            .process_pull_response(&mut self.crds, from, response, now)
    }
    /// Purge values from the crds that are older then `active_timeout`
    /// The value_hash of an active item is put into self.purged_values queue
    pub fn purge_active(&mut self, min_ts: u64) {
        self.pull.purge_active(&mut self.crds, self.id, min_ts)
    }
    /// Purge values from the `self.purged_values` queue that are older then purge_timeout
    pub fn purge_purged(&mut self, min_ts: u64) {
        self.pull.purge_purged(min_ts)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use bincode::serialized_size;
    use contact_info::ContactInfo;
    use crds_gossip_push::{CRDS_GOSSIP_NUM_ACTIVE, CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS};
    use crds_value::CrdsValueLabel;
    use rayon::prelude::*;
    use signature::{Keypair, KeypairUtil};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Node = Arc<Mutex<CrdsGossip>>;
    type Network = HashMap<Pubkey, Node>;
    fn star_network_create(num: usize) -> Network {
        let entry = CrdsValue::ContactInfo(ContactInfo::new_localhost(Keypair::new().pubkey()));
        let mut network: HashMap<_, _> = (1..num)
            .map(|_| {
                let new =
                    CrdsValue::ContactInfo(ContactInfo::new_localhost(Keypair::new().pubkey()));
                let id = new.label().pubkey();
                let mut node = CrdsGossip::default();
                node.crds.insert(new.clone(), 0).unwrap();
                node.crds.insert(entry.clone(), 0).unwrap();
                node.set_self(id);
                (new.label().pubkey(), Arc::new(Mutex::new(node)))
            }).collect();
        let mut node = CrdsGossip::default();
        let id = entry.label().pubkey();
        node.crds.insert(entry.clone(), 0).unwrap();
        node.set_self(id);
        network.insert(id, Arc::new(Mutex::new(node)));
        network
    }

    fn ring_network_create(num: usize) -> Network {
        let mut network: HashMap<_, _> = (0..num)
            .map(|_| {
                let new =
                    CrdsValue::ContactInfo(ContactInfo::new_localhost(Keypair::new().pubkey()));
                let id = new.label().pubkey();
                let mut node = CrdsGossip::default();
                node.crds.insert(new.clone(), 0).unwrap();
                node.set_self(id);
                (new.label().pubkey(), Arc::new(Mutex::new(node)))
            }).collect();
        let keys: Vec<Pubkey> = network.keys().cloned().collect();
        for k in 0..keys.len() {
            let start_info = {
                let start = &network[&keys[k]];
                let start_id = start.lock().unwrap().id.clone();
                start
                    .lock()
                    .unwrap()
                    .crds
                    .lookup(&CrdsValueLabel::ContactInfo(start_id))
                    .unwrap()
                    .clone()
            };
            let end = network.get_mut(&keys[(k + 1) % keys.len()]).unwrap();
            end.lock().unwrap().crds.insert(start_info, 0).unwrap();
        }
        network
    }

    fn network_simulator_pull_only(network: &mut Network) {
        let num = network.len();
        let (converged, bytes_tx) = network_run_pull(network, 0, num * 2, 0.9);
        trace!(
            "network_simulator_pull_{}: converged: {} total_bytes: {}",
            num,
            converged,
            bytes_tx
        );
        assert!(converged >= 0.9);
    }

    fn network_simulator(network: &mut Network) {
        let num = network.len();
        // run for a small amount of time
        let (converged, bytes_tx) = network_run_pull(network, 0, 10, 1.0);
        trace!("network_simulator_push_{}: converged: {}", num, converged);
        // make sure there is someone in the active set
        let network_values: Vec<Node> = network.values().cloned().collect();
        network_values.par_iter().for_each(|node| {
            node.lock().unwrap().refresh_push_active_set(10);
        });
        let mut total_bytes = bytes_tx;
        for second in 1..num {
            let start = second * 10;
            let end = (second + 1) * 10;
            let now = (start * 100) as u64;
            // push a message to the network
            network_values.par_iter().for_each(|locked_node| {
                let node = &mut locked_node.lock().unwrap();
                let mut m = node
                    .crds
                    .lookup(&CrdsValueLabel::ContactInfo(node.id))
                    .and_then(|v| v.clone().contact_info())
                    .unwrap();
                m.wallclock = now;
                node.process_push_message(CrdsValue::ContactInfo(m), now)
                    .unwrap();
            });
            // push for a bit
            let (queue_size, bytes_tx) = network_run_push(network, start, end);
            total_bytes += bytes_tx;
            trace!(
                "network_simulator_push_{}: queue_size: {} bytes: {}",
                num,
                queue_size,
                bytes_tx
            );
            // pull for a bit
            let (converged, bytes_tx) = network_run_pull(network, start, end, 1.0);
            total_bytes += bytes_tx;
            trace!(
                "network_simulator_push_{}: converged: {} bytes: {} total_bytes: {}",
                num,
                converged,
                bytes_tx,
                total_bytes
            );
            if converged > 0.9 {
                break;
            }
        }
    }

    fn network_run_push(network: &mut Network, start: usize, end: usize) -> (usize, usize) {
        let mut bytes: usize = 0;
        let mut num_msgs: usize = 0;
        let mut total: usize = 0;
        let num = network.len();
        let mut prunes: usize = 0;
        let mut delivered: usize = 0;
        let network_values: Vec<Node> = network.values().cloned().collect();
        for t in start..end {
            let now = t as u64 * 100;
            let requests: Vec<_> = network_values
                .par_iter()
                .map(|node| {
                    if now > CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS {
                        node.lock().unwrap().purge_old_pending_push_messages(
                            now - CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS as u64,
                        );
                        node.lock().unwrap().purge_old_pushed_once_messages(
                            now - 2 * CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS as u64,
                        );
                    }
                    node.lock().unwrap().new_push_messages(now)
                }).collect();
            let transfered: Vec<_> = requests.par_iter().map(|(from, peers, msgs)| {
                let mut bytes: usize = 0;
                let mut delivered: usize = 0;
                let mut num_msgs: usize = 0;
                let mut prunes: usize = 0;
                for to in peers {
                    bytes += serialized_size(msgs).unwrap() as usize;
                    num_msgs += 1;
                    for m in msgs {
                        let origin = m.label().pubkey();
                        let rsp = network
                            .get(&to)
                            .map(|node| node.lock().unwrap().process_push_message(m.clone(), now))
                            .unwrap();
                        if rsp == Err(CrdsGossipError::PushMessagePrune) {
                            prunes += 1;
                            bytes += serialized_size(&to).unwrap() as usize;
                            bytes += serialized_size(&origin).unwrap() as usize;
                            network
                                .get(&from)
                                .map(|node| node.lock().unwrap().process_prune_msg(*to, origin))
                                .unwrap();
                        }
                        delivered += rsp.is_ok() as usize;
                    }
                }
                (bytes, delivered, num_msgs, prunes)
            }).collect();
            for (b,d,m,p) in transfered {
                bytes += b;
                delivered += d;
                num_msgs += m;
                prunes += p;
            }
            if now % CRDS_GOSSIP_PUSH_MSG_TIMEOUT_MS == 0 && now > 0 {
                network_values.par_iter().for_each(|node| {
                    node.lock()
                        .unwrap()
                        .refresh_push_active_set(CRDS_GOSSIP_NUM_ACTIVE);
                });
            }
            total = network_values
                .par_iter()
                .map(|v| v.lock().unwrap().push.num_pending())
                .sum();
            trace!(
                "network_run_push_{}: now: {} queue: {} bytes: {} num_msgs: {} prunes: {} delivered: {}",
                num,
                now,
                total,
                bytes,
                num_msgs,
                prunes,
                delivered,
            );
        }
        (total, bytes)
    }

    fn network_run_pull(
        network: &mut Network,
        start: usize,
        end: usize,
        max_convergance: f64,
    ) -> (f64, usize) {
        let mut bytes: usize = 0;
        let mut msgs: usize = 0;
        let mut overhead: usize = 0;
        let mut convergance = 0f64;
        let num = network.len();
        let network_values: Vec<Node> = network.values().cloned().collect();
        for t in start..end {
            let now = t as u64 * 100;
            let mut requests: Vec<_> = {
                network_values
                    .par_iter()
                    .filter_map(|from| from.lock().unwrap().new_pull_request(now).ok())
                    .collect()
            };
            let transfered: Vec<_> = requests
                .into_par_iter()
                .map(|(to, request, caller_info)| {
                    let mut bytes: usize = 0;
                    let mut msgs: usize = 0;
                    let mut overhead: usize = 0;
                    let from = caller_info.label().pubkey();
                    bytes += request.keys.len();
                    bytes += (request.bits.len() / 8) as usize;
                    bytes += serialized_size(&caller_info).unwrap() as usize;
                    let rsp = network
                        .get(&to)
                        .map(|node| {
                            node.lock()
                                .unwrap()
                                .process_pull_request(caller_info, request, now)
                        }).unwrap();
                    bytes += serialized_size(&rsp).unwrap() as usize;
                    msgs += rsp.len();
                    network.get(&from).map(|node| {
                        node.lock()
                            .unwrap()
                            .mark_pull_request_creation_time(from, now);
                        overhead += node.lock().unwrap().process_pull_response(from, rsp, now);
                    });
                    (bytes, msgs, overhead)
                }).collect();
            for (b,m, o) in transfered {
                bytes += b;
                msgs += m;
                overhead += o;
            }
            let total: usize = network_values
                .par_iter()
                .map(|v| v.lock().unwrap().crds.table.len())
                .sum();
            convergance = total as f64 / ((num * num) as f64);
            if convergance > max_convergance {
                break;
            }
            trace!(
                "network_run_pull_{}: now: {} connections: {} convergance: {} bytes: {} msgs: {} overhead: {}",
                num,
                now,
                total,
                convergance,
                bytes,
                msgs,
                overhead
            );
        }
        (convergance, bytes)
    }

    #[test]
    fn test_star_network_pull_50() {
        let mut network = star_network_create(50);
        network_simulator_pull_only(&mut network);
    }
    #[test]
    fn test_star_network_pull_100() {
        let mut network = star_network_create(100);
        network_simulator_pull_only(&mut network);
    }
    #[test]
    fn test_star_network_push_star_200() {
        use logger;
        logger::setup();
        let mut network = star_network_create(200);
        network_simulator(&mut network);
    }
    #[test]
    fn test_star_network_push_ring_200() {
        use logger;
        logger::setup();
        let mut network = ring_network_create(200);
        network_simulator(&mut network);
    }
    #[test]
    #[ignore]
    fn test_star_network_large_pull() {
        use logger;
        logger::setup();
        let mut network = star_network_create(2000);
        network_simulator_pull_only(&mut network);
    }
    #[test]
    #[ignore]
    fn test_ring_network_large_push() {
        use logger;
        logger::setup();
        let mut network = ring_network_create(4000);
        network_simulator(&mut network);
    }
    #[test]
    #[ignore]
    fn test_star_network_large_push() {
        use logger;
        logger::setup();
        let mut network = star_network_create(4000);
        network_simulator(&mut network);
    }
}
