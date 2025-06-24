use wayland_client::{EventQueue, WEnum};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use tracing::debug;

use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};

use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
    },
};

use wayland_client::protocol::{wl_compositor::WlCompositor, wl_surface::WlSurface};

use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use std::sync::{Arc, RwLock};

// Replace the layer shell imports with xdg_shell imports
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use wayland_protocols::{
    ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options,
    wp::viewporter::client::wp_viewporter::WpViewporter,
};

use image::ColorType;
use memmap2::MmapMut;

use std::os::fd::{AsFd, AsRawFd};

use std::{
    fs::File,
    time::{SystemTime, UNIX_EPOCH},
};

use std::{ops::Deref, os::fd::OwnedFd};

use std::io;
use thiserror::Error;
use wayland_client::{
    ConnectError, DispatchError,
    globals::{BindError, GlobalError},
};

use crate::dispatch::{DMABUFState, FrameState, OutputCaptureState, XdgShellState};
use crate::output::OutputInfo;
use crate::region::{LogicalRegion, Position, Region, Size};
use crate::{WayshotBase, WayshotError}; // Add this import

/// Image view means what part to use
/// When use the project, every time you will get a picture of the full screen,
/// and when you do area screenshot, This lib will also provide you with the view of the selected
/// part
#[derive(Debug, Clone)]
pub struct ImageViewInfo {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub region: Region,
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub(crate) struct CaptureOutputData {
    pub(crate) output: WlOutput,
    pub(crate) buffer: WlBuffer,
    pub(crate) real_width: u32,
    pub(crate) real_height: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_bytes: u32,
    pub(crate) stride: u32,
    pub(crate) transform: wl_output::Transform,
    pub(crate) frame_format: Format,
    pub(crate) screen_position: Position,
}

#[derive(Debug, Clone)]
pub struct TopLevel {
    pub(crate) handle: ExtForeignToplevelHandleV1,
    pub(crate) title: String,
}

impl TopLevel {
    pub(crate) fn new(handle: ExtForeignToplevelHandleV1) -> Self {
        Self {
            handle,
            title: "".to_string(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn handle(&self) -> &ExtForeignToplevelHandleV1 {
        &self.handle
    }
}

#[derive(Debug, Default)]
pub(crate) struct FrameInfo {
    pub(crate) buffer_size: Option<Size>,
    pub(crate) shm_format: Option<WEnum<Format>>,
}

impl FrameInfo {
    pub(crate) fn size(&self) -> Size {
        self.buffer_size.clone().expect("not inited")
    }

    pub(crate) fn format(&self) -> WEnum<Format> {
        self.shm_format.clone().expect("Not inited")
    }
}

pub(crate) struct CaptureInfo {
    pub(crate) transform: wl_output::Transform,
    pub(crate) state: FrameState,
}

impl CaptureInfo {
    pub(crate) fn new() -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            transform: wl_output::Transform::Normal,
            state: FrameState::Pending,
        }))
    }

    pub(crate) fn transform(&self) -> wl_output::Transform {
        self.transform
    }
    pub(crate) fn state(&self) -> FrameState {
        self.state
    }
}

pub trait AreaSelectCallback {
    fn slurp(self, state: &WayshotConnection) -> Result<Region, WayshotError>;
}

impl<F> AreaSelectCallback for F
where
    F: Fn(&WayshotConnection) -> Result<Region, WayshotError>,
{
    fn slurp(self, state: &WayshotConnection) -> Result<Region, WayshotError> {
        self(state)
    }
}
impl AreaSelectCallback for Region {
    fn slurp(self, _state: &WayshotConnection) -> Result<Region, WayshotError> {
        Ok(self)
    }
}

use std::collections::HashSet;

/// Describe the capture option
/// Now this library provide two options
/// [CaptureOption::PaintCursors] and [CaptureOption::None]
/// It decides whether cursor will be shown
#[derive(Debug, Clone, Copy)]
pub enum CaptureOption {
    PaintCursors,
    None,
}

impl From<CaptureOption> for Options {
    fn from(val: CaptureOption) -> Self {
        match val {
            CaptureOption::None => Options::empty(),
            CaptureOption::PaintCursors => Options::PaintCursors,
        }
    }
}

pub(crate) struct AreaShotInfo {
    pub(crate) data: CaptureOutputData,
    pub(crate) mem_file: File,
}

impl AreaShotInfo {
    pub(crate) fn in_this_screen(
        &self,
        Region {
            position: point, ..
        }: Region,
    ) -> bool {
        let CaptureOutputData {
            real_width,
            real_height,
            screen_position: Position { x, y },
            ..
        } = self.data;
        if point.y < y
            || point.x < x
            || point.x > x + real_width as i32
            || point.y > y + real_height as i32
        {
            return false;
        }
        true
    }
    pub(crate) fn clip_area(&self, region: Region) -> Option<Region> {
        if !self.in_this_screen(region) {
            return None;
        }
        let CaptureOutputData {
            real_width,
            real_height,
            width,
            height,
            screen_position,
            ..
        } = self.data;
        let Region {
            position: point,
            size,
        } = region;
        let relative_point = point - screen_position;
        let position = Position {
            x: (relative_point.x as f64 * width as f64 / real_width as f64) as i32,
            y: (relative_point.y as f64 * height as f64 / real_height as f64) as i32,
        };

        Some(Region {
            position,
            size: Size {
                width: (size.width as f64 * width as f64 / real_width as f64) as u32,
                height: (size.height as f64 * height as f64 / real_height as f64) as u32,
            },
        })
    }
}
#[derive(Debug)]
pub struct HaruhiShotBase<T> {
    pub toplevels: Vec<TopLevel>,
    pub img_copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    pub output_image_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub shm: Option<WlShm>,
    pub qh: Option<QueueHandle<T>>,
    pub event_queue: Option<EventQueue<T>>,
}

use nix::{
    fcntl,
    sys::{memfd, mman, stat},
    unistd,
};

/// capture_output_frame.
pub(crate) fn ext_create_shm_fd() -> std::io::Result<OwnedFd> {
    // Only try memfd on linux and freebsd.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    loop {
        // Create a file that closes on successful execution and seal it's operations.
        match memfd::memfd_create(
            c"wayshot",
            memfd::MFdFlags::MFD_CLOEXEC | memfd::MFdFlags::MFD_ALLOW_SEALING,
        ) {
            Ok(fd) => {
                // This is only an optimization, so ignore errors.
                // F_SEAL_SRHINK = File cannot be reduced in size.
                // F_SEAL_SEAL = Prevent further calls to fcntl().
                let _ = fcntl::fcntl(
                    fd.as_fd(),
                    fcntl::F_ADD_SEALS(
                        fcntl::SealFlag::F_SEAL_SHRINK | fcntl::SealFlag::F_SEAL_SEAL,
                    ),
                );
                return Ok(fd);
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::ENOSYS) => break,
            Err(errno) => return Err(std::io::Error::from(errno)),
        }
    }

    // Fallback to using shm_open.
    let sys_time = SystemTime::now();
    let mut mem_file_handle = format!(
        "/wayshot-{}",
        sys_time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
    );
    loop {
        match mman::shm_open(
            // O_CREAT = Create file if does not exist.
            // O_EXCL = Error if create and file exists.
            // O_RDWR = Open for reading and writing.
            // O_CLOEXEC = Close on successful execution.
            // S_IRUSR = Set user read permission bit .
            // S_IWUSR = Set user write permission bit.
            mem_file_handle.as_str(),
            fcntl::OFlag::O_CREAT
                | fcntl::OFlag::O_EXCL
                | fcntl::OFlag::O_RDWR
                | fcntl::OFlag::O_CLOEXEC,
            stat::Mode::S_IRUSR | stat::Mode::S_IWUSR,
        ) {
            Ok(fd) => match mman::shm_unlink(mem_file_handle.as_str()) {
                Ok(_) => return Ok(fd),
                Err(errno) => match unistd::close(fd.as_raw_fd()) {
                    Ok(_) => return Err(std::io::Error::from(errno)),
                    Err(errno) => return Err(std::io::Error::from(errno)),
                },
            },
            Err(nix::errno::Errno::EEXIST) => {
                // If a file with that handle exists then change the handle
                mem_file_handle = format!(
                    "/wayshot-{}",
                    sys_time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
                );
                continue;
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(errno) => return Err(std::io::Error::from(errno)),
        }
    }
}

use crate::WayshotConnection;
