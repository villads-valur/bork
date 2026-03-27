use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("tmux error: {0}")]
    Tmux(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
