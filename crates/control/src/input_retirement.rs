//! Pending synthetic input release survives lease retirement and concurrent cleanup.
use anyhow::{bail, Result};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputRetirement {
    pub owner: String,
    pub vm: String,
    pub epoch: u64,
    pub worker_instance: String,
}
#[derive(Default)]
pub struct Retirements {
    entries: BTreeSet<InputRetirement>,
}
impl Retirements {
    pub fn record(&mut self, record: InputRetirement) -> Result<()> {
        if !self.entries.contains(&record) && self.entries.len() >= 4096 {
            bail!("input cleanup ledger capacity reached; confirm outstanding cleanup first")
        }
        self.entries.insert(record);
        Ok(())
    }
    pub fn contains(&self, record: &InputRetirement) -> bool {
        self.entries.contains(record)
    }
    pub fn complete(&mut self, record: &InputRetirement) {
        self.entries.remove(record);
    }
    pub fn records(&self, owner: Option<&str>, vm: Option<&str>) -> Vec<InputRetirement> {
        self.entries
            .iter()
            .filter(|r| {
                owner.is_none_or(|owner| r.owner == owner) && vm.is_none_or(|vm| r.vm == vm)
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        io::BufReader,
        os::unix::net::UnixListener,
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };
    #[test]
    fn real_unix_failure_retry_requires_exact_ack_and_preserves_other_incarnation() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let record = InputRetirement {
            owner: "a".repeat(64),
            vm: "vm".into(),
            epoch: 2,
            worker_instance: "b".repeat(64),
        };
        let mut unrelated = record.clone();
        unrelated.worker_instance = "c".repeat(64);
        let ledger = Arc::new(Mutex::new(Retirements::default()));
        ledger.lock().unwrap().record(record.clone()).unwrap();
        ledger.lock().unwrap().record(unrelated.clone()).unwrap();
        let expected = record.clone();
        let server = thread::spawn(move || {
            for reply in [
                json!({"error":"injected release failure"}),
                json!({"result":{"released":false}}),
                json!({"result":{"released":true,"synthetic_release_dispatched":true,"through_epoch":3}}),
                json!({"result":{"released":true,"synthetic_release_dispatched":true,"through_epoch":2}}),
            ] {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("fixture accept: {e}"),
                    }
                };
                // Darwin accepted sockets inherit the listener's O_NONBLOCK.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request: Value = serde_json::from_slice(
                    &crate::read_frame(&mut BufReader::new(stream.try_clone().unwrap()))
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(request["method"], "input_revoke");
                assert_eq!(request["arguments"]["through_epoch"], expected.epoch);
                assert_eq!(
                    request["arguments"]["worker_instance"],
                    expected.worker_instance
                );
                crate::write_json(&mut stream, &reply).unwrap();
            }
        });
        for _ in 0..3 {
            assert!(crate::retire_input(dir.path(), &record, &ledger).is_err());
            assert!(ledger.lock().unwrap().contains(&record));
        }
        assert!(crate::retire_input(dir.path(), &record, &ledger).is_ok());
        assert!(!ledger.lock().unwrap().contains(&record));
        assert!(ledger.lock().unwrap().contains(&unrelated));
        server.join().unwrap();
        assert!(
            crate::retire_input(dir.path(), &record, &ledger).is_ok(),
            "already confirmed tuple needs no new RPC"
        );
    }
}
