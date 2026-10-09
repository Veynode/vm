//! Install handlers before advertising the listener. Both Unix termination
//! signals enter the same bounded transport/Guest reconciliation path.
use std::{future::Future, io};

#[cfg(unix)]
pub fn install() -> io::Result<impl Future<Output = io::Result<()>>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        let received = tokio::select! {
            received = interrupt.recv() => received,
            received = terminate.recv() => received,
        };
        received.ok_or_else(|| io::Error::other("shutdown signal stream closed"))
    })
}
#[cfg(not(unix))]
pub fn install() -> io::Result<impl Future<Output = io::Result<()>>> {
    Ok(tokio::signal::ctrl_c())
}

#[cfg(all(test, unix))]
#[path = "shutdown_signal_tests.rs"]
mod tests;
