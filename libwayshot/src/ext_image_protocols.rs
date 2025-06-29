use wayland_client::WEnum;

use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::{self, WlOutput},
    wl_shm::Format,
};

use std::sync::{Arc, RwLock};

use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options;

use image::ColorType;

use std::os::fd::{AsFd, AsRawFd};

use std::{
    fs::File,
    time::{SystemTime, UNIX_EPOCH},
};

use std::os::fd::OwnedFd;

use crate::WayshotConnection;
use crate::WayshotError; // Removed WayshotBase import
use crate::dispatch::FrameState;
use crate::region::{Position, Region, Size, LogicalRegion};

use nix::{
    fcntl,
    sys::{memfd, mman, stat},
    unistd,
};

/// Image view means what part to use
/// When use the project, every time you will get a picture of the full screen,
/// and when you do area screenshot, This lib will also provide you with the view of the selected
/// part
#[derive(Debug, Clone)]
pub struct ImageViewInfo {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType, // add this field
    pub region: Region,
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub(crate) struct FrameInfo {
    pub(crate) format: Format,
    pub(crate) size: Size,
    pub(crate) stride: u32,
}

impl FrameInfo {
    pub(crate) fn byte_size(&self) -> u32 {
        self.stride * self.size.height
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureOutputData {
    pub(crate) output: WlOutput,
    pub(crate) buffer: WlBuffer,

    pub(crate) frame_info: FrameInfo,

    pub(crate) color_type: ColorType, // added here


	pub(crate) transform: wl_output::Transform,
	pub(crate) logical_region: LogicalRegion, // replaces width, height, screen_position
	pub(crate) physical_size: Size, // replaced real_width/real_height

}

#[derive(Debug, Clone)]
pub struct TopLevel {
    pub(crate) handle: ExtForeignToplevelHandleV1,
    pub(crate) title: String,
	pub(crate) app_id: String,
	pub(crate) identifier: String,
	pub(crate) active: bool,
}

impl TopLevel {
    pub(crate) fn new(handle: ExtForeignToplevelHandleV1) -> Self {
        Self {
            handle,
			title: "".to_owned(),
			app_id: "".to_owned(),
			identifier: "".to_owned(),
			active: true,
		}
    }

    pub fn title(&self) -> &str {
        &self.title
    }

	pub fn app_id(&self) -> &str {
		&self.app_id
	}

	pub fn identifier(&self) -> &str {
		&self.identifier
	}

	pub fn id_and_title(&self) -> String {
		format!("{} {}", self.app_id(), self.title())
	}

    pub fn handle(&self) -> &ExtForeignToplevelHandleV1 {
        &self.handle
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

#[allow(unused)]
#[derive(Debug, Clone)]
struct CaptureTopLevelData {
	buffer: WlBuffer,
	width: u32,
	height: u32,
	frame_bytes: u32,
	stride: u32,
	frame_format: Format,
	transform: wl_output::Transform,
}

pub trait AreaSelectCallback {
    fn screenshot(self, state: &WayshotConnection) -> Result<Region, WayshotError>;
}

impl<F> AreaSelectCallback for F
where
    F: Fn(&WayshotConnection) -> Result<Region, WayshotError>,
{
    fn screenshot(self, state: &WayshotConnection) -> Result<Region, WayshotError> {
        self(state)
    }
}
impl AreaSelectCallback for Region {
    fn screenshot(self, _state: &WayshotConnection) -> Result<Region, WayshotError> {
        Ok(self)
    }
}

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
            physical_size,
            logical_region,
            ..
        } = &self.data;
        let Position { x, y } = logical_region.inner.position;
        if point.y < y
            || point.x < x
            || point.x > x + physical_size.width as i32
            || point.y > y + physical_size.height as i32
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
            physical_size,
            logical_region,
            ..
        } = &self.data;
        let width = logical_region.inner.size.width;
        let height = logical_region.inner.size.height;
        let screen_position = logical_region.inner.position;
        let Region {
            position: point,
            size,
        } = region;
        let relative_point = point - screen_position;
        let position = Position {
            x: (relative_point.x as f64 * width as f64 / physical_size.width as f64) as i32,
            y: (relative_point.y as f64 * height as f64 / physical_size.height as f64) as i32,
        };

        Some(Region {
            position,
            size: Size {
                width: (size.width as f64 * width as f64 / physical_size.width as f64) as u32,
                height: (size.height as f64 * height as f64 / physical_size.height as f64) as u32,
            },
        })
    }
}

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

use std::ops::Deref;

// Implementation of WayshotConnection methods related to ext_image_protocols
impl crate::WayshotConnection {
    /// Capture a single output
    pub fn ext_capture_single_output(
        &mut self,
        option: CaptureOption,
        output: crate::output::OutputInfo,
    ) -> std::result::Result<ImageViewInfo, crate::WayshotError> {
        let mem_fd = ext_create_shm_fd().unwrap();
        let mem_file = File::from(mem_fd);
        let mut capture_data = self.ext_capture_output_inner(
            output.clone(),
            option,
            mem_file.as_fd(),
            Some(&mem_file),
        )?;

        let mut frame_mmap = unsafe { memmap2::MmapMut::map_mut(&mem_file).unwrap() };

        let converter = crate::convert::create_converter(capture_data.frame_info.format).unwrap();
        let color_type = converter.convert_inplace(&mut frame_mmap);

        capture_data.color_type = color_type;

        // Create a full screen region representing the entire output
        let region = output.logical_region.inner.clone();

        Ok(ImageViewInfo {
            data: frame_mmap.deref().into(),
            width: capture_data.logical_region.inner.size.width,
            height: capture_data.logical_region.inner.size.height,
            color_type: capture_data.color_type, // pass color_type
            region,
        })
    }

    fn ext_capture_output_inner<T: AsFd>(
        &mut self,
        output_info: crate::output::OutputInfo,
        option: CaptureOption,
        fd: T,
        file: Option<&File>,
    ) -> std::result::Result<CaptureOutputData, crate::WayshotError> {
        let crate::output::OutputInfo {
            output,
            logical_region,
            ..
        } = output_info;

        let mut event_queue = self
            .ext_image
            .as_mut()
            .expect("ext_image should be initialized")
            .event_queue
            .take()
            .expect("Control your self");
        let img_manager = self
            .ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .output_image_manager
            .as_ref()
            .expect("Should init");
        let capture_manager = self
            .ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .img_copy_manager
            .as_ref()
            .expect("Should init");
        let qh = self
            .ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .qh
            .as_ref()
            .expect("Should init");
        let source = img_manager.create_source(&output, qh, ());
        let info = Arc::new(RwLock::new(FrameInfo {
            format: Format::Xrgb8888, // placeholder, will be set by protocol event
            size: Size { width: 0, height: 0 }, // placeholder
            stride: 0, // placeholder
        }));
        let session = capture_manager.create_session(&source, option.into(), qh, info.clone());

        let capture_info = CaptureInfo::new();
        let frame = session.create_frame(qh, capture_info.clone());
        event_queue.blocking_dispatch(self).unwrap();
        let qh = self
            .ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .qh
            .as_ref()
            .expect("Should init");
        let shm = self
            .ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .shm
            .as_ref()
            .expect("Should init");
        let info = info.read().unwrap();

        // Use direct field access for FrameInfo
        let Size { width, height } = info.size;
        let frame_format = info.format;
        if !matches!(
            frame_format,
			Format::Xbgr2101010
				| Format::Xrgb2101010
                | Format::Abgr2101010
                | Format::Argb8888
                | Format::Xrgb8888
                | Format::Xbgr8888
				| Format::Bgr888
        ) {
			println!("Unsupported format: {:?}", frame_format);
			return Err(crate::WayshotError::NotSupportFormat);
		} else {
			println!("Matched format: {:?}", frame_format);
		}

		let frame_bytes = 4 * height * width;
        let mem_fd = fd.as_fd();

        if let Some(file) = file {
            file.set_len(frame_bytes as u64).unwrap();
        }

        let stride = 4 * width;

        let shm_pool = shm.create_pool(mem_fd, (width * height * 4) as i32, qh, ());
        let buffer = shm_pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            frame_format,
            qh,
            (),
        );
        frame.attach_buffer(&buffer);
        frame.capture();

        let transform;
        loop {
            event_queue.blocking_dispatch(self)?;
            let info = capture_info.read().unwrap();
            match info.state() {
                FrameState::Succeeded => {
                    transform = info.transform();
                    break;
                }
                FrameState::Failed(info) => match info {
                    Some(WEnum::Value(reason)) => match reason {
                        wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason::Stopped => {
                            return Err(crate::WayshotError::CaptureFailed("Stopped".to_owned()));
                        }

                        wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints => {
                            return Err(crate::WayshotError::CaptureFailed(
                                "BufferConstraints".to_owned(),
                            ));
                        }
                        wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason::Unknown | _ => {
                            return Err(crate::WayshotError::CaptureFailed("Unknown".to_owned()));
                        }
                    },
                    Some(WEnum::Unknown(code)) => {
                        return Err(crate::WayshotError::CaptureFailed(format!(
                            "Unknown reason, code : {code}"
                        )));
                    }
                    None => {
                        return Err(crate::WayshotError::CaptureFailed(
                            "No failure reason provided".to_owned(),
                        ));
                    }
                },
                FrameState::Pending => {}
            }
        }

        self.reset_event_queue(event_queue);

        Ok(CaptureOutputData {
            output,
            buffer,
            logical_region: logical_region.clone(),
            frame_info: FrameInfo {
                format: frame_format,
                size: Size {
                    width: logical_region.inner.size.width as u32,
                    height: logical_region.inner.size.height as u32,
                },
                stride,
            },
            transform,
            color_type: ColorType::Rgba8, // placeholder, will be set after conversion
            physical_size: Size {
                width: logical_region.inner.size.width as u32,
                height: logical_region.inner.size.height as u32,
            },
        })
    }

    pub fn ext_capture_area2<F>(
        &mut self,
        option: CaptureOption,
        callback: F,
    ) -> std::result::Result<ImageViewInfo, crate::WayshotError>
    where
        F: AreaSelectCallback,
    {
        use crate::dispatch::XdgShellState;
        use wayland_client::{protocol::wl_surface::WlSurface, EventQueue};
        use wayland_protocols::xdg::shell::client::{xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel};
        use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
        use wayland_client::protocol::wl_compositor::WlCompositor;
        use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
        use tracing::debug;

        let outputs = self.vector_of_Outputs().clone();

        let mut data_list = vec![];
        for data in outputs.into_iter() {
            let mem_fd = ext_create_shm_fd().unwrap();
            let mem_file = File::from(mem_fd);
            let mut data =
                self.ext_capture_output_inner(data, option, mem_file.as_fd(), Some(&mem_file))?;
            data_list.push(AreaShotInfo { data, mem_file })
        }

        let mut state = XdgShellState::new();
        let mut event_queue: EventQueue<XdgShellState> = self.conn.new_event_queue();
        let globals = &self.globals;
        let qh = event_queue.handle();

        let compositor = globals.bind::<WlCompositor, _, _>(&qh, 3..=3, ())?;
        let xdg_wm_base = globals.bind::<XdgWmBase, _, _>(&qh, 1..=1, ())?;
        let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ())?;

        let mut xdg_surfaces: Vec<(WlSurface, XdgSurface, XdgToplevel)> =
            Vec::with_capacity(data_list.len());
        for AreaShotInfo { data, .. } in data_list.iter() {
            let CaptureOutputData {
                output,
                buffer,
                physical_size,
                transform,
                ..
            } = data;
            let surface = compositor.create_surface(&qh, ());

            let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &qh, output.clone());
            let xdg_toplevel = xdg_surface.get_toplevel(&qh, ());

            // Configure the toplevel to be fullscreen on the specific output
            xdg_toplevel.set_fullscreen(Some(output));
            xdg_toplevel.set_title("wayshot-overlay".to_string());
            xdg_toplevel.set_app_id("wayshot".to_string());

            debug!("Committing surface creation changes.");
            surface.commit();

            debug!("Waiting for layer surface to be configured.");
            while !state.configured_surfaces.contains(&xdg_surface) {
                event_queue.blocking_dispatch(&mut state)?;
            }

            surface.set_buffer_transform(*transform);
            // surface.set_buffer_scale(output_info.scale());
            surface.attach(Some(buffer), 0, 0);

            let viewport = viewporter.get_viewport(&surface, &qh, ());
            viewport.set_destination(physical_size.width as i32, physical_size.height as i32);

            debug!("Committing surface with attached buffer.");
            surface.commit();
            xdg_surfaces.push((surface, xdg_surface, xdg_toplevel));
            event_queue.blocking_dispatch(&mut state)?;
        }

        let region_re = callback.screenshot(self);

        debug!("Unmapping and destroying layer shell surfaces.");
        for (surface, xdg_surface, xdg_toplevel) in xdg_surfaces.iter() {
            surface.attach(None, 0, 0);
            surface.commit(); // unmap surface by committing a null buffer
            xdg_toplevel.destroy();
            xdg_surface.destroy();
        }
        event_queue.roundtrip(&mut state)?;
        let region = region_re?;

        let shotdata = data_list
            .iter()
            .find(|data| data.in_this_screen(region))
            .ok_or(crate::WayshotError::CaptureFailed("not in region".to_owned()))?;
        let area = shotdata.clip_area(region).expect("should have");
        let mut frame_mmap = unsafe { memmap2::MmapMut::map_mut(&shotdata.mem_file).unwrap() };

        let converter = crate::convert::create_converter(shotdata.data.frame_info.format).unwrap();
        let color_type = converter.convert_inplace(&mut frame_mmap);

        // Set color_type in CaptureOutputData
        let mut shotdata = shotdata.data.clone();
        shotdata.color_type = color_type;

        Ok(ImageViewInfo {
            data: frame_mmap.deref().into(),
            width: shotdata.logical_region.inner.size.width,
            height: shotdata.logical_region.inner.size.height,
            color_type: shotdata.color_type, // pass color_type
            region: area,
        })
    }

	/// Capture a single output
	pub fn ext_capture_toplevel2(
		&mut self,
		option: CaptureOption,
		toplevel: TopLevel,
	) -> Result<ImageViewInfo, WayshotError> {
		let mem_fd = ext_create_shm_fd().unwrap();
		let mem_file = File::from(mem_fd);
		let CaptureTopLevelData {
			width,
			height,
			frame_format,
			..
		} = self.ext_capture_toplevel_inner(toplevel, option, mem_file.as_fd(), Some(&mem_file))?;

		let mut frame_mmap = unsafe { memmap2::MmapMut::map_mut(&mem_file).unwrap() };

		let converter = crate::convert::create_converter(frame_format).unwrap();
		let color_type = converter.convert_inplace(&mut frame_mmap);

        // Use the full window as the region
        let region = Region {
            position: Position { x: 0, y: 0 },
            size: Size { width, height },
        };

		Ok(ImageViewInfo {
			data: frame_mmap.deref().into(),
			width,
			height,
			color_type,
			region,
		})
	}

	fn ext_capture_toplevel_inner<T: AsFd>(
		&mut self,
		TopLevel { handle, .. }: TopLevel,
		option: CaptureOption,
		fd: T,
		file: Option<&File>,
	) -> Result<CaptureTopLevelData, WayshotError> {
		let mut event_queue = self.ext_image
			.as_mut()
			.expect("ext_image should be initialized")
			.event_queue
			.take()
			.expect("Control your self");
		let img_manager = self.ext_image
			.as_ref()
			.expect("ext_image should be initialized")
			.toplevel_image_manager
			.as_ref()
			.expect("Should init");
		let capture_manager = self.ext_image
			.as_ref()
			.expect("ext_image should be initialized")
			.img_copy_manager
			.as_ref()
			.expect("Should init");
		let qh = self.ext_image
			.as_ref()
			.expect("ext_image should be initialized")
			.qh
			.as_ref()
			.expect("Should init");
		let source = img_manager.create_source(&handle, qh, ());
        // Provide a default FrameInfo since FrameInfo::default() does not exist
        let info = Arc::new(RwLock::new(FrameInfo {
			format: Format::Xrgb8888, // placeholder, will be set by protocol event
			size: Size { width: 0, height: 0 }, // placeholder
			stride: 0, // placeholder
		}));
		let session = capture_manager.create_session(&source, option.into(), qh, info.clone());

		let capture_info = CaptureInfo::new();
		let frame = session.create_frame(qh, capture_info.clone());
		event_queue.blocking_dispatch(self).unwrap();
		let qh = self.ext_image
			.as_ref()
			.expect("ext_image should be initialized")
			.qh
			.as_ref()
			.expect("Should init");
		let shm = self.ext_image
			.as_ref()
			.expect("ext_image should be initialized")
			.shm
			.as_ref()
			.expect("Should init");
		let info = info.read().unwrap();

		let Size { width, height } = info.size;
		let frame_format = info.format;
		let frame_bytes = 4 * height * width;
		if !matches!(
            frame_format,
            Format::Xbgr2101010
                | Format::Abgr2101010
                | Format::Argb8888
                | Format::Xrgb8888
                | Format::Xbgr8888
        ) {
			return Err(WayshotError::NotSupportFormat);
		}
		let mem_fd = fd.as_fd();

		if let Some(file) = file {
			file.set_len(frame_bytes as u64).unwrap();
		}

		let stride = 4 * width;

		let shm_pool = shm.create_pool(mem_fd, (width * height * 4) as i32, qh, ());
		let buffer = shm_pool.create_buffer(
			0,
			width as i32,
			height as i32,
			stride as i32,
			frame_format,
			qh,
			(),
		);
		frame.attach_buffer(&buffer);
		frame.capture();

		let transform;
		loop {
			event_queue.blocking_dispatch(self)?;
			let info = capture_info.read().unwrap();
			match info.state() {
				FrameState::Succeeded => {
					transform = info.transform();
					break;
				}
				FrameState::Failed(info) => match info {
					Some(WEnum::Value(reason)) => match reason {
						FailureReason::Stopped => {
							return Err(WayshotError::CaptureFailed("Stopped".to_owned()));
						}

						FailureReason::BufferConstraints => {
							return Err(WayshotError::CaptureFailed("BufferConstraints".to_owned()));
						}
						FailureReason::Unknown | _ => {
							return Err(WayshotError::CaptureFailed("Unknown".to_owned()));
						}
					},
					Some(WEnum::Unknown(code)) => {
						return Err(WayshotError::CaptureFailed(format!(
							"Unknown reason, code : {code}"
						)));
					}
					None => {
						return Err(WayshotError::CaptureFailed(
							"No failure reason provided".to_owned(),
						));
					}
				},
				FrameState::Pending => {}
			}
		}

		self.reset_event_queue(event_queue);

		Ok(CaptureTopLevelData {
			transform,
			buffer,
			width,
			height,
			frame_bytes,
			stride,
			frame_format,
		})
	}
}

use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason;
