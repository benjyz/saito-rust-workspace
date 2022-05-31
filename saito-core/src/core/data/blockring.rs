use log::trace;

use crate::common::defs::SaitoHash;
use crate::core::data::block::Block;
use crate::core::data::blockchain::GENESIS_PERIOD;

pub const RING_BUFFER_LENGTH: u64 = 2 * GENESIS_PERIOD;

//
// TODO -- shift to a RingBuffer ?
// - block_ring --> ring
// - block_ring_lc_pos --> lc_pos
//
#[derive(Debug)]
pub struct BlockRing {
    //
    // each ring_item is a point on our blockchain
    //
    // include Slice-VecDeque and have a slice that points to
    // contiguous entries for rapid lookups, inserts and updates?
    //
    pub block_ring: Vec<RingItem>,
    /// a ring of blocks, index is not the block_id.
    block_ring_lc_pos: Option<usize>,
}

impl BlockRing {
    /// Create new `BlockRing`
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        //
        // initialize the block-ring
        //
        let mut init_block_ring: Vec<RingItem> = vec![];
        for _i in 0..RING_BUFFER_LENGTH {
            init_block_ring.push(RingItem::new());
        }

        BlockRing {
            block_ring: init_block_ring,
            block_ring_lc_pos: None,
        }
    }

    pub fn add_block(&mut self, block: &Block) {
        let insert_pos = block.get_id() % RING_BUFFER_LENGTH;
        self.block_ring[(insert_pos as usize)].add_block(block.get_id(), block.get_hash());
    }

    pub fn contains_block_hash_at_block_id(&self, block_id: u64, block_hash: SaitoHash) -> bool {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        self.block_ring[(insert_pos as usize)].contains_block_hash(block_hash)
    }

    pub fn get_latest_block_hash(&self) -> SaitoHash {
        match self.block_ring_lc_pos {
            Some(block_ring_lc_pos) => match self.block_ring[block_ring_lc_pos].lc_pos {
                Some(lc_pos) => self.block_ring[block_ring_lc_pos].block_hashes[lc_pos],
                None => [0; 32],
            },
            None => [0; 32],
        }
    }

    pub fn get_latest_block_id(&self) -> u64 {
        match self.block_ring_lc_pos {
            Some(block_ring_lc_pos) => match self.block_ring[block_ring_lc_pos].lc_pos {
                Some(lc_pos) => self.block_ring[block_ring_lc_pos].block_ids[lc_pos],
                None => 0,
            },
            None => 0,
        }
    }

    pub fn get_longest_chain_block_hash_by_block_id(&self, id: u64) -> SaitoHash {
        let insert_pos = (id % RING_BUFFER_LENGTH) as usize;
        match self.block_ring[insert_pos].lc_pos {
            Some(lc_pos) => self.block_ring[insert_pos].block_hashes[lc_pos],
            None => [0; 32],
        }
    }

    pub fn is_block_hash_at_block_id(&self, block_id: u64, block_hash: SaitoHash) -> bool {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        for i in 0..self.block_ring[(insert_pos as usize)].block_hashes.len() {
            if self.block_ring[(insert_pos as usize)].block_hashes[i] == block_hash {
               return true;
            }
        }
        return false;
    }

    pub fn is_empty(&self) -> bool {
        return self.block_ring_lc_pos.is_none();
    }


    pub fn delete_block(&mut self, block_id: u64, block_hash: SaitoHash) {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        self.block_ring[(insert_pos as usize)].delete_block(block_id, block_hash);
    }

    pub fn get_block_hashes_at_block_id(&mut self, block_id: u64) -> Vec<SaitoHash> {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        let mut v: Vec<SaitoHash> = vec![];
        for i in 0..self.block_ring[(insert_pos as usize)].block_hashes.len() {
            if self.block_ring[(insert_pos as usize)].block_ids[i] == block_id {
                v.push(self.block_ring[(insert_pos as usize)].block_hashes[i].clone());
            }
        }
        v
    }

    pub fn on_chain_reorganization(&mut self, block_id: u64, hash: SaitoHash, lc: bool) -> bool {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        if !self.block_ring[(insert_pos as usize)].on_chain_reorganization(hash, lc) {
            return false;
        }
        if lc {
            self.block_ring_lc_pos = Some(insert_pos as usize);
        } else {
            //
            // if we are unsetting the longest-chain, we automatically
            // roll backwards and set the longest-chain to the previous
            // position if available. this adds some complexity to unwinding
            // the chain but should ensure that in most situations there is
            // always a known longest-chain position. this is not guaranteed
	    // behavior, so the blockring should not be treated as something
	    // that guarantees correctness of lc_pos in situations like this.
            //
            if let Some(block_ring_lc_pos) = self.block_ring_lc_pos {
                if block_ring_lc_pos == insert_pos as usize {
                    let previous_block_idx;

                    if block_ring_lc_pos > 0 {
                        previous_block_idx = block_ring_lc_pos - 1;
                    } else {
                        previous_block_idx = RING_BUFFER_LENGTH as usize - 1;
                    }

                    // reset to lc_pos to unknown
                    self.block_ring_lc_pos = None;

                    // but try to find it
                    // let previous_block_idx_lc_pos = self.block_ring[previous_block_idx as usize].lc_pos;
                    if let Some(previous_block_idx_lc_pos) =
                        self.block_ring[previous_block_idx as usize].lc_pos
                    {
                        if self.block_ring[previous_block_idx].block_ids.len()
                            > previous_block_idx_lc_pos
                        {
                            if self.block_ring[previous_block_idx].block_ids
                                [previous_block_idx_lc_pos]
                                == block_id - 1
                            {
                                self.block_ring_lc_pos = Some(previous_block_idx);
                            }
                        }
                    }
                }
            }
        }
        true
    }

    pub fn print_lc(&self) {
        for i in 0..GENESIS_PERIOD {
            if !self.block_ring[(i as usize)].block_hashes.is_empty() {
                trace!(
                    "Block {:?}: {:?}",
                    i,
                    self.get_longest_chain_block_hash_by_block_id(i)
                );
            }
        }
    }
}


//
// This is an index with shorthand information on the block_ids and hashes of the blocks
// in the longest-chain.
//
// The BlockRing is a fixed size Vector which can be made contiguous and theoretically
// made available for fast-access through a slice with the same lifetime as the vector
// itself.
//
#[derive(Debug)]
pub struct RingItem {
    lc_pos: Option<usize>,
    // which idx in the vectors below points to the longest-chain block
    pub block_hashes: Vec<SaitoHash>,
    block_ids: Vec<u64>,
}

impl RingItem {
    pub fn new() -> Self {
        Self {
            lc_pos: None,
            block_hashes: vec![],
            block_ids: vec![],
        }
    }

    pub fn contains_block_hash(&self, hash: SaitoHash) -> bool {
        self.block_hashes.iter().any(|&i| i == hash)
    }

    pub fn add_block(&mut self, block_id: u64, hash: SaitoHash) {
        self.block_hashes.push(hash);
        self.block_ids.push(block_id);
    }

    pub fn delete_block(&mut self, block_id: u64, hash: SaitoHash) {
        let mut new_block_hashes: Vec<SaitoHash> = vec![];
        let mut new_block_ids: Vec<u64> = vec![];
        let mut idx_loop = 0;
        let mut new_lc_pos = Some(0);

        for i in 0..self.block_ids.len() {
            if self.block_ids[i] == block_id && self.block_hashes[i] == hash {
            } else {
                new_block_hashes.push(self.block_hashes[i]);
                new_block_ids.push(self.block_ids[i]);
                if self.lc_pos == Some(i) {
                    new_lc_pos = Some(idx_loop);
                }
                idx_loop += 1;
            }
        }

        self.block_hashes = new_block_hashes;
        self.block_ids = new_block_ids;
        self.lc_pos = new_lc_pos;
    }

    pub fn on_chain_reorganization(&mut self, hash: SaitoHash, lc: bool) -> bool {
        if !lc {
            self.lc_pos = None;
        } else {
            self.lc_pos = self.block_hashes.iter().position(|b_hash| b_hash == &hash);
        }

        true
    }
}



#[cfg(test)]
mod tests {

    use crate::core::data::blockring::{BlockRing, RingItem};
    use crate::core::data::block::{Block, BlockType};

    use crate::core::data::blockchain::GENESIS_PERIOD;
    pub const RING_BUFFER_LENGTH: u64 = 2 * GENESIS_PERIOD;

    #[test]
    fn blockring_new_test() {
        let blockring = BlockRing::new();
        assert_eq!(blockring.block_ring.len() as u64, RING_BUFFER_LENGTH);
        assert_eq!(blockring.block_ring_lc_pos, None);
    }

    #[test]
    fn blockring_add_block_test() {

        let mut blockring = BlockRing::new();
        let mut block = Block::new();
        block.generate_hash();
	let block_hash = block.get_hash();
	let block_id = block.get_id();


	// everything is empty to start
	assert_eq!(blockring.is_empty(), true);
	assert_eq!(blockring.get_latest_block_hash(), [0; 32]);
	assert_eq!(blockring.get_latest_block_id(), 0);
	assert_eq!(blockring.get_longest_chain_block_hash_by_block_id(0), [0; 32]);
	assert_eq!(blockring.contains_block_hash_at_block_id(block.get_id(), block.get_hash()), false);

	blockring.add_block(&block);
	blockring.on_chain_reorganization(block.get_id(), block.get_hash(), true);

	assert_eq!(blockring.is_empty(), false);
	assert_eq!(blockring.get_latest_block_hash(), block_hash);
	assert_eq!(blockring.get_latest_block_id(), block_id);
	assert_eq!(blockring.get_longest_chain_block_hash_by_block_id(block_id), block_hash);
	assert_eq!(blockring.contains_block_hash_at_block_id(block.get_id(), block.get_hash()), true);

    }


    #[test]
    fn blockring_delete_block_test() {

        let mut blockring = BlockRing::new();
        let mut block = Block::new();
        block.generate_hash();
	let block_hash = block.get_hash();
	let block_id = block.get_id();


	// everything is empty to start
	assert_eq!(blockring.is_empty(), true);
	assert_eq!(blockring.get_latest_block_hash(), [0; 32]);
	assert_eq!(blockring.get_latest_block_id(), 0);
	assert_eq!(blockring.get_longest_chain_block_hash_by_block_id(0), [0; 32]);
	assert_eq!(blockring.contains_block_hash_at_block_id(block.get_id(), block.get_hash()), false);

	blockring.add_block(&block);
	blockring.on_chain_reorganization(block.get_id(), block.get_hash(), true);

	assert_eq!(blockring.is_empty(), false);
	assert_eq!(blockring.get_latest_block_hash(), block_hash);
	assert_eq!(blockring.get_latest_block_id(), block_id);
	assert_eq!(blockring.get_longest_chain_block_hash_by_block_id(block_id), block_hash);
	assert_eq!(blockring.contains_block_hash_at_block_id(block.get_id(), block.get_hash()), true);

	blockring.delete_block(block.get_id(), block.get_hash());
	assert_eq!(blockring.contains_block_hash_at_block_id(block.get_id(), block.get_hash()), false);

    }



}

