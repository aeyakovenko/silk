//! The `crdt` module defines a data structure that is shared by all the nodes in the network over
//! a gossip control plane.  The goal is to share small bits of of-chain information and detect and
//! repair partitions.
//!
//! This CRDT only supports a very limited set of types.  A map of PublicKey -> Versioned Struct.
//! The last version is always picked durring an update.
//!
//! The network is arranged in layers:
//!
//! * layer 0 - Leader.
//! * layer 1 - As many nodes as we can fit
//! * layer 2 - Everyone else, if layer 1 is `2^10`, layer 2 should be able to fit `2^20` number of nodes.
//!
//! Accountant needs to provide an interface for us to query the stake weight

use bincode::{deserialize, serialize};
use byteorder::{LittleEndian, ReadBytesExt};
use hash::Hash;
use result::{Error, Result};
use ring::rand::{SecureRandom, SystemRandom};
use rayon::prelude::*;
use signature::{PublicKey, Signature};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{sleep, spawn, JoinHandle};
use std::time::Duration;
use packet::SharedBlob;

/// Structure to be replicated by the network
#[derive(Serialize, Deserialize, Clone)]
pub struct ReplicatedData {
    pub id: PublicKey,
    sig: Signature,
    /// should always be increasing
    version: u64,
    /// address to connect to for gossip
    pub gossip_addr: SocketAddr,
    /// address to connect to for replication
    pub replicate_addr: SocketAddr,
    /// address to connect to when this node is leader
    serve_addr: SocketAddr,
    /// current leader identity
    current_leader_id: PublicKey,
    /// last verified hash that was submitted to the leader
    last_verified_hash: Hash,
    /// last verified count, always increasing
    last_verified_count: u64,
}

impl ReplicatedData {
    pub fn new(id: PublicKey,
               gossip_addr: SocketAddr,
               replicate_addr: SocketAddr,
               serve_addr: SocketAddr) -> ReplicatedData {
        let daddr:SocketAddr = "0.0.0.0:0".parse().unwrap();
        ReplicatedData {
            id,
            sig: Signature::default(),
            version: 0,
            gossip_addr,
            replicate_addr,
            serve_addr,
            current_leader_id: PublicKey::default(),
            last_verified_hash: Hash::default(),
            last_verified_count: 0,
        }
    }
}

/// `Crdt` structure keeps a table of `ReplicatedData` structs
/// # Properties
/// * `table` - map of public id's to versioned and signed ReplicatedData structs
/// * `local` - map of public id's to what `self.update_index` `self.table` was updated
/// * `remote` - map of public id's to the `remote.update_index` was sent
/// * `update_index` - my update index
/// # Remarks
/// This implements two services, `gossip` and `listen`.
/// * `gossip` - asynchronously ask nodes to send updates
/// * `listen` - listen for requests and responses
/// No attempt to keep track of timeouts or dropped requests is made, or should be.
pub struct Crdt {
    table: HashMap<PublicKey, ReplicatedData>,
    /// Value of my update index when entry in table was updated.
    /// Nodes will ask for updates since `update_index`, and this node
    /// should respond with all the identities that are greater then the
    /// request's `update_index` in this list
    local: HashMap<PublicKey, u64>,
    /// The value of the remote update index that i have last seen
    /// This Node will ask external nodes for updates since the value in this list
    remote: HashMap<PublicKey, u64>,
    update_index: u64,
    me: PublicKey,
    timeout: Duration,
}
// TODO These messages should be signed, and go through the gpu pipeline for spam filtering
#[derive(Serialize, Deserialize)]
enum Protocol {
    /// forward your own latest data structure when requesting an update
    /// this doesn't update the `remote` update index, but it allows the
    /// recepient of this request to add knowledge of this node to the network
    RequestUpdates(u64, ReplicatedData),
    //TODO might need a since?
    /// from id, form's last update index, ReplicatedData
    ReceiveUpdates(PublicKey, u64, Vec<ReplicatedData>),
}

impl Crdt {
    pub fn new(me: ReplicatedData) -> Crdt {
        assert_eq!(me.version, 0);
        let mut g = Crdt {
            table: HashMap::new(),
            local: HashMap::new(),
            remote: HashMap::new(),
            me: me.id,
            update_index: 1,
            timeout: Duration::new(0, 100_000),
        };
        g.local.insert(me.id, g.update_index);
        g.table.insert(me.id, me);
        g
    }
    pub fn my_data(&self) -> &ReplicatedData {
        &self.table[&self.me]
    }
    pub fn leader_data(&self) -> &ReplicatedData {
        &self.table[&self.table[&self.me].current_leader_id]
    }
    pub fn insert(&mut self, v: &ReplicatedData) {
        // TODO check that last_verified types are always increasing
        if self.table.get(&v.id).is_none() || (v.version > self.table[&v.id].version) {
            //somehow we signed a message for our own identity with a higher version that we have storred ourselves
            assert!(self.me != v.id);
            trace!("insert! {}", v.version);
            self.update_index += 1;
            let _ = self.table.insert(v.id, v.clone());
            let _ = self.local.insert(v.id, self.update_index);
        } else {
            trace!("INSERT FAILED {}", v.version);
        }
    }

    /// broadcast messages from the leader to layer 1 nodes
    /// # Remarks
    /// We need to avoid having obj locked while doing any io, such as the `send_to`
    pub fn broadcast(
        obj: &Arc<RwLock<Self>>,
        blobs: &Vec<SharedBlob>,
        s: &UdpSocket,
        transmit_index: &mut u64
    ) -> Result<()> {
        let (me, table): (ReplicatedData, Vec<ReplicatedData>) = {
            // copy to avoid locking durring IO
            let robj = obj.read().unwrap();
            let cloned_table:Vec<ReplicatedData> = robj.table.values().cloned().collect();
            (robj.table[&robj.me].clone(), cloned_table)
        };
        let errs: Vec<_> = table.iter()
            .enumerate()
            .cycle()
            .zip(blobs.iter())
            .map(|((i,v),b)| {
                if me.id == v.id {
                    return Ok(0);
                }
                // only leader should be broadcasting
                assert!(me.current_leader_id != v.id);
                let mut blob = b.write().unwrap();
                blob.set_index(*transmit_index + i as u64);
                s.send_to(&blob.data[..blob.meta.size], &v.replicate_addr)
            })
            .collect();
        for e in errs {
            trace!("retransmit result {:?}", e);
            match e {
                Err(e) => return Err(Error::IO(e)),
                _ => (),
            }
            *transmit_index += 1;
        }
        Ok(())
    }

    /// retransmit messages from the leader to layer 1 nodes
    /// # Remarks
    /// We need to avoid having obj locked while doing any io, such as the `send_to`
    pub fn retransmit(
        obj: &Arc<RwLock<Self>>,
        blob: &SharedBlob,
        s: &UdpSocket
    ) -> Result<()> {
        let (me, table): (ReplicatedData, Vec<ReplicatedData>) = {
            // copy to avoid locking durring IO
            let s = obj.read().unwrap();
            (s.table[&s.me].clone(), s.table.values().cloned().collect())
        };
        let rblob = blob.read().unwrap();
        let errs: Vec<_> = table
            .par_iter()
            .map(|v| {
                if me.id == v.id {
                    return Ok(0);
                }
                if me.current_leader_id == v.id {
                    trace!("skip retransmit to leader{:?}", v.id);
                    return Ok(0);
                }
                trace!("retransmit blob to {}", v.replicate_addr);
                s.send_to(&rblob.data[..rblob.meta.size], &v.replicate_addr)
            })
            .collect();
        for e in errs {
            trace!("retransmit result {:?}", e);
            match e {
                Err(e) => return Err(Error::IO(e)),
                _ => (),
            }
        }
        Ok(())
    }

    fn random() -> u64 {
        let rnd = SystemRandom::new();
        let mut buf = [0u8; 8];
        rnd.fill(&mut buf).unwrap();
        let mut rdr = Cursor::new(&buf);
        rdr.read_u64::<LittleEndian>().unwrap()
    }
    fn get_updates_since(&self, v: u64) -> (PublicKey, u64, Vec<ReplicatedData>) {
        trace!("get updates since {}", v);
        let data = self.table
            .values()
            .filter(|x| self.local[&x.id] > v)
            .cloned()
            .collect();
        let id = self.me;
        let ups = self.update_index;
        (id, ups, data)
    }

    /// Create a random gossip request
    /// # Returns
    /// (A,B,C)
    /// * A - Remote gossip address
    /// * B - My gossip address
    /// * C - Remote update index to request updates since
    fn gossip_request(&self) -> (SocketAddr, Protocol) {
        let n = (Self::random() as usize) % self.table.len();
        trace!("random {:?} {}", &self.me[0..1], n);
        let v = self.table.values().nth(n).unwrap().clone();
        let remote_update_index = *self.remote.get(&v.id).unwrap_or(&0);
        let req = Protocol::RequestUpdates(remote_update_index, self.table[&self.me].clone());
        (v.gossip_addr, req)
    }

    /// At random pick a node and try to get updated changes from them
    fn run_gossip(obj: &Arc<RwLock<Self>>) -> Result<()> {
        //TODO we need to keep track of stakes and weight the selection by stake size
        //TODO cache sockets

        // Lock the object only to do this operation and not for any longer
        // especially not when doing the `sock.send_to`
        let (remote_gossip_addr, req) = obj.read().unwrap().gossip_request();
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        // TODO this will get chatty, so we need to first ask for number of updates since
        // then only ask for specific data that we dont have
        let r = serialize(&req)?;
        sock.send_to(&r, remote_gossip_addr)?;
        Ok(())
    }

    /// Apply updates that we received from the identity `from`
    /// # Arguments
    /// * `from` - identity of the sender of the updates
    /// * `update_index` - the number of updates that `from` has completed and this set of `data` represents
    /// * `data` - the update data
    fn apply_updates(&mut self, from: PublicKey, update_index: u64, data: &[ReplicatedData]) {
        trace!("got updates {}", data.len());
        // TODO we need to punish/spam resist here
        // sig verify the whole update and slash anyone who sends a bad update
        for v in data {
            self.insert(&v);
        }
        *self.remote.entry(from).or_insert(update_index) = update_index;
    }

    /// randomly pick a node and ask them for updates asynchronously
    pub fn gossip(obj: Arc<RwLock<Self>>, exit: Arc<AtomicBool>) -> JoinHandle<()> {
        spawn(move || loop {
            let _ = Self::run_gossip(&obj);
            if exit.load(Ordering::Relaxed) {
                return;
            }
            //TODO this should be a tuned parameter
            sleep(obj.read().unwrap().timeout);
        })
    }

    /// Process messages from the network
    fn run_listen(obj: &Arc<RwLock<Self>>, sock: &UdpSocket) -> Result<()> {
        //TODO cache connections
        let mut buf = vec![0u8; 1024 * 64];
        let (amt, src) = sock.recv_from(&mut buf)?;
        trace!("got request from {}", src);
        buf.resize(amt, 0);
        let r = deserialize(&buf)?;
        match r {
            // TODO sigverify these
            Protocol::RequestUpdates(v, reqdata) => {
                trace!("RequestUpdates {}", v);
                let addr = reqdata.gossip_addr;
                // only lock for this call, dont lock durring IO `sock.send_to` or `sock.recv_from`
                let (from, ups, data) = obj.read().unwrap().get_updates_since(v);
                trace!("get updates since response {} {}", v, data.len());
                let rsp = serialize(&Protocol::ReceiveUpdates(from, ups, data))?;
                trace!("send_to {}", addr);
                //TODO verify reqdata belongs to sender
                obj.write().unwrap().insert(&reqdata);
                sock.send_to(&rsp, addr).unwrap();
                trace!("send_to done!");
            }
            Protocol::ReceiveUpdates(from, ups, data) => {
                trace!("ReceivedUpdates");
                obj.write().unwrap().apply_updates(from, ups, &data);
            }
        }
        Ok(())
    }
    pub fn listen(
        obj: Arc<RwLock<Self>>,
        sock: UdpSocket,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        sock.set_read_timeout(Some(Duration::new(2, 0))).unwrap();
        spawn(move || loop {
            let _ = Self::run_listen(&obj, &sock);
            if exit.load(Ordering::Relaxed) {
                return;
            }
        })
    }
}

#[cfg(test)]
mod test {
    use crdt::{Crdt, ReplicatedData};
    use signature::KeyPair;
    use signature::KeyPairUtil;
    use std::net::UdpSocket;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, RwLock};
    use std::thread::{sleep, JoinHandle};
    use std::time::Duration;

    use rayon::iter::*;
    use streamer::{blob_receiver, retransmitter};
    use std::sync::mpsc::channel;
    use subscribers::{Node, Subscribers};
    use packet::{Blob, BlobRecycler};
    use std::collections::VecDeque;

    /// Test that the network converges.
    /// Run until every node in the network has a full ReplicatedData set.
    /// Check that nodes stop sending updates after all the ReplicatedData has been shared.
    /// tests that actually use this function are below
    fn run_gossip_topo<F>(topo: F)
    where
        F: Fn(&Vec<(Arc<RwLock<Crdt>>, JoinHandle<()>)>) -> (),
    {
        let num: usize = 5;
        let exit = Arc::new(AtomicBool::new(false));
        let listen: Vec<_> = (0..num)
            .map(|_| {
                let gossip = UdpSocket::bind("0.0.0.0:0").unwrap();
                let replicate = UdpSocket::bind("0.0.0.0:0").unwrap();
                let serve = UdpSocket::bind("0.0.0.0:0").unwrap();
                let pubkey = KeyPair::new().pubkey();
                let d = ReplicatedData::new(pubkey,
                                            gossip.local_addr().unwrap(),
                                            replicate.local_addr().unwrap(),
                                            serve.local_addr().unwrap(),
                                            );
                let crdt = Crdt::new(d);
                let c = Arc::new(RwLock::new(crdt));
                let l = Crdt::listen(c.clone(), gossip, exit.clone());
                (c, l)
            })
            .collect();
        topo(&listen);
        let gossip: Vec<_> = listen
            .iter()
            .map(|&(ref c, _)| Crdt::gossip(c.clone(), exit.clone()))
            .collect();
        let mut done = true;
        for _ in 0..(num * 16) {
            done = true;
            for &(ref c, _) in listen.iter() {
                trace!(
                    "done updates {} {}",
                    c.read().unwrap().table.len(),
                    c.read().unwrap().update_index
                );
                //make sure the number of updates doesn't grow unbounded
                assert!(c.read().unwrap().update_index <= num as u64);
                //make sure we got all the updates
                if c.read().unwrap().table.len() != num {
                    done = false;
                }
            }
            if done == true {
                break;
            }
            sleep(Duration::new(1, 0));
        }
        exit.store(true, Ordering::Relaxed);
        for j in gossip {
            j.join().unwrap();
        }
        for (c, j) in listen.into_iter() {
            j.join().unwrap();
            // make it clear what failed
            // protocol is to chatty, updates should stop after everyone receives `num`
            assert!(c.read().unwrap().update_index <= num as u64);
            // protocol is not chatty enough, everyone should get `num` entries
            assert_eq!(c.read().unwrap().table.len(), num);
        }
        assert!(done);
    }
    /// ring a -> b -> c -> d -> e -> a
    #[test]
    fn gossip_ring_test() {
        run_gossip_topo(|listen| {
            let num = listen.len();
            for n in 0..num {
                let y = n % listen.len();
                let x = (n + 1) % listen.len();
                let mut xv = listen[x].0.write().unwrap();
                let yv = listen[y].0.read().unwrap();
                let mut d = yv.table[&yv.me].clone();
                d.version = 0;
                xv.insert(&d);
            }
        });
    }

    /// star (b,c,d,e) -> a
    #[test]
    fn gossip_star_test() {
        run_gossip_topo(|listen| {
            let num = listen.len();
            for n in 0..(num - 1) {
                let x = 0;
                let y = (n + 1) % listen.len();
                let mut xv = listen[x].0.write().unwrap();
                let yv = listen[y].0.read().unwrap();
                let mut d = yv.table[&yv.me].clone();
                d.version = 0;
                xv.insert(&d);
            }
        });
    }

    /// Test that insert drops messages that are older
    #[test]
    fn insert_test() {
        let mut d = ReplicatedData::new(KeyPair::new().pubkey(),
                                        "127.0.0.1:1234".parse().unwrap(),
                                        "127.0.0.1:1235".parse().unwrap(),
                                        "127.0.0.1:1236".parse().unwrap(),
                                        );
        assert_eq!(d.version, 0);
        let mut crdt = Crdt::new(d.clone());
        assert_eq!(crdt.table[&d.id].version, 0);
        d.version = 2;
        crdt.insert(&d);
        assert_eq!(crdt.table[&d.id].version, 2);
        d.version = 1;
        crdt.insert(&d);
        assert_eq!(crdt.table[&d.id].version, 2);
    }

    #[test]
    pub fn test_crdt_retransmit() {
        let s1 = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let s2 = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let s3 = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let n1 = Node::new([0; 8], 0, s1.local_addr().unwrap());
        let n2 = Node::new([0; 8], 0, s2.local_addr().unwrap());
        let mut s = Subscribers::new(n1.clone(), n2.clone(), &[]);
        let n3 = Node::new([0; 8], 0, s3.local_addr().unwrap());
        s.insert(&[n3]);
        let mut b = Blob::default();
        b.meta.size = 10;
        let s4 = UdpSocket::bind("127.0.0.1:0").expect("bind");
        s.retransmit(&mut b, &s4).unwrap();
        let res: Vec<_> = [s1, s2, s3]
            .into_par_iter()
            .map(|s| {
                let mut b = Blob::default();
                s.set_read_timeout(Some(Duration::new(1, 0))).unwrap();
                s.recv_from(&mut b.data).is_err()
            })
            .collect();
        assert_eq!(res, [true, true, false]);
        let mut n4 = Node::default();
        n4.addr = "255.255.255.255:1".parse().unwrap();
        s.insert(&[n4]);
        assert!(s.retransmit(&mut b, &s4).is_err());
    }
}
