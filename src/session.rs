//! Máquina de estados da sessão de captura.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Grava até o usuário parar (como a gravação de tela do GNOME).
    Manual,
    /// Mantém os últimos N minutos e salva retroativamente.
    Replay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Buffering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    #[error("já existe uma captura em andamento")]
    AlreadyRunning,
    #[error("nenhuma captura em andamento")]
    NotRunning,
    #[error("salvar clip só é possível no modo replay")]
    NotBuffering,
}

pub struct Session {
    state: State,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn start(&mut self, mode: Mode) -> Result<(), SessionError> {
        if self.state != State::Idle {
            return Err(SessionError::AlreadyRunning);
        }
        self.state = match mode {
            Mode::Manual => State::Recording,
            Mode::Replay => State::Buffering,
        };
        Ok(())
    }

    /// Para a captura e devolve o estado em que estava.
    pub fn stop(&mut self) -> Result<State, SessionError> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => Err(SessionError::NotRunning),
            previous => Ok(previous),
        }
    }

    /// Valida que um clip retroativo pode ser salvo agora.
    pub fn can_save_clip(&self) -> Result<(), SessionError> {
        if self.state == State::Buffering {
            Ok(())
        } else {
            Err(SessionError::NotBuffering)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_idle() {
        assert_eq!(Session::new().state(), State::Idle);
    }

    #[test]
    fn manual_start_goes_to_recording() {
        let mut s = Session::new();
        s.start(Mode::Manual).unwrap();
        assert_eq!(s.state(), State::Recording);
    }

    #[test]
    fn replay_start_goes_to_buffering() {
        let mut s = Session::new();
        s.start(Mode::Replay).unwrap();
        assert_eq!(s.state(), State::Buffering);
    }

    #[test]
    fn cannot_start_twice() {
        let mut s = Session::new();
        s.start(Mode::Manual).unwrap();
        assert_eq!(s.start(Mode::Replay), Err(SessionError::AlreadyRunning));
        assert_eq!(s.state(), State::Recording);
    }

    #[test]
    fn stop_returns_previous_state_and_goes_idle() {
        let mut s = Session::new();
        s.start(Mode::Replay).unwrap();
        assert_eq!(s.stop(), Ok(State::Buffering));
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn stop_when_idle_fails() {
        assert_eq!(Session::new().stop(), Err(SessionError::NotRunning));
    }

    #[test]
    fn clip_only_while_buffering() {
        let mut s = Session::new();
        assert_eq!(s.can_save_clip(), Err(SessionError::NotBuffering));
        s.start(Mode::Manual).unwrap();
        assert_eq!(s.can_save_clip(), Err(SessionError::NotBuffering));
        s.stop().unwrap();
        s.start(Mode::Replay).unwrap();
        assert_eq!(s.can_save_clip(), Ok(()));
    }
}
