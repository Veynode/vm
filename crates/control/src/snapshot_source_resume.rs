//! Confirmed recovery of the same live source after an unsuccessful capture.
//! Never starts a process, selects another disk, or restores a snapshot.
use super::*;

struct Permit {
    operation: Operation,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut records) = self.operation.operations.records.lock() {
            if let Some(record) = records.get_mut(self.operation.id()) {
                record.source_resume_pending = false;
            }
        }
    }
}
fn admit(operations: &Operations, record: &Record) -> Result<Permit> {
    let mut records = operations.records.lock().unwrap();
    let current = records
        .get_mut(&record.operation_id)
        .context("source operation disappeared")?;
    ensure!(
        current.action == "create"
            && current.active()
            && current.digest()? == record.digest()?
            && current.source_channel_released
            && !current.source_resume_pending,
        "capture channel is still owned or source resume is already in flight"
    );
    current.source_resume_pending = true;
    Ok(Permit {
        operation: Operation {
            operations: operations.clone(),
            id: record.operation_id.clone(),
        },
    })
}
impl Manager {
    pub(crate) fn snapshot_resume_source_dispatch(
        &mut self,
        method: &str,
        args: &Value,
        deferred: &mut Option<Completion>,
    ) -> Result<Option<Value>> {
        if method != "snapshot_resume_source" {
            return Ok(None);
        }
        ensure!(
            args.as_object().is_some_and(|o| o.len() == 3
                && ["id", "operation_id", "confirm"]
                    .iter()
                    .all(|k| o.contains_key(*k))),
            "snapshot_resume_source accepts exactly id, operation_id and confirm"
        );
        let id = args["id"].as_str().context("source VM id missing")?;
        validate_id(id)?;
        ensure!(
            args["confirm"] == id,
            "source resume confirm must exactly equal VM id"
        );
        let op = args["operation_id"]
            .as_str()
            .context("source operation id missing")?;
        jobs::validate_job_id(op)?;
        let record = self
            .snapshot_ops
            .records
            .lock()
            .unwrap()
            .get(op)
            .filter(|r| r.id == id)
            .cloned()
            .context("source operation not found for VM")?;
        ensure!(
            record.action == "create"
                && record.active()
                && record.source_channel_released
                && !record.source_resume_pending,
            "source recovery requires an unfinished capture whose host channel has returned"
        );
        let config = self
            .configs
            .get(id)
            .context("source VM not registered")?
            .clone();
        ensure!(
            config.backend == Backend::AppleVirtualization && config.guest == "linux",
            "live source recovery supports Apple Linux only"
        );
        create_recovery::validate_live_intent(&self.dir, &config, &record)?;
        let run = self
            .running
            .get_mut(id)
            .context("source worker is not owned by this daemon; use retirement reconciliation")?;
        ensure!(
            run.poll_exit()?.is_none()
                && record.worker_instance.as_deref()
                    == Some(run.bridge.lock().unwrap().worker_instance.as_str()),
            "original source worker is gone or changed"
        );
        let mut channel = WorkerChannel::from_running(run)?;
        let permit = admit(&self.snapshot_ops, &record)?;
        let operations = self.snapshot_ops.clone();
        let vm = id.to_owned();
        let operation = op.to_owned();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name("snapshot-resume-source".into()).spawn(move || {
            let result=(|| {
                let already_running=if let Some(binding)=&record.capture_binding {
                    let reply=channel.request("snapshot_resume",json!({"capture_id":binding.capture_id}),Duration::from_secs(60))?;
                    verify_resume(&reply,&binding.capture_id)?;
                    match (reply["resumed"].as_bool(),reply.get("already_running").and_then(Value::as_bool)) {
                        (Some(true),None|Some(false))=>false,
                        (Some(false),Some(true))=>true,
                        _=>bail!("source resume acknowledgment is malformed"),
                    }
                } else {
                    ensure!(matches!(record.phase.as_str(),"admitted"|"checking_support"),"unbound source may have dispatched a capture");
                    let reply=channel.request("status",json!({}),Duration::from_secs(30))?;
                    ensure!(reply["state"]=="running","pre-capture source running state is unconfirmed");
                    true
                };
                permit.operation.update("source_resumed_after_failure","failed",
                    Some(json!({"source_resumed":!already_running,"source_already_running":already_running,"source_running_confirmed":true,"restored":false,"snapshot_result_verified":false,
                        "original_worker_preserved":true,"registry_selection_preserved":true})),
                    Some("capture_source_resumed"),None)?;
                Ok::<_,anyhow::Error>(())
            })();
            drop(channel);
            drop(permit);
            let response=match result {
                Ok(())=>operations.status(&vm,&operation).map(|v|json!({"id":0,"result":v})),
                Err(error)=>Err(error),
            };
            let _=tx.send(match response { Ok(v)=>v,Err(e)=>json!({"id":0,"error":format!("Source resume unconfirmed; operation stays fenced: {e}")}) });
        })?;
        *deferred = Some(Completion::Backend(Pending {
            receiver: rx,
            number: 0,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            timeout: Duration::from_secs(30),
            post: None,
        }));
        Ok(Some(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn released_channel_and_resume_permit_are_runtime_only_and_do_not_consume_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = crate::test_private_state(temp.path());
        let operations = Operations::open(&path).unwrap();
        let op = operations
            .begin("linux1", "before", "create", Some("a".repeat(64)))
            .unwrap();
        op.fail("fixture_capture_failed", true).unwrap();
        let before = operations.records.lock().unwrap()[op.id()].clone();
        assert!(admit(&operations, &before).is_err());
        op.release_source_channel();
        let ready = operations.records.lock().unwrap()[op.id()].clone();
        let permit = admit(&operations, &ready).unwrap();
        assert!(admit(&operations, &ready).is_err());
        assert_eq!(
            operations.status("linux1", op.id()).unwrap()["source_resume_pending"],
            true
        );
        drop(permit);
        assert!(operations
            .status("linux1", op.id())
            .unwrap()
            .get("source_resume_pending")
            .is_none());
        assert_eq!(
            before.digest().unwrap(),
            operations.records.lock().unwrap()[op.id()]
                .digest()
                .unwrap()
        );
        let reopened = Operations::open(&path).unwrap();
        assert!(!reopened.records.lock().unwrap()[op.id()].source_channel_released);
        let restarted = reopened.records.lock().unwrap()[op.id()].clone();
        assert!(admit(&reopened, &restarted).is_err());
    }
}
