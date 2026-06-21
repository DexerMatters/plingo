use thiserror::Error;

/// Errors that can occur while resolving actions in layers.
#[derive(Debug, Error, Clone)]
pub enum ActionError {
    #[error("Missing resource for action {action} while resolving in layer {layer}")]
    MissingResource { action: String, layer: String },
    #[error(
        "Await target layer {target} does not flow down to requester layer {layer} for action {action}"
    )]
    AwaitPathMissing {
        action: String,
        target: String,
        layer: String,
    },
    #[error("Layer {layer} failed while resolving action {action}: {reason}")]
    ErrorFromLayer {
        action: String,
        layer: String,
        reason: String,
    },
    #[error("Layer channel closed while resolving action {action} in layer {layer}")]
    ChannelClosed { action: String, layer: String },
    #[error("Retry limit reached while resolving action {action} in layer {layer}")]
    RetryLimitReached { action: String, layer: String },
    #[error(
        "Layer call cycle while resolving action {action}: layer {layer} cannot synchronously call layer {target}"
    )]
    LayerCallCycle {
        action: String,
        layer: String,
        target: String,
    },
}

/// Errors that can occur while building the runtime.
#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error("Runtime is already running")]
    AlreadyRunning,
}

/// Errors that can occur while processing deltas in layers.
#[derive(Debug, Error)]
pub(crate) enum DeltaFlowError {
    #[error("Top layer {layer} failed while emitting delta: {reason}")]
    TopEmitFailed { layer: String, reason: String },
    #[error("Top layer {layer} received an unexpected incoming delta")]
    UnexpectedTopDelta { layer: String },
    #[error("Missing lower sender while propagating delta to layer {layer}")]
    MissingLowerSender { layer: String },
    #[error("Lower sender closed while propagating delta to layer {layer}")]
    LowerSenderClosed { layer: String },
    #[error("Layer {layer} failed while processing delta: {reason}")]
    MiddlePassFailed { layer: String, reason: String },
    #[error("Bottom layer {layer} failed while consuming delta: {reason}")]
    BottomConsumeFailed { layer: String, reason: String },
}
