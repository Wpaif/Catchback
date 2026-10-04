//! Seleção de tela/janela via xdg-desktop-portal (ScreenCast + PipeWire).

use std::os::fd::{AsRawFd, OwnedFd};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

use crate::capture::PipewireSource;

#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("falha no portal: {0}")]
    Portal(#[from] ashpd::Error),
    #[error("nenhuma tela ou janela foi selecionada")]
    NoStream,
}

/// Mantém a sessão e o fd do PipeWire vivos enquanto a captura roda.
pub struct PortalCapture {
    pub source: PipewireSource,
    _fd: OwnedFd,
    session: Session<Screencast>,
}

impl PortalCapture {
    /// Abre o diálogo do sistema para escolher o que capturar.
    pub async fn request() -> Result<Self, PortalError> {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?;
        let streams = proxy.start(&session, None, Default::default()).await?.response()?;
        let node_id = streams.streams().first().ok_or(PortalError::NoStream)?.pipe_wire_node_id();
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok(Self { source: PipewireSource { fd: fd.as_raw_fd(), node_id }, _fd: fd, session })
    }

    pub async fn close(self) {
        let _ = self.session.close().await;
    }
}
