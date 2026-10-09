//! Drain only to discard while waiting for the encoder's existing exit attestation.
//! A closed media pipe alone never proves native capture cleanup.
use std::{process::ExitStatus, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout},
};

pub struct Outcome {
    /// Set only for an observed exit within the graceful deadline. The caller
    /// still applies its encoder-specific cleanup-attested exit-code contract.
    pub graceful_status: Option<ExitStatus>,
    pub child_reaped: bool,
}

pub async fn stop_and_wait(
    child: &mut Child,
    mut input: ChildStdin,
    output: &mut ChildStdout,
) -> Outcome {
    let _ = tokio::time::timeout(Duration::from_millis(100), input.write_all(b"stop\n")).await;
    drop(input);
    // The pipe can contain a partial AU at cancellation. Parse nothing here:
    // discard in a fixed-size buffer so the child can finish that write and
    // process/acknowledge stop. Never allocate based on untrusted frame lengths.
    let graceful = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = async {
            let _ = tokio::io::copy(output, &mut tokio::io::sink()).await;
            // EOF/read failure does not attest cleanup and must not spin.
            std::future::pending::<()>().await;
        };
        tokio::select! {
            status = child.wait() => status,
            _ = drain => unreachable!("drain remains pending after EOF"),
        }
    })
    .await;
    if let Ok(Ok(status)) = graceful {
        return Outcome {
            graceful_status: Some(status),
            child_reaped: true,
        };
    }
    // A forced exit, including a successful reap after the deadline, is never
    // promoted to acknowledged native cleanup.
    let _ = child.start_kill();
    let child_reaped = matches!(
        tokio::time::timeout(Duration::from_secs(1), child.wait()).await,
        Ok(Ok(_))
    );
    Outcome {
        graceful_status: None,
        child_reaped,
    }
}
