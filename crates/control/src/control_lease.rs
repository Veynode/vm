//! Who controls a VM's input: a person at a console or browser desktop, or
//! the agent (docs/computer-use.md "Control lease"). A person always wins:
//! while a viewer is open and its person gave input within the quiet period,
//! agent input is refused with `person_in_control`; it resumes on its own once
//! the person stops, or at once when the viewer closes. Person input is never
//! held back. The agent holds the lease from its input on and can give it
//! back (`release`), so a viewer shows nobody in control.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long after a person's last input the agent waits.
pub const QUIET: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    /// Open viewers, by kind ("console", "stream").
    viewers: BTreeMap<String, usize>,
    human_input: Option<Instant>,
    agent_input: Option<Instant>,
    released: bool,
}

/// A change worth an MCP notification. The agent's turn coming back once a
/// person goes quiet is a matter of time, not an event, so only a viewer
/// closing reports `AgentMayAct`.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    PersonTookControl,
    AgentMayAct,
}

#[derive(Default)]
pub struct Leases {
    vms: Mutex<BTreeMap<String, State>>,
}

impl Leases {
    pub fn viewer(&self, vm: &str, kind: &str, open: bool, now: Instant) -> Option<Change> {
        self.update(vm, now, |state| {
            let count = state.viewers.entry(kind.to_string()).or_default();
            if open {
                *count += 1;
            } else {
                *count = count.saturating_sub(1);
            }
            state.viewers.retain(|_, n| *n > 0);
        })
    }

    pub fn human_input(&self, vm: &str, now: Instant) -> Option<Change> {
        self.update(vm, now, |state| {
            state.human_input = Some(now);
            state.released = false;
        })
    }

    /// The agent gives the lease back until its next input.
    pub fn release(&self, vm: &str, now: Instant) {
        self.update(vm, now, |state| state.released = true);
    }

    /// Asks for the lease before agent input: `Err` is how long a person in
    /// control keeps it (`action_result::person_in_control` words the refusal).
    pub fn acquire(&self, vm: &str, now: Instant) -> Result<(), Duration> {
        let mut vms = self.vms.lock().unwrap();
        let state = vms.entry(vm.to_string()).or_default();
        if let Some(wait) = person_wait(state, now) {
            return Err(wait);
        }
        state.agent_input = Some(now);
        state.released = false;
        Ok(())
    }

    /// The lease as `vm_control_lease` reports it.
    pub fn status(&self, vm: &str, now: Instant) -> Value {
        let vms = self.vms.lock().unwrap();
        let empty = State::default();
        let state = vms.get(vm).unwrap_or(&empty);
        let wait = person_wait(state, now);
        let holder = if wait.is_some() {
            "person"
        } else if state.agent_input.is_some() && !state.released {
            "agent"
        } else {
            "none"
        };
        json!({
            "holder": holder,
            "person_in_control": wait.is_some(),
            "viewers": state.viewers.keys().collect::<Vec<_>>(),
            "agent_may_act_in_ms": wait.map_or(0, |w| w.as_millis() as u64),
            "quiet_ms": QUIET.as_millis() as u64,
        })
    }

    /// Whether a viewer is open: a person may be watching, so the agent keeps
    /// to background delivery.
    pub fn watched(&self, vm: &str) -> bool {
        self.vms
            .lock()
            .unwrap()
            .get(vm)
            .is_some_and(|s| !s.viewers.is_empty())
    }

    pub fn forget(&self, vm: &str) {
        self.vms.lock().unwrap().remove(vm);
    }

    fn update(&self, vm: &str, now: Instant, change: impl FnOnce(&mut State)) -> Option<Change> {
        let mut vms = self.vms.lock().unwrap();
        let state = vms.entry(vm.to_string()).or_default();
        let before = person_wait(state, now).is_some();
        change(state);
        match (before, person_wait(state, now).is_some()) {
            (false, true) => Some(Change::PersonTookControl),
            (true, false) => Some(Change::AgentMayAct),
            _ => None,
        }
    }
}

/// How long the agent must still wait, while a person is in control.
fn person_wait(state: &State, now: Instant) -> Option<Duration> {
    if state.viewers.is_empty() {
        return None;
    }
    let last = state.human_input?;
    QUIET
        .checked_sub(now.saturating_duration_since(last))
        .filter(|w| !w.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_at_an_open_viewer_pauses_the_agent_until_quiet() {
        let leases = Leases::default();
        let t0 = Instant::now();
        assert!(leases.acquire("vm", t0).is_ok());
        assert_eq!(leases.status("vm", t0)["holder"], "agent");
        assert_eq!(leases.viewer("vm", "console", true, t0), None);
        assert_eq!(
            leases.human_input("vm", t0),
            Some(Change::PersonTookControl)
        );
        let wait = leases
            .acquire("vm", t0 + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(wait, Duration::from_secs(4));
        assert_eq!(
            leases.status("vm", t0 + Duration::from_secs(1))["agent_may_act_in_ms"],
            4000
        );
        assert!(leases.acquire("vm", t0 + QUIET).is_ok());
    }

    #[test]
    fn closing_the_viewer_hands_control_back_at_once() {
        let leases = Leases::default();
        let t0 = Instant::now();
        leases.viewer("vm", "stream", true, t0);
        leases.viewer("vm", "console", true, t0);
        leases.human_input("vm", t0);
        assert_eq!(leases.viewer("vm", "stream", false, t0), None);
        assert!(leases.acquire("vm", t0).is_err());
        assert_eq!(
            leases.viewer("vm", "console", false, t0),
            Some(Change::AgentMayAct)
        );
        assert!(leases.acquire("vm", t0).is_ok());
    }

    #[test]
    fn input_without_a_viewer_and_other_vms_never_block() {
        let leases = Leases::default();
        let t0 = Instant::now();
        leases.human_input("vm", t0);
        assert!(leases.acquire("vm", t0).is_ok());
        leases.viewer("vm", "console", true, t0);
        leases.human_input("vm", t0);
        assert!(leases.acquire("other", t0).is_ok());
        leases.release("other", t0);
        assert_eq!(leases.status("other", t0)["holder"], "none");
        assert_eq!(leases.status("vm", t0)["holder"], "person");
    }
}
