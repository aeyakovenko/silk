use crate::cuda_runtime::PinnedVec;
use crate::packet::Packet;
use crate::recycler::Recycler;
use crate::sigverify::TxOffset;

#[derive(Default, Clone)]
pub struct RecyclerCache {
    recycler_offsets: Recycler<TxOffset>,
    recycler_buffer: Recycler<PinnedVec<u8>>,
    recycler_packets: Recycler<PinnedVec<Packet>>,
}

impl RecyclerCache {
    pub fn warmed() -> Self {
        Self {
            recycler_offsets: Recycler::warmed(50, 4096),
            recycler_buffer: Recycler::warmed(50, 4096),
            recycler_packets: Recycler::warmed(1024, 1024),
        }
    }
    pub fn offsets(&self) -> &Recycler<TxOffset> {
        &self.recycler_offsets
    }
    pub fn buffer(&self) -> &Recycler<PinnedVec<u8>> {
        &self.recycler_buffer
    }
    pub fn packets(&self) -> &Recycler<PinnedVec<Packet>> {
        &self.recycler_packets
    }
}
