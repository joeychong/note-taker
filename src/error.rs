use thiserror::Error;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("{0} queue is full; stopping to avoid silent data loss")]
    Backpressure(&'static str),
    #[error("{0} output worker stopped unexpectedly")]
    OutputClosed(&'static str),
}
