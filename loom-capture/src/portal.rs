//! xdg-desktop-portal ScreenCast handshake (async, `ashpd`/zbus).
//!
//! Returns the PipeWire remote fd + node id for a user-selected monitor; the
//! synchronous PipeWire stream (`stream.rs`) takes it from there. A picker dialog
//! appears each time this runs — restore-token persistence is deferred (M1.4+),
//! see the crate docs. Adapted from the R2 spike, which proved this path on the
//! KDE/Wayland/NVIDIA stack.

use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::enumflags2::BitFlags;

use crate::CaptureError;

/// Open a single-monitor ScreenCast session; returns `(pipewire_fd, node_id)`.
pub async fn open_screencast() -> Result<(OwnedFd, u32), CaptureError> {
    let proxy = Screencast::new().await.map_err(CaptureError::portal)?;

    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(CaptureError::portal)?;

    // Cursor embedded in the stream: input isn't wired until M4, so the streamed
    // cursor is the only pointer the client sees. One monitor, no multi-select.
    let sources = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
        .set_sources(BitFlags::from(SourceType::Monitor))
        .set_multiple(false);
    proxy
        .select_sources(&session, sources)
        .await
        .map_err(CaptureError::portal)?
        .response()
        .map_err(CaptureError::portal)?;

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(CaptureError::portal)?
        .response()
        .map_err(CaptureError::portal)?;
    let node_id = response
        .streams()
        .first()
        .map(|s| s.pipe_wire_node_id())
        .ok_or_else(|| CaptureError::Portal("portal returned no streams".into()))?;

    let fd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(CaptureError::portal)?;

    Ok((fd, node_id))
}
