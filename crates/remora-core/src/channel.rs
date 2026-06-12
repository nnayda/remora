//! The byte-stream + resize pipe returned by spawn and attach.

use remora_protocol::{ChannelInput, ChannelOutput, TerminalSize};
use tokio::sync::mpsc;

use crate::SourceError;

/// Queue depth for each direction of a [`SessionChannel`].
///
/// Bounded so a PTY firehose exerts backpressure instead of growing memory.
pub const CHANNEL_CAPACITY: usize = 256;

/// A live two-way pipe to one session's PTY: protocol messages in, raw PTY
/// output back. Resize rides the input queue, preserving input ordering.
///
/// Death is structural, not signaled: when the transport drops its ends,
/// [`send_bytes`](Self::send_bytes)/[`resize`](Self::resize) return
/// [`SourceError::ChannelClosed`] and [`recv`](Self::recv) returns `None`.
/// There is no close or detach call (spine spike: channel death is only
/// observable locally).
#[derive(Debug)]
pub struct SessionChannel {
    /// Caller → PTY. `Sender` is `Clone`: a kept clone holds the input side
    /// open past this struct's drop.
    pub input: mpsc::Sender<ChannelInput>,
    /// PTY → caller.
    pub output: mpsc::Receiver<ChannelOutput>,
}

impl SessionChannel {
    /// Creates a connected pair: the caller-facing channel plus the
    /// transport-facing ends (input receiver, output sender). Transports
    /// keep the latter two and must drop *both* together to signal death —
    /// dropping only one leaves a half-dead channel (sends succeed into a
    /// queue nobody drains, or `recv` ends while input lingers).
    pub fn pair() -> (
        Self,
        mpsc::Receiver<ChannelInput>,
        mpsc::Sender<ChannelOutput>,
    ) {
        let (input_tx, input_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (output_tx, output_rx) = mpsc::channel(CHANNEL_CAPACITY);
        (
            Self {
                input: input_tx,
                output: output_rx,
            },
            input_rx,
            output_tx,
        )
    }

    /// Sends raw input bytes (keystrokes, pastes) to the session's PTY.
    pub async fn send_bytes(&self, bytes: Vec<u8>) -> Result<(), SourceError> {
        self.input
            .send(ChannelInput::Bytes(bytes))
            .await
            .map_err(|_| SourceError::ChannelClosed)
    }

    /// Propagates a terminal resize to the remote TTY.
    pub async fn resize(&self, size: TerminalSize) -> Result<(), SourceError> {
        self.input
            .send(ChannelInput::Resize(size))
            .await
            .map_err(|_| SourceError::ChannelClosed)
    }

    /// Receives the next output message; `None` means the channel is dead.
    pub async fn recv(&mut self) -> Option<ChannelOutput> {
        self.output.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helpers_wrap_protocol_messages() {
        let (mut channel, mut transport_input, transport_output) = SessionChannel::pair();
        assert_eq!(channel.input.max_capacity(), CHANNEL_CAPACITY);
        assert_eq!(transport_output.max_capacity(), CHANNEL_CAPACITY);

        // Bytes then resize back-to-back: resize rides the input queue, so
        // the transport must see them in send order.
        let size = TerminalSize::new(30, 100).expect("nonzero size");
        channel
            .send_bytes(b"hi".to_vec())
            .await
            .expect("send bytes");
        channel.resize(size).await.expect("send resize");
        let Some(ChannelInput::Bytes(bytes)) = transport_input.recv().await else {
            panic!("expected bytes input first");
        };
        assert_eq!(bytes, b"hi");
        let Some(ChannelInput::Resize(got)) = transport_input.recv().await else {
            panic!("expected resize input second");
        };
        assert_eq!(got, size);

        transport_output
            .send(ChannelOutput::Bytes(b"out".to_vec()))
            .await
            .expect("transport send");
        let Some(ChannelOutput::Bytes(bytes)) = channel.recv().await else {
            panic!("expected bytes output");
        };
        assert_eq!(bytes, b"out");
    }

    #[tokio::test]
    async fn dropped_transport_ends_mean_channel_closed() {
        let (mut channel, transport_input, transport_output) = SessionChannel::pair();
        drop(transport_input);
        drop(transport_output);

        let err = channel.send_bytes(b"x".to_vec()).await.expect_err("closed");
        assert!(matches!(err, SourceError::ChannelClosed));
        let size = TerminalSize::new(1, 1).expect("nonzero size");
        let err = channel.resize(size).await.expect_err("closed");
        assert!(matches!(err, SourceError::ChannelClosed));
        assert!(channel.recv().await.is_none());
    }
}
