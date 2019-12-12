#![feature(test)]

extern crate test;

use rand::{thread_rng, Rng};
use solana_core::cluster_info::{ClusterInfo, Node};
use solana_core::contact_info::ContactInfo;
use solana_perf::packet::{Packet, Packets};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::timing::timestamp;
use std::collections::HashMap;
use std::net::UdpSocket;
use test::Bencher;

#[bench]
fn broadcast_shreds_bench(bencher: &mut Bencher) {
    solana_logger::setup();
    let leader_pubkey = Pubkey::new_rand();
    let leader_info = Node::new_localhost_with_pubkey(&leader_pubkey);
    let mut cluster_info = ClusterInfo::new_with_invalid_keypair(leader_info.info.clone());
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();

    const NUM_SHREDS: usize = 32;
    let mut shreds = Packets::default();
    (0..NUM_SHREDS).for_each(|_| {
        shreds.packets.push(Packet::default());
    });
    let seeds = vec![vec![[0u8; 32]; NUM_SHREDS]];
    let mut stakes = HashMap::new();
    const NUM_PEERS: usize = 200;
    for _ in 0..NUM_PEERS {
        let id = Pubkey::new_rand();
        let contact_info = ContactInfo::new_localhost(&id, timestamp());
        cluster_info.insert_info(contact_info);
        stakes.insert(id, thread_rng().gen_range(1, NUM_PEERS) as u64);
    }
    bencher.iter(move || {
        let shreds = shreds.clone();
        cluster_info
            .broadcast_shreds(&socket, &mut [shreds], &seeds, Some(&stakes))
            .unwrap();
    });
}
