//! Audio between this computer and a phone, one direction at a time.
//!
//! Uses the same encrypted UDP PCM stream as PC audio (`edge-audio`). Each
//! stream gets fresh keys and a fresh socket, so restarting can never reuse a
//! key and nonce. The receiver probes the phone's UDP port first, which opens
//! the path through this machine's firewall for the phone's replies.
//!
//! - **To the phone:** local audio is redirected into the `edge_kvm_phone`
//!   null sink and its monitor is streamed, so it plays on the phone instead of
//!   here. Stopping restores the previous default sink.
//! - **From the phone:** the stream plays wherever this machine's audio goes,
//!   including on to a PC peer, but never back into the phone sink.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use edge_audio::{PacketCipher, SessionSecrets};
use edge_linux_audio::{LinuxAudioReceiver, LinuxAudioSender, PHONE_SINK};
use edge_protocol::{AudioDirection, Frame, PhoneEvent};
use tokio::{net::UdpSocket, sync::mpsc};

use crate::AbortOnDropTask;

/// Phones are on Wi-Fi, so buffer more than PC peers do; quality over latency.
pub(crate) const PHONE_JITTER_TARGET_MS: u16 = 80;
/// Phone Wi-Fi sends in bursts even with a low-latency lock; absorb them.
const PHONE_PLAYBACK_QUEUE_MS: usize = 200;
const PATH_TIMEOUT: Duration = Duration::from_secs(5);

/// A running stream. Dropping it stops the stream and restores routing.
pub(crate) struct PhoneAudio {
    pub generation: u64,
    pub direction: AudioDirection,
    _task: AbortOnDropTask,
}

/// Starts a stream and returns the `AudioStart` frame to send the phone
/// first. When the stream ends by itself, `(generation, reason)` arrives on
/// `stopped`.
pub(crate) fn start(
    direction: AudioDirection,
    phone: SocketAddr,
    state_dir: PathBuf,
    generation: u64,
    stopped: mpsc::UnboundedSender<(u64, String)>,
) -> (Frame, PhoneAudio) {
    let secrets = SessionSecrets::generate();
    let frame = Frame::Phone(PhoneEvent::AudioStart {
        direction,
        session_id: secrets.session_id,
        session_salt: secrets.session_salt,
        session_key: secrets.session_key,
        jitter_target_ms: PHONE_JITTER_TARGET_MS,
    });
    let task = tokio::spawn(async move {
        let reason = run(direction, phone, secrets, state_dir).await;
        stopped.send((generation, reason)).ok();
    });
    (
        frame,
        PhoneAudio {
            generation,
            direction,
            _task: AbortOnDropTask(task),
        },
    )
}

async fn run(
    direction: AudioDirection,
    phone: SocketAddr,
    secrets: SessionSecrets,
    state_dir: PathBuf,
) -> String {
    let bind = match phone.ip() {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = match UdpSocket::bind(bind).await {
        Ok(socket) => Arc::new(socket),
        Err(error) => return format!("couldn't open an audio socket: {error}"),
    };
    let cipher = PacketCipher::new(&secrets);
    let endpoint =
        match edge_linux_audio::establish_peer(&socket, &cipher, phone, phone.ip(), PATH_TIMEOUT)
            .await
        {
            Ok(endpoint) => endpoint,
            Err(error) => return format!("no audio path to the phone: {error:#}"),
        };
    match direction {
        AudioDirection::ToPhone => {
            let sender = match LinuxAudioSender::start_to_sink(
                socket, endpoint, secrets, &state_dir, true, PHONE_SINK,
            )
            .await
            {
                Ok(sender) => sender,
                Err(error) => return format!("couldn't capture this computer's audio: {error:#}"),
            };
            tracing::info!(%endpoint, "streaming audio to the phone");
            while !sender.is_finished() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            sender.stop().await.ok();
            "audio capture on the computer stopped".to_string()
        }
        AudioDirection::FromPhone => {
            let device = edge_linux_audio::phone_playback_sink(&state_dir).await.ok();
            let receiver = match LinuxAudioReceiver::start_on(
                socket,
                endpoint,
                secrets,
                u32::from(PHONE_JITTER_TARGET_MS),
                device,
                PHONE_PLAYBACK_QUEUE_MS,
            )
            .await
            {
                Ok(receiver) => receiver,
                Err(error) => return format!("couldn't play the phone's audio: {error:#}"),
            };
            tracing::info!(%endpoint, "playing audio from the phone");
            while !receiver.is_finished() {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            receiver.failure_reason().await
        }
    }
}
