//! The set of "please stop" signals shared by the long-running commands
//! (`noeio boot`, `noeio forward`).
//!
//! Every signal here means *exit*. In particular `SIGHUP` is not a reload:
//! neither command has anything to reload, and a closed terminal or a dropped
//! SSH session must end the process rather than leave it orphaned.

/// Wait for the first shutdown signal and name it (for the log line).
#[cfg(unix)]
pub async fn wait_for_shutdown() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    // A failed registration is logged and that signal is simply not waited
    // on; Ctrl+C always works.
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|err| tracing::warn!("failed to register SIGTERM handler: {err}"))
        .ok();
    let mut sighup = signal(SignalKind::hangup())
        .map_err(|err| tracing::warn!("failed to register SIGHUP handler: {err}"))
        .ok();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = recv(&mut sigterm) => "SIGTERM",
        _ = recv(&mut sighup) => "SIGHUP",
    }
}

#[cfg(unix)]
async fn recv(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(sig) => {
            sig.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// Windows: Ctrl+C, Ctrl+Break, console close, logoff and shutdown all end
/// the process.
#[cfg(windows)]
pub async fn wait_for_shutdown() -> &'static str {
    use tokio::signal::windows;

    async fn recv<S>(sig: std::io::Result<S>, name: &'static str) -> &'static str
    where
        S: SignalRecv,
    {
        match sig {
            Ok(mut sig) => {
                sig.wait().await;
                name
            }
            Err(err) => {
                tracing::warn!("failed to register {name} handler: {err}");
                std::future::pending().await
            }
        }
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
        name = recv(windows::ctrl_break(), "Ctrl+Break") => name,
        name = recv(windows::ctrl_close(), "console close") => name,
        name = recv(windows::ctrl_logoff(), "logoff") => name,
        name = recv(windows::ctrl_shutdown(), "shutdown") => name,
    }
}

#[cfg(windows)]
trait SignalRecv {
    async fn wait(&mut self);
}

#[cfg(windows)]
macro_rules! impl_signal_recv {
    ($($t:ty),*) => {$(
        impl SignalRecv for $t {
            async fn wait(&mut self) {
                let _ = self.recv().await;
            }
        }
    )*};
}

#[cfg(windows)]
impl_signal_recv!(
    tokio::signal::windows::CtrlBreak,
    tokio::signal::windows::CtrlClose,
    tokio::signal::windows::CtrlLogoff,
    tokio::signal::windows::CtrlShutdown
);

/// Other platforms: Ctrl+C only.
#[cfg(not(any(unix, windows)))]
pub async fn wait_for_shutdown() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl+C"
}
