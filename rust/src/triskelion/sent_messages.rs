// Cross-process SendMessage tracker with adaptive routing.
//
// When Wine dispatches a blocking SendMessage across process boundaries, the
// sender thread parks until the receiver calls reply_message. The daemon
// holds the in-flight envelope here so it can wake the sender once the reply
// arrives.
//
// ADAPTIVE ROUTING (PANDEMONIUM-inspired observe -> classify -> decide):
//
// Cold start: all cross-process sends are TRACKED (sender blocks until
// reply_message). This is correct Wine semantics and won't break games.
//
// As we observe replies, we build per-msg_code profiles. Messages that
// consistently get zero-result replies (>= MIN_OBSERVATIONS at >= 90%
// zero-reply rate) are promoted to FAST PATH (immediate QS_SMRESULT,
// no blocking). This is safe because the sender never uses the reply value.
//
// Profiles persist across launches via intel.rs (.quark_cache).

use rustc_hash::FxHashMap;

// Confidence gating (from PANDEMONIUM procdb)
const MIN_OBSERVATIONS: u32 = 8;
const PROMOTE_THRESHOLD: f64 = 0.90;

pub struct PendingSentMessage {
    pub sender_tid: u32,
    pub receiver_tid: u32,
    pub msg_code: u32,
    pub win: u32,
    pub wparam: u64,
    pub lparam: u64,
    pub msg_type: i32,
}

#[derive(Clone, Debug)]
pub struct MsgProfile {
    pub fast_votes: u32,
    pub tracked_votes: u32,
    pub observations: u32,
    pub promoted: bool,
}

impl MsgProfile {
    fn new() -> Self {
        Self { fast_votes: 0, tracked_votes: 0, observations: 0, promoted: false }
    }

    fn confidence(&self) -> f64 {
        if self.observations < MIN_OBSERVATIONS { return 0.0; }
        self.fast_votes as f64 / self.observations as f64
    }

    fn check_promote(&mut self) {
        if !self.promoted && self.confidence() >= PROMOTE_THRESHOLD {
            self.promoted = true;
        }
    }

    // Harsh demote: if a previously-promoted message gets a nonzero reply,
    // revoke promotion and reset fast_votes. Same pattern as PANDEMONIUM's
    // procdb stability demotion.
    fn demote(&mut self) {
        if self.promoted {
            self.promoted = false;
            self.fast_votes = 0;
        }
    }
}

#[derive(Default)]
pub struct SentMessages {
    pending: FxHashMap<u32, Vec<PendingSentMessage>>,
    profiles: FxHashMap<u32, MsgProfile>,
}

impl SentMessages {
    pub fn new() -> Self {
        Self::default()
    }

    // DECISION: should this cross-process msg_code skip tracking?
    // Cold start (no profile or insufficient observations) = tracked (conservative).
    // Warm start = use learned promotion state.
    pub fn should_fast_path(&self, msg_code: u32) -> bool {
        self.profiles.get(&msg_code).map_or(false, |p| p.promoted)
    }

    // Track a new in-flight cross-process SendMessage (tracked path).
    pub fn track(&mut self, msg: PendingSentMessage) {
        self.pending.entry(msg.receiver_tid).or_default().push(msg);
    }

    // Receiver called reply_message -- pop envelope and return (sender_tid, msg_code)
    // so caller can feed the result value back to observe_reply.
    pub fn drain_one_with_code(&mut self, receiver_tid: u32) -> Option<(u32, u32)> {
        let stack = self.pending.get_mut(&receiver_tid)?;
        let msg = stack.pop()?;
        let sender_tid = msg.sender_tid;
        let msg_code = msg.msg_code;
        if stack.is_empty() {
            self.pending.remove(&receiver_tid);
        }
        Some((sender_tid, msg_code))
    }

    // OBSERVATION: receiver replied with a result value.
    // Zero result = message didn't carry meaningful data back (fast-path safe).
    // Nonzero result = sender depends on the reply (must track).
    pub fn observe_reply(&mut self, msg_code: u32, result: u64) {
        let p = self.profiles.entry(msg_code).or_insert_with(MsgProfile::new);
        p.observations += 1;
        if result == 0 {
            p.fast_votes += 1;
            p.check_promote();
        } else {
            p.tracked_votes += 1;
            p.demote();
        }
    }

    // Receiver thread disconnected -- return every sender that was waiting
    // on it so they can be woken, and observe the msg_codes.
    pub fn drain_all_for_receiver(&mut self, receiver_tid: u32) -> Vec<u32> {
        let msgs = match self.pending.remove(&receiver_tid) {
            Some(msgs) => msgs,
            None => return Vec::new(),
        };
        let mut sender_tids = Vec::with_capacity(msgs.len());
        for m in msgs {
            // Receiver disconnected without replying = fast-path safe
            let p = self.profiles.entry(m.msg_code).or_insert_with(MsgProfile::new);
            p.observations += 1;
            p.fast_votes += 1;
            p.check_promote();
            sender_tids.push(m.sender_tid);
        }
        sender_tids
    }

    // Most recent in-flight message for a receiver, used by get_message
    // to deliver the QS_SENDMESSAGE category before posted messages.
    pub fn peek(&self, receiver_tid: u32) -> Option<&PendingSentMessage> {
        self.pending.get(&receiver_tid)?.last()
    }

    // Snapshot profiles for persistence (called by intel.rs on shutdown).
    pub fn snapshot_profiles(&self) -> Vec<(u32, MsgProfile)> {
        self.profiles.iter().map(|(&k, v)| (k, v.clone())).collect()
    }

    // Seed profiles from persisted data (called by intel.rs on startup).
    pub fn load_profiles(&mut self, profiles: Vec<(u32, MsgProfile)>) {
        for (msg_code, profile) in profiles {
            self.profiles.insert(msg_code, profile);
        }
    }
}
