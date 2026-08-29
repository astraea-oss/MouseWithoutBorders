use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use ashpd::{
    Error as PortalError,
    desktop::{
        Session,
        input_capture::{
            ActivatedBarrier, Barrier, Capabilities, CreateSessionOptions, InputCapture,
            ReleaseOptions, StartOptions,
        },
    },
};
use edge_protocol::{Edge, InputEvent, MouseButton};
use futures_util::StreamExt;
use reis::{
    ei::{self, button::ButtonState, keyboard::KeyState},
    event::{DeviceCapability, EiEvent},
};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

use crate::{LinuxInputError, Result};

const PORTAL_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(3);
const PORTAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    Activated {
        activation_id: u32,
        edge: Edge,
        normalized_position: f32,
    },
    Input(InputEvent),
    Deactivated,
    EmergencyReleased,
    LayoutChanged {
        previous_zone_set: u32,
        current_zone_set: u32,
    },
    BackendFailed(String),
}

#[derive(Debug)]
enum CaptureCommand {
    Arm(oneshot::Sender<Result<()>>),
    Release {
        cursor_position: Option<(f64, f64)>,
        response: oneshot::Sender<Result<()>>,
    },
    Disarm(oneshot::Sender<Result<()>>),
    Shutdown,
}

#[derive(Debug)]
pub struct PortalCaptureBackend {
    command_tx: mpsc::Sender<CaptureCommand>,
    event_rx: mpsc::UnboundedReceiver<CaptureEvent>,
    zone_set: Arc<AtomicU32>,
}

impl PortalCaptureBackend {
    pub async fn preflight(edge: Edge) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (startup_cancel_tx, startup_cancel_rx) = oneshot::channel();
        let zone_set = Arc::new(AtomicU32::new(0));
        let task_zone_set = zone_set.clone();
        std::thread::Builder::new()
            .name("edge-portal-capture".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = event_tx.send(CaptureEvent::BackendFailed(format!(
                            "failed to create portal capture runtime: {error}"
                        )));
                        return;
                    }
                };
                let capture = run_portal_capture(
                    edge,
                    command_rx,
                    event_tx.clone(),
                    ready_tx.clone(),
                    task_zone_set,
                );
                let startup_cancel = async move {
                    if startup_cancel_rx.await.is_err() {
                        futures_util::future::pending::<()>().await;
                    }
                };
                let result = runtime.block_on(async {
                    tokio::select! {
                        result = capture => result,
                        () = startup_cancel => Ok(()),
                    }
                });
                if let Err(error) = result {
                    let detail = error.to_string();
                    let _ = ready_tx.send(Err(LinuxInputError::LibeiInit(detail.clone())));
                    let _ = event_tx.send(CaptureEvent::BackendFailed(error.to_string()));
                }
            })
            .map_err(|error| {
                LinuxInputError::LibeiInit(format!(
                    "failed to start portal capture thread: {error}"
                ))
            })?;
        match time::timeout(PORTAL_PREFLIGHT_TIMEOUT, ready_rx.recv()).await {
            Ok(Some(result)) => result?,
            Ok(None) => {
                return Err(LinuxInputError::LibeiInit(
                    "InputCapture portal task ended during preflight".to_string(),
                ));
            }
            Err(_) => {
                // Cancelling the setup future is important: returning while its dedicated
                // thread remains blocked leaks both the thread and (on affected XDPH
                // versions) the compositor-side capture session. Repeated role attempts
                // would eventually wedge the portal for every application.
                let _ = startup_cancel_tx.send(());
                return Err(LinuxInputError::LibeiInit(
                    "InputCapture portal stopped responding during preflight; restart xdg-desktop-portal-hyprland before retrying"
                        .to_string(),
                ));
            }
        }
        Ok(Self {
            command_tx,
            event_rx,
            zone_set,
        })
    }

    pub fn zone_set(&self) -> u32 {
        self.zone_set.load(Ordering::Acquire)
    }

    pub async fn arm(&self) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(CaptureCommand::Arm(response))
            .await
            .map_err(|_| capture_task_closed())?;
        receiver.await.map_err(|_| capture_task_closed())?
    }

    pub async fn release(&self, cursor_position: Option<(f64, f64)>) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(CaptureCommand::Release {
                cursor_position,
                response,
            })
            .await
            .map_err(|_| capture_task_closed())?;
        receiver.await.map_err(|_| capture_task_closed())?
    }

    pub async fn disarm(&self) -> Result<()> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(CaptureCommand::Disarm(response))
            .await
            .map_err(|_| capture_task_closed())?;
        receiver.await.map_err(|_| capture_task_closed())?
    }

    pub async fn next_event(&mut self) -> Option<CaptureEvent> {
        self.event_rx.recv().await
    }
}

impl Drop for PortalCaptureBackend {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(CaptureCommand::Shutdown);
    }
}

fn capture_task_closed() -> LinuxInputError {
    LinuxInputError::LibeiInit("portal capture task is not running".to_string())
}

#[derive(Debug, Clone, Copy)]
struct BarrierMetadata {
    edge: Edge,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

async fn run_portal_capture(
    edge: Edge,
    mut command_rx: mpsc::Receiver<CaptureCommand>,
    event_tx: mpsc::UnboundedSender<CaptureEvent>,
    ready_tx: mpsc::UnboundedSender<Result<()>>,
    zone_set_state: Arc<AtomicU32>,
) -> Result<()> {
    let input_capture = InputCapture::new().await.map_err(portal_error)?;
    let capabilities = Capabilities::Keyboard | Capabilities::Pointer;
    let (session, available_capabilities) =
        create_and_start_session(&input_capture, capabilities).await?;
    if !available_capabilities.contains(Capabilities::Keyboard)
        || !available_capabilities.contains(Capabilities::Pointer)
    {
        return Err(LinuxInputError::LibeiInit(format!(
            "InputCapture portal granted {available_capabilities:?}; keyboard and pointer are required"
        )));
    }

    let (mut zone_set, mut barriers) = configure_barriers(&input_capture, &session, edge).await?;
    zone_set_state.store(zone_set, Ordering::Release);

    let eis_fd = input_capture
        .connect_to_eis(&session, Default::default())
        .await
        .map_err(portal_error)?;
    let stream = UnixStream::from(eis_fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    context
        .flush()
        .map_err(|error| LinuxInputError::LibeiInit(error.to_string()))?;
    let (_connection, mut eis_events) = context
        .handshake_tokio("edge-kvm controller", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|error| LinuxInputError::LibeiInit(error.to_string()))?;

    let mut activated_events = input_capture
        .receive_activated()
        .await
        .map_err(portal_error)?;
    let mut deactivated_events = input_capture
        .receive_deactivated()
        .await
        .map_err(portal_error)?;
    let mut disabled_events = input_capture
        .receive_disabled()
        .await
        .map_err(portal_error)?;
    let mut zones_changed_events = input_capture
        .receive_zones_changed()
        .await
        .map_err(portal_error)?;
    let mut closed_events = session.receive_closed().await.map_err(portal_error)?;
    let mut activation_id = None;
    let mut input_state = CaptureInputState::default();

    let _ = ready_tx.send(Ok(()));

    loop {
        tokio::select! {
            command = command_rx.recv() => {
                match command {
                    Some(CaptureCommand::Arm(response)) => {
                        let result = portal_command("Enable", input_capture.enable(&session, Default::default())).await;
                        let _ = response.send(result);
                    }
                    Some(CaptureCommand::Release { cursor_position, response }) => {
                        let result = if let Some(id) = activation_id.take() {
                            portal_command(
                                "Release",
                                input_capture.release(
                                    &session,
                                    ReleaseOptions::default()
                                        .set_activation_id(id)
                                        .set_cursor_position(cursor_position),
                                ),
                            )
                            .await
                        } else {
                            Ok(())
                        };
                        let _ = response.send(result);
                    }
                    Some(CaptureCommand::Disarm(response)) => {
                        activation_id = None;
                        let result = portal_command("Disable", input_capture.disable(&session, Default::default())).await;
                        let _ = response.send(result);
                    }
                    Some(CaptureCommand::Shutdown) | None => break,
                }
            }
            Some(activated) = activated_events.next() => {
                if let Some(id) = activated.activation_id() {
                    activation_id = Some(id);
                    if let Some((edge, normalized_position)) = activated
                        .barrier_id()
                        .and_then(|barrier| barrier_metadata(barrier, &barriers))
                        .map(|metadata| {
                            let normalized = normalized_cursor_position(
                                metadata,
                                activated.cursor_position(),
                            );
                            (metadata.edge, normalized)
                        })
                    {
                        let _ = event_tx.send(CaptureEvent::Activated {
                            activation_id: id,
                            edge,
                            normalized_position,
                        });
                    }
                }
            }
            Some(_deactivated) = deactivated_events.next() => {
                activation_id = None;
                input_state.clear();
                let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                let _ = event_tx.send(CaptureEvent::Deactivated);
            }
            Some(_disabled) = disabled_events.next() => {
                activation_id = None;
                input_state.clear();
                let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                let _ = event_tx.send(CaptureEvent::Deactivated);
            }
            Some(changed) = zones_changed_events.next() => {
                let invalidated = changed.zone_set().unwrap_or(zone_set);
                input_capture.disable(&session, Default::default()).await.map_err(portal_error)?;
                activation_id = None;
                input_state.clear();
                let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                let (current_zone_set, current_barriers) =
                    configure_barriers(&input_capture, &session, edge).await?;
                zone_set = current_zone_set;
                barriers = current_barriers;
                zone_set_state.store(zone_set, Ordering::Release);
                let _ = event_tx.send(CaptureEvent::LayoutChanged {
                    previous_zone_set: invalidated,
                    current_zone_set: zone_set,
                });
            }
            Some(_closed) = closed_events.next() => {
                input_state.clear();
                let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                return Err(LinuxInputError::LibeiInit(
                    "InputCapture portal session was revoked".to_string(),
                ));
            }
            event = eis_events.next() => {
                let Some(event) = event else {
                    input_state.clear();
                    let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                    return Err(LinuxInputError::LibeiInit("EIS event stream closed".to_string()));
                };
                let event = event.map_err(|error| LinuxInputError::LibeiInit(error.to_string()))?;
                match handle_eis_event(&context, &mut input_state, event)? {
                    EisAction::None => {}
                    EisAction::Input(input) => {
                        let _ = event_tx.send(CaptureEvent::Input(input));
                    }
                    EisAction::EmergencyRelease => {
                        let _ = event_tx.send(CaptureEvent::Input(InputEvent::AllKeysUp));
                        if let Some(id) = activation_id.take() {
                            input_capture
                                .release(
                                    &session,
                                    ReleaseOptions::default().set_activation_id(id),
                                )
                                .await
                                .map_err(portal_error)?;
                        }
                        let _ = event_tx.send(CaptureEvent::EmergencyReleased);
                    }
                }
            }
        }
    }

    let _ = input_capture.disable(&session, Default::default()).await;
    let _ = session.close().await;
    Ok(())
}

async fn portal_command<T>(
    name: &'static str,
    command: impl Future<Output = std::result::Result<T, PortalError>>,
) -> Result<T> {
    time::timeout(PORTAL_COMMAND_TIMEOUT, command)
        .await
        .map_err(|_| {
            LinuxInputError::LibeiInit(format!(
                "InputCapture portal {name} call timed out after two seconds"
            ))
        })?
        .map_err(portal_error)
}

async fn create_and_start_session(
    input_capture: &InputCapture,
    capabilities: reis::enumflags2::BitFlags<Capabilities>,
) -> Result<(
    Session<InputCapture>,
    reis::enumflags2::BitFlags<Capabilities>,
)> {
    match input_capture.create_session2(Default::default()).await {
        Ok(session) => {
            let response = input_capture
                .start(
                    &session,
                    None,
                    StartOptions::default().set_capabilities(capabilities),
                )
                .await
                .map_err(portal_error)?
                .response()
                .map_err(portal_error)?;
            Ok((session, response.capabilities()))
        }
        Err(PortalError::RequiresVersion(_, _)) => input_capture
            .create_session(
                None,
                CreateSessionOptions::default().set_capabilities(capabilities),
            )
            .await
            .map_err(portal_error),
        Err(error) => Err(portal_error(error)),
    }
}

async fn configure_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    edge: Edge,
) -> Result<(u32, HashMap<u32, BarrierMetadata>)> {
    let zones = input_capture
        .zones(session, Default::default())
        .await
        .map_err(portal_error)?
        .response()
        .map_err(portal_error)?;
    if zones.regions().is_empty() {
        return Err(LinuxInputError::LibeiInit(
            "InputCapture portal returned no pointer-barrier zones".to_string(),
        ));
    }

    let requested = zones
        .regions()
        .iter()
        .enumerate()
        .map(|(index, region)| {
            let id = NonZeroU32::new((index + 1) as u32).expect("barrier id is non-zero");
            let x = region.x_offset();
            let y = region.y_offset();
            let width = region.width();
            let height = region.height();
            let right = x.saturating_add(width as i32);
            let bottom = y.saturating_add(height as i32);
            let position = match edge {
                Edge::Left => (x, y, x, bottom.saturating_sub(1)),
                Edge::Right => (right, y, right, bottom.saturating_sub(1)),
                Edge::Top => (x, y, right.saturating_sub(1), y),
                Edge::Bottom => (x, bottom, right.saturating_sub(1), bottom),
            };
            (
                id,
                position,
                BarrierMetadata {
                    edge,
                    x,
                    y,
                    width,
                    height,
                },
            )
        })
        .collect::<Vec<_>>();
    let portal_barriers = requested
        .iter()
        .map(|(id, position, _)| Barrier::new(*id, *position))
        .collect::<Vec<_>>();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &portal_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await
        .map_err(portal_error)?
        .response()
        .map_err(portal_error)?;
    let failed = response.failed_barriers();
    let accepted = requested
        .into_iter()
        .filter(|(id, _, _)| !failed.contains(id))
        .map(|(id, _, metadata)| (id.get(), metadata))
        .collect::<HashMap<_, _>>();
    if accepted.is_empty() {
        return Err(LinuxInputError::LibeiInit(format!(
            "InputCapture portal rejected every requested {edge:?} pointer barrier"
        )));
    }
    Ok((zones.zone_set(), accepted))
}

fn barrier_metadata(
    barrier: ActivatedBarrier,
    barriers: &HashMap<u32, BarrierMetadata>,
) -> Option<BarrierMetadata> {
    match barrier {
        ActivatedBarrier::Barrier(id) => barriers.get(&id.get()).copied(),
        ActivatedBarrier::UnknownBarrier => None,
    }
}

fn normalized_cursor_position(
    barrier: BarrierMetadata,
    cursor_position: Option<(f32, f32)>,
) -> f32 {
    let Some((cursor_x, cursor_y)) = cursor_position else {
        return 0.5;
    };
    let (position, origin, extent) = match barrier.edge {
        Edge::Left | Edge::Right => (cursor_y, barrier.y as f32, barrier.height),
        Edge::Top | Edge::Bottom => (cursor_x, barrier.x as f32, barrier.width),
    };
    if extent <= 1 {
        return 0.0;
    }
    ((position - origin) / (extent - 1) as f32).clamp(0.0, 1.0)
}

#[derive(Debug, Default)]
struct CaptureInputState {
    pressed_keys: BTreeSet<u16>,
}

impl CaptureInputState {
    fn clear(&mut self) {
        self.pressed_keys.clear();
    }

    fn update_key(&mut self, evdev_code: u16, down: bool) -> bool {
        if down {
            self.pressed_keys.insert(evdev_code);
        } else {
            self.pressed_keys.remove(&evdev_code);
        }
        down && evdev_code == KEY_PAUSE
            && self
                .pressed_keys
                .iter()
                .any(|key| CONTROL_KEYS.contains(key))
            && self.pressed_keys.iter().any(|key| ALT_KEYS.contains(key))
    }
}

const CONTROL_KEYS: &[u16] = &[29, 97];
const ALT_KEYS: &[u16] = &[56, 100];
const KEY_PAUSE: u16 = 119;

#[derive(Debug, Clone, PartialEq)]
enum EisAction {
    None,
    Input(InputEvent),
    EmergencyRelease,
}

fn handle_eis_event(
    context: &ei::Context,
    state: &mut CaptureInputState,
    event: EiEvent,
) -> Result<EisAction> {
    let action = match event {
        EiEvent::SeatAdded(event) => {
            event.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            context
                .flush()
                .map_err(|error| LinuxInputError::LibeiInit(error.to_string()))?;
            EisAction::None
        }
        EiEvent::PointerMotion(event) => EisAction::Input(InputEvent::PointerMotion {
            dx: f64::from(event.dx),
            dy: f64::from(event.dy),
        }),
        EiEvent::Button(event) => mouse_button(event.button)
            .map(|button| {
                EisAction::Input(InputEvent::PointerButton {
                    button,
                    down: event.state == ButtonState::Press,
                })
            })
            .unwrap_or(EisAction::None),
        EiEvent::ScrollDiscrete(event) => EisAction::Input(InputEvent::PointerWheel {
            x: f64::from(event.discrete_dx) / 120.0,
            y: -f64::from(event.discrete_dy) / 120.0,
        }),
        EiEvent::ScrollDelta(event) => EisAction::Input(InputEvent::PointerWheel {
            x: f64::from(event.dx) / 120.0,
            y: -f64::from(event.dy) / 120.0,
        }),
        EiEvent::KeyboardKey(event) => {
            let Some(evdev_code) = u16::try_from(event.key).ok() else {
                return Ok(EisAction::None);
            };
            let down = event.state == KeyState::Press;
            if state.update_key(evdev_code, down) {
                state.clear();
                EisAction::EmergencyRelease
            } else {
                EisAction::Input(InputEvent::Key { evdev_code, down })
            }
        }
        EiEvent::DeviceStopEmulating(_) | EiEvent::Disconnected(_) => {
            state.clear();
            EisAction::Input(InputEvent::AllKeysUp)
        }
        _ => EisAction::None,
    };
    Ok(action)
}

fn mouse_button(button: u32) -> Option<MouseButton> {
    match button {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x115 => Some(MouseButton::Forward),
        0x116 => Some(MouseButton::Back),
        _ => None,
    }
}

fn portal_error(error: PortalError) -> LinuxInputError {
    LinuxInputError::LibeiInit(format!("InputCapture portal: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_activation_position_on_all_edges() {
        let vertical = BarrierMetadata {
            edge: Edge::Left,
            x: 100,
            y: 200,
            width: 1920,
            height: 1080,
        };
        assert!((normalized_cursor_position(vertical, Some((100.0, 739.5))) - 0.5).abs() < 0.001);

        let horizontal = BarrierMetadata {
            edge: Edge::Top,
            ..vertical
        };
        assert!(
            (normalized_cursor_position(horizontal, Some((1059.5, 200.0))) - 0.5).abs() < 0.001
        );
    }

    #[test]
    fn maps_linux_button_codes() {
        assert_eq!(mouse_button(0x110), Some(MouseButton::Left));
        assert_eq!(mouse_button(0x116), Some(MouseButton::Back));
        assert_eq!(mouse_button(0xffff), None);
    }

    #[test]
    fn emergency_chord_requires_control_alt_and_pause() {
        let mut state = CaptureInputState::default();
        assert!(!state.update_key(29, true));
        assert!(!state.update_key(56, true));
        assert!(state.update_key(KEY_PAUSE, true));

        state.clear();
        assert!(!state.update_key(29, true));
        assert!(!state.update_key(KEY_PAUSE, true));
    }
}
