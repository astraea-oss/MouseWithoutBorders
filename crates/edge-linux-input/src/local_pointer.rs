use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::Read,
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
    time::Duration,
};

use crate::{LinuxInputError, Result};
use tokio::{
    io::unix::AsyncFd,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time,
};

const DEVICE_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const ACTIVITY_CHANNEL_CAPACITY: usize = 8;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const BTN_MOUSE: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
const BTN_TOUCH: u16 = 0x14a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPointerKind {
    Mouse,
    Touchpad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPointerActivity {
    pub kind: LocalPointerKind,
}

pub struct LocalPointerActivityMonitor {
    receiver: mpsc::Receiver<LocalPointerActivity>,
    manager: JoinHandle<()>,
}

impl LocalPointerActivityMonitor {
    pub async fn connect(wake_on_mouse: bool, wake_on_touchpad: bool) -> Result<Self> {
        let devices = discover_devices(wake_on_mouse, wake_on_touchpad)?;
        if devices.is_empty() {
            return Err(LinuxInputError::LocalPointerMonitor(
                "no enabled physical mouse or touchpad devices found".to_string(),
            ));
        }

        let mut initial = Vec::new();
        let mut failures = Vec::new();
        for device in devices {
            match open_device(&device) {
                Ok(fd) => initial.push((device, fd)),
                Err(err) => {
                    failures.push(format!("{}: {err}", device.path.display()));
                    tracing::warn!(device = %device.path.display(), %err, "failed to monitor local pointer device")
                }
            }
        }
        if initial.is_empty() {
            return Err(LinuxInputError::LocalPointerMonitor(format!(
                "cannot read any physical pointer devices; install packaging/udev/72-edge-kvm-pointer.rules: {}",
                failures.join("; ")
            )));
        }

        let (sender, receiver) = mpsc::channel(ACTIVITY_CHANNEL_CAPACITY);
        let manager = tokio::spawn(run_device_manager(
            wake_on_mouse,
            wake_on_touchpad,
            initial,
            sender,
        ));
        Ok(Self { receiver, manager })
    }

    pub async fn recv(&mut self) -> Option<LocalPointerActivity> {
        self.receiver.recv().await
    }
}

impl Drop for LocalPointerActivityMonitor {
    fn drop(&mut self) {
        self.manager.abort();
    }
}

#[derive(Debug, Clone)]
struct PointerDevice {
    path: PathBuf,
    kind: LocalPointerKind,
    major: u32,
    minor: u32,
}

impl PointerDevice {
    fn key(&self) -> (u32, u32) {
        (self.major, self.minor)
    }
}

fn open_device(device: &PointerDevice) -> Result<OwnedFd> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&device.path)?;
    Ok(file.into())
}

fn discover_devices(wake_on_mouse: bool, wake_on_touchpad: bool) -> Result<Vec<PointerDevice>> {
    let mut enumerator = udev::Enumerator::new()
        .map_err(|err| LinuxInputError::LocalPointerMonitor(err.to_string()))?;
    enumerator
        .match_subsystem("input")
        .map_err(|err| LinuxInputError::LocalPointerMonitor(err.to_string()))?;

    let mut devices = Vec::new();
    for device in enumerator
        .scan_devices()
        .map_err(|err| LinuxInputError::LocalPointerMonitor(err.to_string()))?
    {
        let kind = if wake_on_touchpad
            && device.property_value("ID_INPUT_TOUCHPAD") == Some(std::ffi::OsStr::new("1"))
        {
            LocalPointerKind::Touchpad
        } else if wake_on_mouse
            && device.property_value("ID_INPUT_MOUSE") == Some(std::ffi::OsStr::new("1"))
        {
            LocalPointerKind::Mouse
        } else {
            continue;
        };
        let Some(path) = device.devnode().map(PathBuf::from) else {
            continue;
        };
        if !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"event"))
        {
            continue;
        }
        let metadata = std::fs::metadata(&path)?;
        let dev = metadata.rdev();
        devices.push(PointerDevice {
            path,
            kind,
            major: libc::major(dev) as u32,
            minor: libc::minor(dev) as u32,
        });
    }
    Ok(devices)
}

async fn run_device_manager(
    wake_on_mouse: bool,
    wake_on_touchpad: bool,
    initial: Vec<(PointerDevice, OwnedFd)>,
    sender: mpsc::Sender<LocalPointerActivity>,
) {
    let mut readers = JoinSet::new();
    let mut active = HashSet::new();
    for (device, fd) in initial {
        active.insert(device.key());
        spawn_reader(&mut readers, device, fd, sender.clone());
    }

    let mut scan = time::interval(DEVICE_SCAN_INTERVAL);
    loop {
        tokio::select! {
            result = readers.join_next(), if !readers.is_empty() => {
                let Some(result) = result else { continue };
                match result {
                    Ok((key, Ok(()))) => {
                        active.remove(&key);
                        tracing::debug!(?key, "local pointer reader stopped");
                        if active.is_empty() {
                            tracing::warn!("all local pointer readers stopped");
                            return;
                        }
                    }
                    Ok((key, Err(err))) => {
                        active.remove(&key);
                        tracing::warn!(?key, %err, "local pointer reader failed");
                        if active.is_empty() {
                            tracing::warn!("all local pointer readers failed");
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, "local pointer reader task failed");
                        continue;
                    }
                }
            }
            _ = scan.tick() => {
                let Ok(devices) = discover_devices(wake_on_mouse, wake_on_touchpad) else {
                    continue;
                };
                for device in devices {
                    if active.contains(&device.key()) {
                        continue;
                    }
                    match open_device(&device) {
                        Ok(fd) => {
                            active.insert(device.key());
                            spawn_reader(&mut readers, device, fd, sender.clone());
                        }
                        Err(err) => tracing::debug!(device = %device.path.display(), %err, "local pointer device is not available"),
                    }
                }
            }
        }
    }
}

fn spawn_reader(
    readers: &mut JoinSet<((u32, u32), Result<()>)>,
    device: PointerDevice,
    fd: OwnedFd,
    sender: mpsc::Sender<LocalPointerActivity>,
) {
    readers.spawn(async move {
        let key = device.key();
        let result = read_device(device, fd, sender).await;
        (key, result)
    });
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KernelInputEvent {
    time: libc::timeval,
    event_type: u16,
    code: u16,
    value: i32,
}

async fn read_device(
    device: PointerDevice,
    fd: OwnedFd,
    sender: mpsc::Sender<LocalPointerActivity>,
) -> Result<()> {
    let async_fd = AsyncFd::new(std::fs::File::from(fd))?;
    let mut events = [KernelInputEvent::default(); 32];
    loop {
        let mut ready = async_fd.readable().await?;
        let count = match ready.try_io(|inner| read_events(inner.get_ref(), &mut events)) {
            Ok(result) => result?,
            Err(_) => continue,
        };
        if count == 0 {
            return Ok(());
        }
        if events[..count]
            .iter()
            .any(|event| event_is_activity(device.kind, event))
        {
            let _ = sender.try_send(LocalPointerActivity { kind: device.kind });
        }
    }
}

fn read_events(file: &std::fs::File, events: &mut [KernelInputEvent]) -> std::io::Result<usize> {
    let byte_len = std::mem::size_of_val(events);
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(events.as_mut_ptr().cast::<u8>(), byte_len) };
    let mut file = file;
    let read = file.read(bytes)?;
    Ok(read / std::mem::size_of::<KernelInputEvent>())
}

fn event_is_activity(kind: LocalPointerKind, event: &KernelInputEvent) -> bool {
    match kind {
        LocalPointerKind::Mouse => {
            event.event_type == EV_REL
                || (event.event_type == EV_KEY && (BTN_MOUSE..=BTN_TASK).contains(&event.code))
        }
        LocalPointerKind::Touchpad => {
            event.event_type == EV_ABS || (event.event_type == EV_KEY && event.code == BTN_TOUCH)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event_type: u16, code: u16) -> KernelInputEvent {
        KernelInputEvent {
            event_type,
            code,
            value: 1,
            ..Default::default()
        }
    }

    #[test]
    fn classifies_only_pointer_activity() {
        assert!(event_is_activity(
            LocalPointerKind::Mouse,
            &event(EV_REL, 0)
        ));
        assert!(event_is_activity(
            LocalPointerKind::Mouse,
            &event(EV_KEY, BTN_MOUSE)
        ));
        assert!(!event_is_activity(
            LocalPointerKind::Mouse,
            &event(EV_KEY, 30)
        ));
        assert!(event_is_activity(
            LocalPointerKind::Touchpad,
            &event(EV_ABS, 0)
        ));
        assert!(event_is_activity(
            LocalPointerKind::Touchpad,
            &event(EV_KEY, BTN_TOUCH)
        ));
        assert!(!event_is_activity(
            LocalPointerKind::Touchpad,
            &event(EV_KEY, 30)
        ));
    }

    #[tokio::test]
    #[ignore = "requires active-user read access to Linux pointer devices"]
    async fn connects_to_active_session_pointer_devices() {
        if let Err(err) = LocalPointerActivityMonitor::connect(true, true).await {
            panic!("{err}");
        }
    }
}
