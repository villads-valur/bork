use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("tmux error: {0}")]
    Tmux(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
