//! Client receive model — fragment reassembly, decode gating, and IDR-request
//! logic (PROTOCOL.md §6 rules 1–3 + §3.6).
//!
//! The [`Reassembler`] is deliberately driven by `(t_ms, Fragment)` tuples — the
//! exact shape of the `reassembly` conformance traces — with no I/O and no
//! notion of wall-clock time of its own, so it is fully deterministic and
//! replayable. `t_ms` is the caller-supplied arrival time in milliseconds.
//!
//! Counter semantics (as fixed by the vectors):
//! * `dropped_incomplete` — rule-2 window evictions **and** rule-1 cleanup of
//!   frames left incomplete once a newer frame completes;
//! * `discarded_gap` — completed non-keyframes that fail decode gating (§6.3);
//! * `stale_fragments` — fragments at/below `newest_complete` (rule 1) or older
//!   than everything currently in the 2-frame window.

use std::collections::{BTreeMap, HashSet};

/// The subset of a video datagram header (§4) that drives reassembly, plus the
/// arrival time supplied to [`Reassembler::push`].
#[derive(Clone, Copy, Debug)]
pub struct Fragment {
    /// Per-stream frame counter.
    pub frame_seq: u32,
    /// 0-based fragment index.
    pub frag_index: u16,
    /// Total fragments for this frame.
    pub frag_count: u16,
    /// Whether this frame is a keyframe (IDR).
    pub keyframe: bool,
}

/// An event emitted by the reassembler, in occurrence order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A completed frame handed to the decoder.
    Deliver {
        /// Arrival time of the fragment that completed the frame.
        t_ms: i64,
        /// The delivered frame's sequence number.
        frame_seq: u32,
        /// Whether it was a keyframe.
        keyframe: bool,
    },
    /// An IDR (keyframe) request to be sent on the control stream (§3.6).
    IdrRequest {
        /// Time the request was raised.
        t_ms: i64,
        /// `last_good_frame_seq`: newest fully decoded frame (0 if none).
        last_good: u32,
    },
}

/// Running counters over a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Rule-2 evictions + rule-1 cleanup of incomplete frames.
    pub dropped_incomplete: u64,
    /// Completed frames discarded by decode gating (§6.3).
    pub discarded_gap: u64,
    /// Fragments dropped as stale (rule 1 / below the window).
    pub stale_fragments: u64,
}

struct Incomplete {
    need: u16,
    have: HashSet<u16>,
    keyframe: bool,
}

/// Reassembly + decode-gating + IDR-request state machine (§6 + §3.6).
pub struct Reassembler {
    /// Highest `frame_seq` fully reassembled (−1 before any).
    newest_complete: i64,
    /// Newest `frame_seq` delivered to the decoder (`None` before any).
    last_decoded: Option<i64>,
    /// In-flight incomplete frames, keyed by `frame_seq` (min key = oldest).
    incomplete: BTreeMap<u32, Incomplete>,
    events: Vec<Event>,
    counters: Counters,
    idr_outstanding: bool,
    idr_last_t: Option<i64>,
    /// `last_decoded` captured at the moment the outstanding IDR was requested.
    idr_last_good_at_request: Option<i64>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    /// A fresh reassembler with no history.
    pub fn new() -> Self {
        Self {
            newest_complete: -1,
            last_decoded: None,
            incomplete: BTreeMap::new(),
            events: Vec::new(),
            counters: Counters::default(),
            idr_outstanding: false,
            idr_last_t: None,
            idr_last_good_at_request: None,
        }
    }

    /// All events emitted so far, in occurrence order.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The running counters.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Feed one fragment that arrived at time `t_ms`. Emits any resulting
    /// deliver / idr_request events into [`Self::events`].
    pub fn push(&mut self, t_ms: i64, frag: Fragment) {
        let seq = frag.frame_seq;

        // Rule 1: fragments at/below the newest completed frame are stale.
        if i64::from(seq) <= self.newest_complete {
            self.counters.stale_fragments += 1;
            return;
        }

        if !self.incomplete.contains_key(&seq) {
            // Rule 2: hold at most 2 incomplete frames.
            if self.incomplete.len() >= 2 {
                let oldest = *self.incomplete.keys().next().expect("non-empty");
                if seq > oldest {
                    self.incomplete.remove(&oldest);
                    self.counters.dropped_incomplete += 1;
                } else {
                    // Older than everything in the window: treat as a stale drop.
                    self.counters.stale_fragments += 1;
                    return;
                }
            }
            self.incomplete.insert(
                seq,
                Incomplete {
                    need: frag.frag_count,
                    have: HashSet::new(),
                    keyframe: frag.keyframe,
                },
            );
        }

        let entry = self.incomplete.get_mut(&seq).expect("just inserted");
        entry.have.insert(frag.frag_index); // duplicates are idempotent
        if entry.have.len() != usize::from(entry.need) {
            return;
        }

        // Frame complete.
        let keyframe = entry.keyframe;
        self.incomplete.remove(&seq);
        self.newest_complete = self.newest_complete.max(i64::from(seq));

        // Rule-1 cleanup: any still-incomplete frame now at/below newest dies.
        let stale: Vec<u32> = self
            .incomplete
            .keys()
            .copied()
            .filter(|s| i64::from(*s) <= self.newest_complete)
            .collect();
        for s in stale {
            self.incomplete.remove(&s);
            self.counters.dropped_incomplete += 1;
        }

        // Rule 3: decode gating.
        if keyframe {
            self.deliver(t_ms, seq, true);
        } else if self.last_decoded == Some(i64::from(seq) - 1) {
            self.deliver(t_ms, seq, false);
        } else {
            self.counters.discarded_gap += 1;
            self.maybe_idr(t_ms);
        }
    }

    fn deliver(&mut self, t_ms: i64, seq: u32, keyframe: bool) {
        self.events.push(Event::Deliver {
            t_ms,
            frame_seq: seq,
            keyframe,
        });
        self.last_decoded = Some(i64::from(seq));
        // An outstanding IDR clears once we deliver a keyframe newer than the
        // last-good frame recorded when it was requested (§3.6).
        if keyframe && self.idr_outstanding {
            let clears = match self.idr_last_good_at_request {
                None => true,
                Some(g) => i64::from(seq) > g,
            };
            if clears {
                self.idr_outstanding = false;
            }
        }
    }

    fn maybe_idr(&mut self, t_ms: i64) {
        if self.idr_outstanding {
            return;
        }
        // Rate limit: at most one IDR request per 250 ms (§3.6).
        if let Some(last) = self.idr_last_t {
            if t_ms - last < 250 {
                return;
            }
        }
        let last_good = self.last_decoded.unwrap_or(0);
        self.events.push(Event::IdrRequest {
            t_ms,
            last_good: last_good as u32,
        });
        self.idr_outstanding = true;
        self.idr_last_t = Some(t_ms);
        self.idr_last_good_at_request = self.last_decoded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(frame_seq: u32, frag_index: u16, frag_count: u16, keyframe: bool) -> Fragment {
        Fragment {
            frame_seq,
            frag_index,
            frag_count,
            keyframe,
        }
    }

    #[test]
    fn happy_path_delivers_in_order() {
        let mut r = Reassembler::new();
        r.push(0, frag(0, 0, 2, true));
        r.push(1, frag(0, 1, 2, true));
        r.push(14, frag(1, 0, 1, false));
        r.push(28, frag(2, 0, 1, false));
        assert_eq!(
            r.events(),
            &[
                Event::Deliver { t_ms: 1, frame_seq: 0, keyframe: true },
                Event::Deliver { t_ms: 14, frame_seq: 1, keyframe: false },
                Event::Deliver { t_ms: 28, frame_seq: 2, keyframe: false },
            ]
        );
        assert_eq!(r.counters(), &Counters::default());
    }

    #[test]
    fn duplicate_fragment_is_idempotent() {
        let mut r = Reassembler::new();
        r.push(0, frag(0, 0, 1, true));
        // frame 2 arrives twice (dup), then frame 1 fills the gap late.
        r.push(5, frag(2, 0, 1, false));
        r.push(6, frag(2, 0, 1, false));
        r.push(7, frag(1, 0, 1, false));
        assert_eq!(r.counters().stale_fragments, 2); // dup + late frame 1
        assert_eq!(r.counters().discarded_gap, 1); // frame 2 had a gap
    }

    #[test]
    fn gap_triggers_single_idr_then_keyframe_clears_it() {
        let mut r = Reassembler::new();
        r.push(0, frag(0, 0, 1, true));
        r.push(14, frag(2, 0, 1, false)); // gap -> discard + idr
        r.push(200, frag(3, 0, 1, false)); // still outstanding -> no idr
        r.push(900, frag(6, 0, 1, true)); // keyframe -> clears outstanding
        r.push(914, frag(7, 0, 1, false)); // now in sequence -> delivered
        let idrs = r
            .events()
            .iter()
            .filter(|e| matches!(e, Event::IdrRequest { .. }))
            .count();
        assert_eq!(idrs, 1);
        assert!(matches!(
            r.events().last(),
            Some(Event::Deliver { frame_seq: 7, .. })
        ));
    }

    #[test]
    fn window_holds_at_most_two_incomplete() {
        let mut r = Reassembler::new();
        r.push(0, frag(0, 0, 1, true));
        r.push(10, frag(1, 0, 2, false)); // incomplete
        r.push(11, frag(2, 0, 2, false)); // incomplete
        r.push(12, frag(3, 0, 2, false)); // evicts frame 1
        assert_eq!(r.counters().dropped_incomplete, 1);
    }
}
