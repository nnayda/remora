//! The ctl server (`ctl.sock`) lands in plan Task 9; this stub keeps the
//! daemon wiring compiling and honours the shutdown token so `serve` exits
//! cleanly on signal.

use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

pub async fn serve_ctl(
    _listener: UnixListener,
    _state: crate::daemon::DaemonState,
    shutdown: CancellationToken,
) {
    shutdown.cancelled().await;
}
