//! Streams silent audio over localhost into the real Linux receiver and output
//! device, with bursty delivery and a deliberate sender clock offset, then
//! reports underruns and dropped frames.
//!
//! ```text
//! cargo run -p edge-linux-audio --example playback_soak -- [seconds] [drift_ppm] [burst_packets]
//! ```
//!
//! The payload is silence, so nothing is audible. A healthy run reports no
//! dropped output frames and no underruns after the initial prebuffer.

use std::{sync::Arc, time::Duration};

use edge_audio::{
    AudioPacket, FRAME_MS, PacketCipher, PcmCodec, SAMPLES_PER_FRAME, SessionSecrets,
};
use edge_linux_audio::LinuxAudioReceiver;
use tokio::net::UdpSocket;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map_or(Ok(20), |value| value.parse())?;
    let drift_ppm: f64 = args.next().map_or(Ok(4_000.0), |value| value.parse())?;
    let burst: u32 = args.next().map_or(Ok(4), |value| value.parse())?;

    let receiver_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let sender_socket = UdpSocket::bind("127.0.0.1:0").await?;
    let secrets = SessionSecrets::generate();
    let receiver = LinuxAudioReceiver::start(
        receiver_socket.clone(),
        sender_socket.local_addr()?,
        secrets.clone(),
        60,
    )
    .await?;

    let cipher = PacketCipher::new(&secrets);
    let payload = PcmCodec::encode(&[0.0; SAMPLES_PER_FRAME])?;
    let destination = receiver_socket.local_addr()?;
    // A positive offset makes the sender's clock run fast relative to the
    // output device; a negative one makes it run slow.
    let period = Duration::from_secs_f64(f64::from(FRAME_MS) / 1_000.0 / (1.0 + drift_ppm / 1e6));
    let start = tokio::time::Instant::now();
    let total = (seconds as f64 / period.as_secs_f64()) as u64;
    let mut sequence = 1_u64;
    while sequence <= total {
        // Deliver packets in bursts, as Wi-Fi and the capture quantum do.
        let due = start + period * (sequence + u64::from(burst) - 1) as u32;
        tokio::time::sleep_until(due).await;
        for _ in 0..burst {
            let packet = cipher.seal(&AudioPacket {
                sequence,
                sample_timestamp: (sequence as u32).wrapping_mul(240),
                flags: 0,
                payload: payload.clone(),
            })?;
            sender_socket.send_to(&packet, destination).await?;
            sequence += 1;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stats = receiver.stats();
    println!(
        "seconds={seconds} drift_ppm={drift_ppm} burst={burst} authenticated={} late={} concealed={} underruns={} dropped_output_frames={} queued_output_ms={}",
        stats.authenticated_packets,
        stats.late_packets,
        stats.concealed_packets,
        stats.output_underruns,
        stats.dropped_output_frames,
        stats.queued_output_ms,
    );
    Ok(())
}
