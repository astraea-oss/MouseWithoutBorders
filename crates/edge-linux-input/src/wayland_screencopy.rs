//! Window-region capture with wlr-screencopy into a reusable shared-memory
//! buffer: no screenshot tool, no image encoding, no pipe. Used by the phone's
//! video remote view; frames go straight to the hardware encoder.
//!
//! With damage, a capture waits until something in the region changes, so a
//! still screen costs nothing. Blocking; run it off the async runtime.
use std::{
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{LinuxInputError, Result};

/// One captured region, tightly packed (`width * 4` bytes per row), top row
/// first. `format` names it for GStreamer: `BGRx`, `BGRA`, `RGBx` or `RGBA`.
#[derive(Debug, Clone)]
pub struct CapturedRegion {
    pub width: u32,
    pub height: u32,
    pub format: &'static str,
    pub data: Vec<u8>,
}

pub struct ScreenCopy {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1,
    shm: wl_shm::WlShm,
    buffer: Option<ShmBuffer>,
}

// The Wayland objects are only used from the thread that owns this.
unsafe impl Send for ScreenCopy {}

#[derive(Default)]
struct State {
    /// Every output, with its name once wl_output v4 has sent it.
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    frame: FrameState,
}

#[derive(Default)]
struct FrameState {
    /// (format, width, height, stride) of the offered shared-memory buffer.
    shm: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

struct ShmBuffer {
    _file: File,
    map: *mut u8,
    len: usize,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    key: (wl_shm::Format, u32, u32, u32),
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

fn init_error(error: impl std::fmt::Display) -> LinuxInputError {
    LinuxInputError::HyprlandVirtualInputInit(format!("screen capture: {error}"))
}

impl ScreenCopy {
    pub fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().map_err(init_error)?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection).map_err(init_error)?;
        let qh = queue.handle();
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .map_err(|e| init_error(format!("no zwlr_screencopy_manager_v1 v3: {e}")))?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| init_error(format!("no wl_shm: {e}")))?;
        let mut state = State::default();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" && global.version >= 4 {
                let index = state.outputs.len();
                let output = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    4,
                    &qh,
                    index,
                );
                state.outputs.push((output, None));
            }
        }
        queue.roundtrip(&mut state).map_err(init_error)?;
        Ok(Self {
            connection,
            queue,
            state,
            manager,
            shm,
            buffer: None,
        })
    }

    /// Captures `width`×`height` at (`x`, `y`) of output `name`, in that
    /// output's logical coordinates (the buffer is in its physical pixels).
    /// With `wait_for_damage`, waits until the region changes; `Ok(None)`
    /// means nothing changed within `timeout`.
    pub fn capture(
        &mut self,
        name: &str,
        (x, y, width, height): (i32, i32, i32, i32),
        wait_for_damage: bool,
        timeout: Duration,
    ) -> Result<Option<CapturedRegion>> {
        let output = self
            .state
            .outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(name))
            .map(|(o, _)| o.clone())
            .ok_or_else(|| init_error(format!("no output named {name}")))?;
        let qh = self.queue.handle();
        self.state.frame = FrameState::default();
        let frame = self
            .manager
            .capture_output_region(0, &output, x, y, width, height, &qh, ());
        let deadline = Instant::now() + timeout.max(Duration::from_millis(250));
        let offered = self.wait(deadline, |f| f.buffer_done || f.failed);
        let Some((format, w, h, stride)) = self
            .state
            .frame
            .shm
            .filter(|_| offered && !self.state.frame.failed)
        else {
            frame.destroy();
            return Err(init_error("the compositor offered no shared-memory buffer"));
        };
        let gst_format = match format {
            wl_shm::Format::Xrgb8888 => "BGRx",
            wl_shm::Format::Argb8888 => "BGRA",
            wl_shm::Format::Xbgr8888 => "RGBx",
            wl_shm::Format::Abgr8888 => "RGBA",
            other => {
                frame.destroy();
                return Err(init_error(format!("unsupported capture format {other:?}")));
            }
        };
        let key = (format, w, h, stride);
        if self.buffer.as_ref().is_none_or(|b| b.key != key) {
            self.buffer = None;
            self.buffer = Some(self.allocate(key, &qh)?);
        }
        let buffer = self.buffer.as_ref().unwrap();
        if wait_for_damage {
            frame.copy_with_damage(&buffer.buffer);
        } else {
            frame.copy(&buffer.buffer);
        }
        let deadline = Instant::now() + timeout;
        let done = self.wait(deadline, |f| f.ready || f.failed);
        frame.destroy();
        let _ = self.connection.flush();
        if !done {
            return Ok(None);
        }
        if self.state.frame.failed {
            return Err(init_error("the compositor could not copy the region"));
        }
        let buffer = self.buffer.as_ref().unwrap();
        let row = w as usize * 4;
        let pixels = unsafe { std::slice::from_raw_parts(buffer.map, buffer.len) };
        let mut data = Vec::with_capacity(row * h as usize);
        let rows: Box<dyn Iterator<Item = usize>> = if self.state.frame.y_invert {
            Box::new((0..h as usize).rev())
        } else {
            Box::new(0..h as usize)
        };
        for line in rows {
            let start = line * stride as usize;
            data.extend_from_slice(&pixels[start..start + row]);
        }
        Ok(Some(CapturedRegion {
            width: w,
            height: h,
            format: gst_format,
            data,
        }))
    }

    fn allocate(
        &self,
        key: (wl_shm::Format, u32, u32, u32),
        qh: &QueueHandle<State>,
    ) -> Result<ShmBuffer> {
        let (format, width, height, stride) = key;
        let len = stride as usize * height as usize;
        let name = std::ffi::CString::new("edge-kvm-capture").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(LinuxInputError::Io(std::io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(LinuxInputError::Io(std::io::Error::last_os_error()));
        }
        let pool = self.shm.create_pool(file.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(ShmBuffer {
            _file: file,
            map: map.cast(),
            len,
            pool,
            buffer,
            key,
        })
    }

    /// Dispatches events until `done` holds or `deadline` passes.
    fn wait(&mut self, deadline: Instant, done: impl Fn(&FrameState) -> bool) -> bool {
        loop {
            if self.queue.dispatch_pending(&mut self.state).is_err() {
                return false;
            }
            if done(&self.state.frame) {
                return true;
            }
            let _ = self.connection.flush();
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let mut poll = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            if unsafe { libc::poll(&mut poll, 1, millis) } > 0 {
                let _ = guard.read();
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = state.outputs.get_mut(*index)
        {
            output.1 = Some(name);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let frame = &mut state.frame;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => frame.shm = Some((format, width, height, stride)),
            Event::Flags {
                flags: WEnum::Value(flags),
            } => frame.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::BufferDone => frame.buffer_done = true,
            Event::Ready { .. } => frame.ready = true,
            Event::Failed => frame.failed = true,
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($interface:ty),*) => {$(
        impl Dispatch<$interface, ()> for State {
            fn event(_: &mut Self, _: &$interface, _: <$interface as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    ZwlrScreencopyManagerV1,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a running wlroots compositor"]
    fn captures_a_region_and_then_waits_for_damage() {
        let mut copy = ScreenCopy::connect().unwrap();
        let name = copy.state.outputs[0].1.clone().unwrap();
        let started = Instant::now();
        let first = copy
            .capture(&name, (0, 0, 800, 600), false, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        println!(
            "{name}: {}x{} {} in {:?}",
            first.width,
            first.height,
            first.format,
            started.elapsed()
        );
        assert_eq!(
            first.data.len(),
            first.width as usize * first.height as usize * 4
        );
        for _ in 0..5 {
            let started = Instant::now();
            let next = copy
                .capture(&name, (0, 0, 800, 600), false, Duration::from_secs(1))
                .unwrap();
            println!(
                "again: {:?} in {:?}",
                next.map(|n| n.data.len()),
                started.elapsed()
            );
        }
        let started = Instant::now();
        let damaged = copy
            .capture(&name, (0, 0, 800, 600), true, Duration::from_millis(300))
            .unwrap();
        println!(
            "with damage: {} after {:?}",
            if damaged.is_some() {
                "changed"
            } else {
                "no change"
            },
            started.elapsed()
        );
    }
}
