//! Run-state observation by QMP events with a slow safety poll, against a scripted QMP peer on a real Unix socket
//! pair. Time for the poll schedule is passed in, so the 1 s, 10 s and 30 s bounds are asserted without waiting.
use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering as Order},
    Arc, Mutex,
};

/// The peer answers every `query-status` with `status` (a JSON `null` status answers a QMP error). Whatever is in
/// `before_reply` is written, byte for byte, after a request has been read and before its reply.
struct Peer {
    stream: UnixStream,
    queries: Arc<AtomicUsize>,
    status: Arc<Mutex<Value>>,
    before_reply: Arc<Mutex<Vec<u8>>>,
}
impl Peer {
    fn queries(&self) -> usize {
        self.queries.load(Order::SeqCst)
    }
    fn set(&self, running: bool, status: &str) {
        *self.status.lock().unwrap() = json!({"running":running,"status":status});
    }
    /// Unsolicited bytes, as QEMU delivers an event between commands. Readable by the worker when this returns.
    fn deliver(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }
    fn event(&mut self, name: &str) {
        self.deliver(&record(&event(name)));
    }
}
fn event(name: &str) -> Value {
    json!({"event":name,"data":{},"timestamp":{"seconds":1,"microseconds":2}})
}
fn record(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}
fn peer() -> (Qmp, Peer) {
    let (client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let qmp = Qmp {
        reader: BufReader::new(client.try_clone().unwrap()),
        stream: client,
        next: 1,
        poisoned: false,
        reply_prefix: Vec::new(),
        run_state_event: false,
        shutdown_event: false,
        guest_reset: false,
    };
    let peer = Peer {
        stream: server.try_clone().unwrap(),
        queries: Arc::new(AtomicUsize::new(0)),
        status: Arc::new(Mutex::new(json!({"running":true,"status":"running"}))),
        before_reply: Arc::new(Mutex::new(Vec::new())),
    };
    let (queries, status, before_reply) = (
        peer.queries.clone(),
        peer.status.clone(),
        peer.before_reply.clone(),
    );
    thread::spawn(move || {
        let mut reader = BufReader::new(server.try_clone().unwrap());
        let mut writer = server;
        while let Ok(Some(line)) = read_frame(&mut reader) {
            let request: Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(request["execute"], "query-status");
            queries.fetch_add(1, Order::SeqCst);
            let interleaved = std::mem::take(&mut *before_reply.lock().unwrap());
            writer.write_all(&interleaved).unwrap();
            let status = status.lock().unwrap().clone();
            let reply = if status.is_null() {
                json!({"id":request["id"],"error":{"class":"GenericError","desc":"scripted"}})
            } else {
                json!({"id":request["id"],"return":status})
            };
            writer.write_all(&record(&reply)).unwrap();
        }
    });
    (qmp, peer)
}
struct Worker {
    qmp: Qmp,
    poll: StatusPoll,
    state: Value,
    published: Vec<Value>,
    start: Instant,
}
impl Worker {
    /// One loop pass at `start + offset`.
    fn pass_with(&mut self, offset: Duration, may_query: bool) {
        let published = &mut self.published;
        observe_status(
            &mut self.qmp,
            &mut self.poll,
            &mut self.state,
            self.start + offset,
            may_query,
            |value| {
                published.push(value);
                Ok(())
            },
        )
        .unwrap();
    }
    fn pass(&mut self, offset: Duration) {
        self.pass_with(offset, true)
    }
}
const MS: Duration = Duration::from_millis(1);
const S: Duration = Duration::from_secs(1);
/// A worker as it is after startup, taken past its active window: 11 queries (0 s to 10 s, one per second), nothing
/// published, and the next query due at 40 s.
const SETTLED: Duration = Duration::from_secs(10);
const SETTLED_QUERIES: usize = 11;
fn settled() -> (Worker, Peer) {
    let (qmp, peer) = peer();
    let start = Instant::now();
    let mut worker = Worker {
        qmp,
        poll: StatusPoll::new(start),
        state: json!({"event":"state","state":"running","qmp_status":"running","vm_id":"test"}),
        published: Vec::new(),
        start,
    };
    // Immediate query on the first pass after connect and `cont`, then the original 1 s cadence.
    worker.pass(Duration::ZERO);
    assert_eq!(peer.queries(), 1);
    for second in 1..=10 {
        worker.pass(S * second - MS);
        assert_eq!(peer.queries(), second as usize);
        worker.pass(S * second);
        assert_eq!(peer.queries(), second as usize + 1);
    }
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert!(worker.published.is_empty());
    (worker, peer)
}

#[test]
fn bounds_are_the_stated_ones() {
    assert_eq!(STATUS_ACTIVE_INTERVAL, Duration::from_secs(1));
    assert_eq!(STATUS_ACTIVE_WINDOW, Duration::from_secs(10));
    assert_eq!(STATUS_SAFETY_INTERVAL, Duration::from_secs(30));
    // An event is acted on by the next loop pass, which is sooner than the 1 s poll it replaces.
    assert!(request_wait(false) < STATUS_ACTIVE_INTERVAL);
    assert!(RUN_STATE_EVENTS.contains(&"STOP") && RUN_STATE_EVENTS.contains(&"RESUME"));
    assert!(!RUN_STATE_EVENTS.contains(&"SHUTDOWN"));
}

#[test]
fn event_between_command_and_reply_is_not_the_reply_and_is_not_lost() {
    let (mut qmp, peer) = peer();
    peer.set(false, "paused");
    // An unknown event, a run-state event whose data imitates a reply, and a late reply to some earlier command, all
    // delivered after the command was read and before its reply.
    let mut interleaved = record(&event("RTC_CHANGE"));
    interleaved.extend(record(&json!({
        "event":"STOP","data":{"return":{"running":true,"status":"running"}},"timestamp":{"seconds":1,"microseconds":2}
    })));
    interleaved.extend(record(
        &json!({"id":999,"return":{"running":true,"status":"running"}}),
    ));
    *peer.before_reply.lock().unwrap() = interleaved;
    assert_eq!(
        qmp.call("query-status").unwrap(),
        json!({"running":false,"status":"paused"})
    );
    assert!(qmp.reply_prefix.is_empty());
    assert!(!qmp.poisoned);
    // The STOP read on the reply path is still reported to the status poll, exactly once.
    assert_eq!(qmp.drain_events(), Drained::RunState);
    assert_eq!(qmp.drain_events(), Drained::Nothing);
    // Only unknown events around the next command: nothing is reported.
    *peer.before_reply.lock().unwrap() = record(&event("BALLOON_CHANGE"));
    assert_eq!(
        qmp.call("query-status").unwrap(),
        json!({"running":false,"status":"paused"})
    );
    assert_eq!(qmp.drain_events(), Drained::Nothing);
    assert_eq!(peer.queries(), 2);
}

#[test]
fn unknown_events_and_non_events_are_ignored_and_cause_no_query() {
    let (mut worker, mut peer) = settled();
    for name in [
        "RTC_CHANGE",
        "BALLOON_CHANGE",
        "SPICE_CONNECTED",
        "NIC_RX_FILTER_CHANGED",
        "POWERDOWN",
        "stop",
        "",
    ] {
        peer.event(name);
    }
    // Not events: a late reply (it carries an id, whatever else it holds), and records with no event name.
    peer.deliver(&record(&json!({"id":77,"event":"STOP","return":{}})));
    peer.deliver(&record(&json!({"return":{}})));
    peer.deliver(&record(&json!({"event":7})));
    peer.deliver(&record(&json!(["STOP"])));
    worker.pass(SETTLED + MS);
    worker.pass(SETTLED + S * 29);
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert!(worker.published.is_empty());
    assert!(worker.qmp.reply_prefix.is_empty());
    // The connection is intact: the next command gets its own reply.
    assert_eq!(
        worker.qmp.call("query-status").unwrap()["status"],
        "running"
    );
}

#[test]
fn run_state_event_is_acted_on_by_the_next_pass_and_restarts_the_one_second_cadence() {
    for (name, running, status) in [
        ("STOP", false, "paused"),
        ("GUEST_PANICKED", false, "guest-panicked"),
        ("BLOCK_IO_ERROR", false, "io-error"),
        ("WATCHDOG", false, "watchdog"),
        ("SUSPEND", false, "suspended"),
        ("RESET", false, "prelaunch"),
    ] {
        let (mut worker, mut peer) = settled();
        peer.set(running, status);
        // Without the event nothing is asked: the safety poll is not due for another 30 s.
        worker.pass(SETTLED + MS);
        assert_eq!(peer.queries(), SETTLED_QUERIES, "{name}");
        peer.event(name);
        worker.pass(SETTLED + MS * 2);
        assert_eq!(peer.queries(), SETTLED_QUERIES + 1, "{name}");
        assert_eq!(worker.published.len(), 1, "{name}");
        assert_eq!(worker.published[0]["state"], "paused", "{name}");
        assert_eq!(worker.published[0]["qmp_status"], status, "{name}");
        assert_eq!(worker.published[0]["vm_id"], "test", "{name}");
        assert_eq!(worker.state, worker.published[0], "{name}");
        // Back to running, announced by its own event (WAKEUP is what QEMU sends when leaving suspend).
        peer.set(true, "running");
        peer.event(if name == "SUSPEND" {
            "WAKEUP"
        } else {
            "RESUME"
        });
        worker.pass(SETTLED + MS * 3);
        assert_eq!(worker.published.len(), 2, "{name}");
        assert_eq!(worker.published[1]["state"], "running", "{name}");
        assert_eq!(worker.published[1]["qmp_status"], "running", "{name}");
        // Active window: 1 s cadence for 10 s after the last change, then the safety interval again.
        let changed = SETTLED + MS * 3;
        let before = peer.queries();
        for second in 1..=10 {
            worker.pass(changed + S * second - MS);
            worker.pass(changed + S * second);
            assert_eq!(peer.queries(), before + second as usize, "{name}");
        }
        worker.pass(changed + S * 39);
        assert_eq!(peer.queries(), before + 10, "{name}");
        worker.pass(changed + S * 40);
        assert_eq!(peer.queries(), before + 11, "{name}");
        assert_eq!(worker.published.len(), 2, "{name}");
    }
}

#[test]
fn safety_poll_catches_a_transition_that_sent_no_event() {
    let (mut worker, peer) = settled();
    peer.set(false, "paused");
    // Last query at 10 s: nothing is asked before 40 s, and at 40 s the missed transition is published.
    for offset in [S * 11, S * 25, S * 40 - MS] {
        worker.pass(offset);
    }
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert!(worker.published.is_empty());
    worker.pass(S * 40);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    assert_eq!(worker.published.len(), 1);
    assert_eq!(worker.published[0]["state"], "paused");
    assert_eq!(worker.published[0]["qmp_status"], "paused");
    // A change found by the safety poll also restarts the 1 s cadence, so a silent change back is seen within 1 s.
    peer.set(true, "running");
    worker.pass(S * 41 - MS);
    assert_eq!(worker.published.len(), 1);
    worker.pass(S * 41);
    assert_eq!(worker.published.len(), 2);
    assert_eq!(worker.published[1]["state"], "running");
}

#[test]
fn event_record_split_across_passes_and_across_a_command_is_reassembled() {
    let (mut worker, mut peer) = settled();
    let stop = record(&event("STOP"));
    let (head, tail) = stop.split_at(stop.len() / 2);
    peer.set(false, "paused");
    peer.deliver(head);
    worker.pass(SETTLED + MS);
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert_eq!(worker.qmp.reply_prefix, head);
    peer.deliver(tail);
    worker.pass(SETTLED + MS * 2);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    assert_eq!(worker.published.len(), 1);
    assert_eq!(worker.published[0]["state"], "paused");
    assert!(worker.qmp.reply_prefix.is_empty());

    // Half an event is buffered when a command is sent; the rest arrives before the reply.
    let resume = record(&event("RESUME"));
    let (head, tail) = resume.split_at(7);
    peer.set(true, "running");
    peer.deliver(head);
    assert_eq!(worker.qmp.drain_events(), Drained::Nothing);
    *peer.before_reply.lock().unwrap() = tail.to_vec();
    assert_eq!(
        worker.qmp.call("query-status").unwrap(),
        json!({"running":true,"status":"running"})
    );
    assert_eq!(worker.qmp.drain_events(), Drained::RunState);
    assert!(!worker.qmp.poisoned);
}

#[test]
fn query_waits_for_a_capture_in_flight_and_then_runs_at_once() {
    let (mut worker, mut peer) = settled();
    peer.set(false, "paused");
    peer.event("STOP");
    worker.pass_with(SETTLED + MS, false);
    worker.pass_with(SETTLED + S * 5, false);
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert!(worker.published.is_empty());
    worker.pass_with(SETTLED + S * 5 + MS, true);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    assert_eq!(worker.published[0]["state"], "paused");
}

#[test]
fn shutdown_event_does_not_query_an_exiting_qemu_at_once() {
    let (mut worker, mut peer) = settled();
    // A guest panic with the default action: GUEST_PANICKED and STOP, then SHUTDOWN, then QEMU exits.
    peer.event("GUEST_PANICKED");
    peer.event("STOP");
    peer.event("SHUTDOWN");
    worker.pass(SETTLED + MS);
    worker.pass(SETTLED + S);
    assert_eq!(peer.queries(), SETTLED_QUERIES);
    assert!(worker.published.is_empty());
    // If QEMU is still there one interval later, it is asked as the fixed 1 s poll would have asked.
    peer.set(false, "shutdown");
    worker.pass(SETTLED + S + MS);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    assert_eq!(worker.published[0]["qmp_status"], "shutdown");
}

#[test]
fn closed_connection_is_reported_by_the_next_pass_with_the_reply_path_error() {
    let (qmp, peer) = peer();
    let start = Instant::now();
    let mut worker = Worker {
        qmp,
        poll: StatusPoll::new(start),
        state: json!({"event":"state","state":"running","qmp_status":"running"}),
        published: Vec::new(),
        start,
    };
    worker.pass(Duration::ZERO);
    assert_eq!(worker.qmp.drain_events(), Drained::Nothing);
    peer.stream.shutdown(std::net::Shutdown::Both).unwrap();
    // The drain neither consumes nor judges the end of stream; it asks for the query that does.
    assert_eq!(worker.qmp.drain_events(), Drained::RunState);
    assert!(!worker.qmp.poisoned);
    worker.pass(MS);
    assert_eq!(worker.published.len(), 1);
    assert_eq!(worker.published[0]["state"], "unavailable");
    assert!(worker.published[0]["qmp_error"]
        .as_str()
        .unwrap()
        .contains("QMP"));
    assert!(worker.qmp.poisoned);
    assert_eq!(worker.qmp.drain_events(), Drained::Nothing);
}

#[test]
fn failed_query_keeps_the_one_second_cadence_until_it_answers_again() {
    let (mut worker, peer) = settled();
    *peer.status.lock().unwrap() = Value::Null;
    worker.pass(S * 40);
    assert_eq!(worker.published.len(), 1);
    assert_eq!(worker.published[0]["state"], "unavailable");
    // Far longer than the active window: an unavailable state never falls back to the 30 s interval.
    for second in 41..=70 {
        worker.pass(S * second);
        assert_eq!(peer.queries(), SETTLED_QUERIES + 1 + (second - 40) as usize);
    }
    assert_eq!(worker.published.len(), 1);
    peer.set(true, "running");
    worker.pass(S * 71);
    assert_eq!(worker.published.len(), 2);
    assert_eq!(worker.published[1]["state"], "running");
    assert!(worker.published[1].get("qmp_error").is_none());
}

#[test]
fn run_state_command_schedules_an_immediate_query() {
    let (mut worker, peer) = settled();
    // What the loop does when it sends `system_powerdown`.
    worker.poll.disturbed(worker.start + SETTLED + S * 3);
    worker.pass(SETTLED + S * 3);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    worker.pass(SETTLED + S * 4);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 2);
    assert!(worker.published.is_empty());
}

#[test]
fn event_storm_is_drained_in_bounded_passes_and_a_run_state_event_in_it_is_still_seen() {
    let (mut worker, mut peer) = settled();
    // Short records: the whole storm must fit the socket buffer (8 KiB on macOS), since nothing reads while it is
    // written.
    for _ in 0..EVENT_DRAIN_BOUND * 3 {
        peer.deliver(b"{\"event\":\"RTC_CHANGE\"}\n");
    }
    peer.set(false, "paused");
    peer.event("STOP");
    for pass in 1..=3 {
        worker.pass(SETTLED + MS * pass);
        assert_eq!(peer.queries(), SETTLED_QUERIES, "pass {pass}");
    }
    worker.pass(SETTLED + MS * 4);
    assert_eq!(peer.queries(), SETTLED_QUERIES + 1);
    assert_eq!(worker.published[0]["state"], "paused");
}
