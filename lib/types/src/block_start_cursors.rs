use serde::{Deserialize, Serialize};

use crate::{InteropRootsLogIndex, L1TxSerialId};

/// Tracks where each L1 data source left off at the start of a block.
///
/// Each field represents a cursor into an L1 event stream. At block start,
/// these values tell us from what point in L1 history to look for the next
/// event of each type. If a block doesn't contain events of a given type,
/// the cursor carries forward unchanged from the previous block.
///
/// Serde field names match the legacy flat field names on `ReplayRecord`
/// so that `#[serde(flatten)]` produces a backwards-compatible representation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockStartCursors {
    /// L1 transaction serial id (0-based) expected at the beginning of this block.
    /// If `l1_transactions` is non-empty, equals to the first tx id in this block;
    /// otherwise, equals to the previous block's value.
    #[serde(rename = "starting_l1_priority_id")]
    pub l1_priority_id: L1TxSerialId,
    /// Event index (block number and index in block) of the interop root tx executed
    /// first in the block. If there is no interop root tx in the block, equals to the
    /// previous block's value.
    #[serde(rename = "starting_interop_event_index")]
    pub interop_event_index: InteropRootsLogIndex,
    /// Migration number at the beginning of the block. If there is no migration event
    /// in the block, equals to the previous block's value.
    #[serde(rename = "starting_migration_number")]
    pub migration_number: u64,
    /// Interop fee update number at the beginning of the block. If there is no interop
    /// fee update in the block, equals to the previous block's value.
    #[serde(rename = "starting_interop_fee_number")]
    pub interop_fee_number: u64,
}
