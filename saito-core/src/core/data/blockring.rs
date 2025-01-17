use tracing::trace;

use crate::common::defs::SaitoHash;
use crate::core::data::block::Block;
use crate::core::data::blockchain::GENESIS_PERIOD;
use crate::core::data::ringitem::RingItem;

pub const RING_BUFFER_LENGTH: u64 = 2 * GENESIS_PERIOD;

//
// TODO -- shift to a RingBuffer ? or Slice-VecDeque so that we can have
// contiguous entries for rapid lookups, inserts and updates? we want to
// have fast access to elements in random positions in the data structure
//
#[derive(Debug)]
pub struct BlockRing {
    //
    // include Slice-VecDeque and have a slice that points to
    // contiguous entries for rapid lookups, inserts and updates?
    //
    pub ring: Vec<RingItem>,
    lc_pos: Option<usize>,
    pub empty: bool,
}

impl BlockRing {
    /// Create new `BlockRing`
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut init_ring: Vec<RingItem> = vec![];
        for _i in 0..RING_BUFFER_LENGTH {
            init_ring.push(RingItem::new());
        }

        BlockRing {
            ring: init_ring,
            lc_pos: None,
            empty: true,
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn add_block(&mut self, block: &Block) {
        let insert_pos = block.id % RING_BUFFER_LENGTH;
        trace!(
            "blockring.add_block : {:?} at pos = {:?}",
            hex::encode(block.hash),
            insert_pos
        );
        self.ring[(insert_pos as usize)].add_block(block.id, block.hash);
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn contains_block_hash_at_block_id(&self, block_id: u64, block_hash: SaitoHash) -> bool {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        self.ring[(insert_pos as usize)].contains_block_hash(block_hash)
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn get_latest_block_hash(&self) -> SaitoHash {
        match self.lc_pos {
            Some(lc_pos_block_ring) => match self.ring[lc_pos_block_ring].lc_pos {
                Some(lc_pos_block_item) => {
                    self.ring[lc_pos_block_ring].block_hashes[lc_pos_block_item]
                }
                None => [0; 32],
            },
            None => [0; 32],
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn get_latest_block_id(&self) -> u64 {
        match self.lc_pos {
            Some(lc_pos_block_ring) => match self.ring[lc_pos_block_ring].lc_pos {
                Some(lc_pos_block_item) => {
                    self.ring[lc_pos_block_ring].block_ids[lc_pos_block_item]
                }
                None => 0,
            },
            None => 0,
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn get_longest_chain_block_hash_by_block_id(&self, id: u64) -> SaitoHash {
        let insert_pos = (id % RING_BUFFER_LENGTH) as usize;
        match self.ring[insert_pos].lc_pos {
            Some(lc_pos) => self.ring[insert_pos].block_hashes[lc_pos],
            None => {
                trace!(
                    "get_longest_chain_block_hash_by_block_id : {:?} insert_pos = {:?} is not set",
                    id,
                    insert_pos
                );
                [0; 32]
            }
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn is_block_hash_at_block_id(&self, block_id: u64, block_hash: SaitoHash) -> bool {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        for i in 0..self.ring[(insert_pos as usize)].block_hashes.len() {
            if self.ring[(insert_pos as usize)].block_hashes[i] == block_hash {
                return true;
            }
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn delete_block(&mut self, block_id: u64, block_hash: SaitoHash) {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        self.ring[(insert_pos as usize)].delete_block(block_id, block_hash);
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn get_block_hashes_at_block_id(&mut self, block_id: u64) -> Vec<SaitoHash> {
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        let mut v: Vec<SaitoHash> = vec![];
        for i in 0..self.ring[(insert_pos as usize)].block_hashes.len() {
            if self.ring[(insert_pos as usize)].block_ids[i] == block_id {
                v.push(self.ring[(insert_pos as usize)].block_hashes[i]);
            }
        }
        v
    }

    #[tracing::instrument(level = "info", skip_all)]
    pub fn on_chain_reorganization(&mut self, block_id: u64, hash: SaitoHash, lc: bool) -> bool {
        trace!(
            "blockring.on_chain_reorg : block_id = {:?}, hash = {:?}",
            block_id,
            hex::encode(hash)
        );
        let insert_pos = block_id % RING_BUFFER_LENGTH;
        if !self.ring[(insert_pos as usize)].on_chain_reorganization(hash, lc) {
            return false;
        }
        if lc {
            self.lc_pos = Some(insert_pos as usize);
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
            if let Some(lc_pos) = self.lc_pos {
                if lc_pos == insert_pos as usize {
                    let previous_block_index;

                    if lc_pos > 0 {
                        previous_block_index = lc_pos - 1;
                    } else {
                        previous_block_index = RING_BUFFER_LENGTH as usize - 1;
                    }

                    // reset to lc_pos to unknown
                    self.lc_pos = None;

                    // but try to find it
                    // let previous_block_index_lc_pos = self.ring[previous_block_index as usize].lc_pos;
                    if let Some(previous_block_index_lc_pos) =
                        self.ring[previous_block_index as usize].lc_pos
                    {
                        if self.ring[previous_block_index].block_ids.len()
                            > previous_block_index_lc_pos
                        {
                            if self.ring[previous_block_index].block_ids
                                [previous_block_index_lc_pos]
                                == block_id - 1
                            {
                                self.lc_pos = Some(previous_block_index);
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
            if !self.ring[(i as usize)].block_hashes.is_empty() {
                trace!(
                    "Block {:?}: {:?}",
                    i,
                    self.get_longest_chain_block_hash_by_block_id(i)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::core::data::block::Block;
    use crate::core::data::blockchain::GENESIS_PERIOD;
    use crate::core::data::blockring::BlockRing;

    pub const RING_BUFFER_LENGTH: u64 = 2 * GENESIS_PERIOD;

    #[test]
    fn blockring_new_test() {
        let blockring = BlockRing::new();
        assert_eq!(blockring.ring.len() as u64, RING_BUFFER_LENGTH);
        assert_eq!(blockring.lc_pos, None);
    }

    #[test]
    fn blockring_add_block_test() {
        let mut blockring = BlockRing::new();
        let mut block = Block::new();
        block.id = 1;
        block.generate_hash();
        let block_hash = block.hash;
        let block_id = block.id;

        // everything is empty to start
        assert_eq!(blockring.is_empty(), true);
        assert_eq!(blockring.get_latest_block_hash(), [0; 32]);
        assert_eq!(blockring.get_latest_block_id(), 0);
        assert_eq!(
            blockring.get_longest_chain_block_hash_by_block_id(0),
            [0; 32]
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(block.id, block.hash),
            false
        );

        blockring.add_block(&block);
        blockring.on_chain_reorganization(block.id, block.hash, true);

        // assert_eq!(blockring.is_empty(), false);
        assert_eq!(blockring.get_latest_block_hash(), block_hash);
        assert_eq!(blockring.get_latest_block_id(), block_id);
        assert_eq!(
            blockring.get_longest_chain_block_hash_by_block_id(block_id),
            block_hash
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(block.id, block.hash),
            true
        );
    }

    #[test]
    fn blockring_delete_block_test() {
        let mut blockring = BlockRing::new();
        let mut block = Block::new();
        block.generate_hash();
        let block_hash = block.hash;
        let block_id = block.id;

        // everything is empty to start
        assert_eq!(blockring.is_empty(), true);
        assert_eq!(blockring.get_latest_block_hash(), [0; 32]);
        assert_eq!(blockring.get_latest_block_id(), 0);
        assert_eq!(
            blockring.get_longest_chain_block_hash_by_block_id(0),
            [0; 32]
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(block.id, block.hash),
            false
        );

        blockring.add_block(&block);
        blockring.on_chain_reorganization(block.id, block.hash, true);

        // assert_eq!(blockring.is_empty(), false);
        assert_eq!(blockring.get_latest_block_hash(), block_hash);
        assert_eq!(blockring.get_latest_block_id(), block_id);
        assert_eq!(
            blockring.get_longest_chain_block_hash_by_block_id(block_id),
            block_hash
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(block.id, block.hash),
            true
        );

        blockring.delete_block(block.id, block.hash);
        assert_eq!(
            blockring.contains_block_hash_at_block_id(block.id, block.hash),
            false
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    //
    // does reorg update blockring view of longest-chain
    //
    async fn blockring_manual_reorganization_test() {
        let mut block1 = Block::new();
        let mut block2 = Block::new();
        let mut block3 = Block::new();
        let mut block4 = Block::new();
        let mut block5 = Block::new();

        block1.id = 1;
        block2.id = 2;
        block3.id = 3;
        block4.id = 4;
        block5.id = 5;

        block1.generate();
        block2.generate();
        block3.generate();
        block4.generate();
        block5.generate();

        let mut blockring = BlockRing::new();

        blockring.add_block(&block1);
        blockring.add_block(&block2);
        blockring.add_block(&block3);
        blockring.add_block(&block4);
        blockring.add_block(&block5);

        // do we contain these block hashes?
        assert_eq!(
            blockring.contains_block_hash_at_block_id(1, block1.hash),
            true
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(2, block2.hash),
            true
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(3, block3.hash),
            true
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(4, block4.hash),
            true
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(5, block5.hash),
            true
        );
        assert_eq!(
            blockring.contains_block_hash_at_block_id(2, block4.hash),
            false
        );

        // reorganize longest chain
        blockring.on_chain_reorganization(1, block1.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 1);
        blockring.on_chain_reorganization(2, block2.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 2);
        blockring.on_chain_reorganization(3, block3.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 3);
        blockring.on_chain_reorganization(4, block4.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 4);
        blockring.on_chain_reorganization(5, block5.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 5);
        blockring.on_chain_reorganization(5, block5.hash, false);
        assert_eq!(blockring.get_latest_block_id(), 4);
        blockring.on_chain_reorganization(4, block4.hash, false);
        assert_eq!(blockring.get_latest_block_id(), 3);
        blockring.on_chain_reorganization(3, block3.hash, false);
        assert_eq!(blockring.get_latest_block_id(), 2);

        // reorg in the wrong block_id location, should not change
        blockring.on_chain_reorganization(532, block5.hash, false);
        assert_eq!(blockring.get_latest_block_id(), 2);

        // double reorg in correct and should be fine still
        blockring.on_chain_reorganization(2, block2.hash, true);
        blockring.on_chain_reorganization(2, block2.hash, true);
        assert_eq!(blockring.get_latest_block_id(), 2);
    }
}
