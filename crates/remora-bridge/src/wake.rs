//! Pure wake policy for the push pipeline (#233): episode de-duplication and
//! fan-out targeting.
//!
//! This module is deliberately **I/O-free and synchronous** — no tokio, no
//! sockets, no roster locks held. It answers two questions the bridge's async
//! shell asks:
//!
//! - [`WakeEpisodes`]: has a `(project, session)` *just* entered an `Awaiting`
//!   episode? At most one wake fires per episode, so a session that stays
//!   `Awaiting` (repaint, re-notify) does not re-wake every paired device.
//! - [`wake_targets`]: given the roster and the set of devices that currently
//!   hold a live Noise session, which paired devices should be push-woken —
//!   the ones with a registered endpoint that are *not* already connected.
//!
//! Keeping this logic pure makes the interesting behaviour (episode edges,
//! cap eviction, target filtering + deterministic order) testable without any
//! of the connection machinery in `bridge.rs`.

use std::collections::{HashMap, HashSet, VecDeque};

use remora_protocol::{DeviceId, ProjectId, SessionId, SessionStatus};

use crate::identity::Roster;

/// Identifies one session for episode tracking: its project and session ids.
pub type EpisodeKey = (ProjectId, SessionId);

/// Tracks which sessions are inside an `Awaiting` episode so the bridge wakes
/// paired devices **at most once per episode**.
///
/// An episode *opens* on the transition into `Awaiting` (the first `Awaiting`
/// note for a key that is not already open) and *closes* on any non-`Awaiting`
/// status ([`SessionStatus::Working`]/`Idle`/`Unknown`) or an explicit
/// [`forget`](Self::forget). Only the opening edge returns `true` from
/// [`note`](Self::note), which is the bridge's signal to fan a wake out.
///
/// The open set is bounded: it holds at most `cap` keys in insertion order and
/// evicts the oldest when a new episode would overflow, so an unbounded stream
/// of distinct sessions cannot grow it without bound. Evicting a key simply
/// forgets its episode — its next `Awaiting` will open (and wake) afresh.
pub struct WakeEpisodes {
    /// Maximum number of concurrently-open episodes retained.
    cap: usize,
    /// The currently-open episode keys (membership test).
    open: HashMap<EpisodeKey, ()>,
    /// The open keys in insertion order, for oldest-first cap eviction. Kept in
    /// lock-step with `open`: every key here is in `open` and vice-versa.
    order: VecDeque<EpisodeKey>,
}

impl WakeEpisodes {
    /// Creates a tracker retaining at most `cap` concurrently-open episodes.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            open: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Records a status transition for `key`, returning `true` **only** when it
    /// opens a fresh `Awaiting` episode (the bridge's cue to fan a wake out).
    ///
    /// - `Awaiting` on a key not already open → opens the episode, returns
    ///   `true` (evicting the oldest episode first if at `cap`).
    /// - `Awaiting` on a key already open → still inside the episode, returns
    ///   `false` (no duplicate wake).
    /// - any non-`Awaiting` status → closes the episode if open, returns
    ///   `false`.
    pub fn note(&mut self, key: EpisodeKey, status: &SessionStatus) -> bool {
        match status {
            SessionStatus::Awaiting => {
                if self.open.contains_key(&key) {
                    return false; // already inside this Awaiting episode
                }
                self.open.insert(key.clone(), ());
                self.order.push_back(key);
                // Cap the open set: drop the oldest episode if we just overflowed.
                if self.open.len() > self.cap {
                    if let Some(oldest) = self.order.pop_front() {
                        self.open.remove(&oldest);
                    }
                }
                true
            }
            // Working / Idle / Unknown (and any future variant): the session is
            // no longer awaiting input, so close any open episode for it.
            _ => {
                self.forget(&key);
                false
            }
        }
    }

    /// Forgets `key`'s episode — used when a session channel is torn down, so a
    /// later respawn's `Awaiting` opens (and wakes) as a fresh episode. A no-op
    /// for a key with no open episode.
    pub fn forget(&mut self, key: &EpisodeKey) {
        if self.open.remove(key).is_some() {
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
        }
    }
}

/// Selects the devices to push-wake: every roster entry that has a registered
/// push endpoint and is **not** currently holding a live Noise session (its id
/// is absent from `live`). A device that is already connected sees the session
/// change directly, so it is never woken.
///
/// The result is sorted by device id, so a caller's fan-out order — and tests —
/// are deterministic.
pub fn wake_targets(roster: &Roster, live: &HashSet<DeviceId>) -> Vec<DeviceId> {
    let mut targets: Vec<DeviceId> = roster
        .entries
        .iter()
        .filter(|entry| entry.push.is_some() && !live.contains(&entry.device_id))
        .map(|entry| entry.device_id)
        .collect();
    // Deterministic order (by the opaque 32-byte id) so fan-out is stable.
    targets.sort_by_key(|id| id.0);
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::RosterEntry;
    use remora_protocol::PushRegistration;

    fn key(project: &str, session: &str) -> EpisodeKey {
        (
            ProjectId::new(project).expect("valid project id"),
            SessionId::new(session).expect("valid session id"),
        )
    }

    #[test]
    fn awaiting_opens_episode_once() {
        // The opening edge wakes; staying Awaiting does not re-wake; a
        // non-Awaiting status closes the episode; the next Awaiting opens again.
        let mut ep = WakeEpisodes::new(8);
        let k = key("proj", "sess");

        assert!(
            ep.note(k.clone(), &SessionStatus::Awaiting),
            "first Awaiting opens"
        );
        assert!(
            !ep.note(k.clone(), &SessionStatus::Awaiting),
            "still inside the episode: no duplicate wake"
        );
        assert!(
            !ep.note(k.clone(), &SessionStatus::Working),
            "a non-Awaiting status closes and never wakes"
        );
        assert!(
            ep.note(k.clone(), &SessionStatus::Awaiting),
            "a fresh Awaiting after closing opens a new episode"
        );
    }

    #[test]
    fn idle_and_unknown_also_close_the_episode() {
        // Any non-Awaiting status (not just Working) closes the episode.
        for closing in [SessionStatus::Idle, SessionStatus::Unknown] {
            let mut ep = WakeEpisodes::new(8);
            let k = key("proj", "sess");
            assert!(ep.note(k.clone(), &SessionStatus::Awaiting));
            assert!(!ep.note(k.clone(), &closing), "{closing:?} closes");
            assert!(
                ep.note(k.clone(), &SessionStatus::Awaiting),
                "reopens after {closing:?}"
            );
        }
    }

    #[test]
    fn episodes_capped() {
        // Cap 2: opening a third distinct episode evicts the oldest, so the
        // oldest key's next Awaiting opens a fresh episode (returns true).
        let mut ep = WakeEpisodes::new(2);
        let a = key("proj", "a");
        let b = key("proj", "b");
        let c = key("proj", "c");

        assert!(ep.note(a.clone(), &SessionStatus::Awaiting));
        assert!(ep.note(b.clone(), &SessionStatus::Awaiting));
        assert!(ep.note(c.clone(), &SessionStatus::Awaiting)); // evicts `a` (oldest)

        // `b` and `c` are still open, so they do not re-wake (these checks do
        // not perturb the set — an already-open key is a no-op).
        assert!(!ep.note(b, &SessionStatus::Awaiting), "b still open");
        assert!(!ep.note(c, &SessionStatus::Awaiting), "c still open");

        // `a` was evicted (oldest), so its next Awaiting opens a fresh episode.
        assert!(
            ep.note(a, &SessionStatus::Awaiting),
            "the evicted oldest episode reopens"
        );
    }

    #[test]
    fn forget_clears() {
        // Forgetting an open episode lets the next Awaiting reopen it.
        let mut ep = WakeEpisodes::new(8);
        let k = key("proj", "sess");
        assert!(ep.note(k.clone(), &SessionStatus::Awaiting));
        ep.forget(&k);
        assert!(
            ep.note(k, &SessionStatus::Awaiting),
            "a forgotten episode reopens on the next Awaiting"
        );
    }

    #[test]
    fn forget_unknown_key_is_a_no_op() {
        // Forgetting a key with no open episode neither panics nor disturbs
        // other episodes.
        let mut ep = WakeEpisodes::new(8);
        let open = key("proj", "open");
        let absent = key("proj", "absent");
        assert!(ep.note(open.clone(), &SessionStatus::Awaiting));
        ep.forget(&absent);
        assert!(
            !ep.note(open, &SessionStatus::Awaiting),
            "the untouched episode is still open"
        );
    }

    fn entry_with_push(id: u8, push: Option<PushRegistration>) -> RosterEntry {
        RosterEntry {
            device_id: DeviceId([id; 32]),
            static_pubkey: vec![id; 32],
            psk: [0; 32],
            relay_token: "tok".to_string(),
            name: "d".to_string(),
            enrolled_at: None,
            last_connected_at: None,
            push,
        }
    }

    fn some_push() -> Option<PushRegistration> {
        Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/t".to_string(),
        })
    }

    #[test]
    fn wake_targets_filters() {
        // A: push + offline → woken. B: push + live → skipped (connected sees
        // it directly). C: no push → skipped (nowhere to wake).
        let a = DeviceId([0xa0; 32]);
        let b = DeviceId([0xb0; 32]);
        let roster = Roster {
            entries: vec![
                entry_with_push(0xa0, some_push()),
                entry_with_push(0xb0, some_push()),
                entry_with_push(0xc0, None),
            ],
        };
        let live: HashSet<DeviceId> = [b].into_iter().collect();

        assert_eq!(
            wake_targets(&roster, &live),
            vec![a],
            "only the push-registered, offline device is a wake target"
        );
    }

    #[test]
    fn wake_targets_sorted_deterministically() {
        // Targets come back sorted by device id regardless of roster order.
        let roster = Roster {
            entries: vec![
                entry_with_push(0xc0, some_push()),
                entry_with_push(0xa0, some_push()),
                entry_with_push(0xb0, some_push()),
            ],
        };
        let live = HashSet::new();
        assert_eq!(
            wake_targets(&roster, &live),
            vec![
                DeviceId([0xa0; 32]),
                DeviceId([0xb0; 32]),
                DeviceId([0xc0; 32]),
            ],
            "fan-out order is deterministic (sorted by id)"
        );
    }
}
