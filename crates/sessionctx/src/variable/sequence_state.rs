//! go-file: pkg/sessionctx/variable/sequence_state.go

use std::collections::HashMap;

use parking_lot::Mutex;

/// SequenceState cache all sequence's latest value accessed by lastval() builtins. It's a session scoped
/// variable, and all public methods of SequenceState are currently-safe.
#[derive(Debug, Default)]
pub struct SequenceState {
    /// latestValueMap caches the last value obtained by nextval() for each sequence id.
    latest_value_map: Mutex<HashMap<i64, i64>>,
}

impl SequenceState {
    /// NewSequenceState creates a SequenceState.
    // go: NewSequenceState L1
    pub fn new() -> SequenceState {
        SequenceState {
            latest_value_map: Mutex::new(HashMap::new()),
        }
    }

    /// UpdateState will update the last value of specified sequenceID in a session.
    // go: SequenceState.UpdateState L1
    pub fn update_state(&self, sequence_id: i64, value: i64) {
        self.latest_value_map.lock().insert(sequence_id, value);
    }

    /// GetLastValue will return last value of the specified sequenceID, bool(true) indicates
    /// the sequenceID is not in the cache map and NULL will be returned.
    // go: SequenceState.GetLastValue L1
    pub fn get_last_value(&self, sequence_id: i64) -> (i64, bool, Result<(), tidb_base::Error>) {
        let m = self.latest_value_map.lock();
        if !m.is_empty()
            && let Some(&last) = m.get(&sequence_id)
        {
            return (last, false, Ok(()));
        }
        (0, true, Ok(()))
    }

    /// GetAllStates returns a copied latestValueMap.
    // go: SequenceState.GetAllStates L1
    pub fn get_all_states(&self) -> HashMap<i64, i64> {
        self.latest_value_map.lock().clone()
    }

    /// SetAllStates sets latestValueMap as a whole.
    // go: SequenceState.SetAllStates L1
    pub fn set_all_states(&self, latest_value_map: &HashMap<i64, i64>) {
        let mut m = self.latest_value_map.lock();
        for (&seq_id, &latest_value) in latest_value_map {
            m.insert(seq_id, latest_value);
        }
    }
}
