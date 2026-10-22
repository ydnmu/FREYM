use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use ashpd::desktop::ResponseError;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use frame_contracts::{ErrorCode, RecorderError};
use futures_util::StreamExt;

use crate::error;

pub(crate) fn session_available() -> bool {
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session_type != "wayland" && session_type != "x11" {
        return false;
    }
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return false;
    };
    runtime
        .block_on(tokio::time::timeout(Duration::from_secs(2), async {
            let portal = Screencast::new().await?;
            let types = portal.available_source_types().await?;
            Ok::<_, ashpd::Error>(
                types.contains(ashpd::enumflags2::BitFlags::from_flag(SourceType::Monitor)),
            )
        }))
        .is_ok_and(|result| result.unwrap_or(false))
}

pub(crate) struct PortalSession {
    runtime: tokio::runtime::Runtime,
    session: Arc<Session<Screencast>>,
    remote: OwnedFd,
    closed: Arc<AtomicBool>,
    close_requested: bool,
    watcher: tokio::task::JoinHandle<()>,
    pub(crate) node_id: u32,
    pub(crate) size: Option<(u32, u32)>,
}

impl PortalSession {
    pub(crate) fn open(cancel: Arc<AtomicBool>) -> Result<Self, RecorderError> {
        let runtime = tokio::runtime::Runtime::new().map_err(|e| {
            error(
                ErrorCode::Internal,
                format!("Portal runtime could not start: {e}"),
                "Retry after restoring the desktop session.",
            )
        })?;
        let (session, remote, node_id, size) = runtime.block_on(async {
            let proxy = Screencast::new().await.map_err(portal_error)?;
            let session = Arc::new(tokio::select! {
                session = proxy.create_session(Default::default()) => session.map_err(portal_error)?,
                () = wait_cancel(Arc::clone(&cancel)) => return Err(cancelled()),
            });
            let result = async {
                if cancel.load(Ordering::Acquire) { return Err(cancelled()); }
                let selection = tokio::select! {
                    selection = proxy.select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(CursorMode::Embedded)
                            .set_sources(ashpd::enumflags2::BitFlags::from_flag(
                                SourceType::Monitor,
                            ))
                            .set_multiple(false),
                    ) => selection.map_err(portal_error)?,
                    () = wait_cancel(Arc::clone(&cancel)) => return Err(cancelled()),
                };
                selection.response().map_err(portal_error)?;
                if cancel.load(Ordering::Acquire) { return Err(cancelled()); }
                let reply = tokio::select! {
                    reply = proxy.start(&session, None, Default::default()) => reply.map_err(portal_error)?,
                    () = wait_cancel(Arc::clone(&cancel)) => return Err(cancelled()),
                };
                let streams = reply.response().map_err(portal_error)?;
                let stream = streams.streams().first().ok_or_else(|| {
                    error(
                        ErrorCode::PermissionDenied,
                        "The portal returned no selected screen stream.",
                        "Choose a screen in the system picker and retry.",
                    )
                })?;
                let size = stream
                    .size()
                    .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)));
                let node_id = stream.pipe_wire_node_id();
                let remote = tokio::select! {
                    remote = proxy.open_pipe_wire_remote(&session, Default::default()) => remote.map_err(portal_error)?,
                    () = wait_cancel(Arc::clone(&cancel)) => return Err(cancelled()),
                };
                Ok::<_, RecorderError>((remote, node_id, size))
            }
            .await;
            if result.is_err() {
                let _ = session.close().await;
            }
            result.map(|(remote, node_id, size)| (session, remote, node_id, size))
        })?;
        let closed = Arc::new(AtomicBool::new(false));
        let watcher = runtime.spawn({
            let session = Arc::clone(&session);
            let closed = Arc::clone(&closed);
            async move {
                if let Ok(mut stream) = session.receive_closed().await {
                    let _ = stream.next().await;
                    closed.store(true, Ordering::Release);
                }
            }
        });
        Ok(Self {
            runtime,
            session,
            remote,
            closed,
            close_requested: false,
            watcher,
            node_id,
            size,
        })
    }

    pub(crate) fn fd(&self) -> i32 {
        // pipewiresrc duplicates this descriptor when it connects. Keep the
        // portal-owned original alive until the video pipeline is stopped.
        self.remote.as_raw_fd()
    }
    pub(crate) fn revoked(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&mut self) {
        if self.close_requested {
            return;
        }
        self.close_requested = true;
        self.closed.store(true, Ordering::Release);
        self.watcher.abort();
        let _ = self.runtime.block_on(tokio::time::timeout(
            Duration::from_secs(2),
            self.session.close(),
        ));
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn portal_error(e: ashpd::Error) -> RecorderError {
    let code = match &e {
        ashpd::Error::Response(ResponseError::Cancelled | ResponseError::Other) => {
            ErrorCode::PermissionDenied
        }
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(_)) => ErrorCode::PermissionDenied,
        _ => ErrorCode::SourceLost,
    };
    error(
        code,
        format!("ScreenCast portal request failed: {e}"),
        "Grant screen sharing in the system picker, or inspect the portal service and retry.",
    )
}

async fn wait_cancel(cancel: Arc<AtomicBool>) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn cancelled() -> RecorderError {
    error(
        ErrorCode::InvalidTransition,
        "Screen selection was stopped before recording began.",
        "Start a new recording when ready.",
    )
}
