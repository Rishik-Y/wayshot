//! `libwayshot` is a convenient wrapper over the wlroots screenshot protocol
//! that provides a simple API to take screenshots with.
//!
//! To get started, look at [`WayshotConnection`].

mod convert;
mod dispatch;
pub mod error;
pub mod ext_image_protocols;
mod image_util;
pub mod output;
pub mod region;
mod screencopy;

use dispatch::{DMABUFState, XdgShellState};
use image::{DynamicImage, imageops::replace};
use khronos_egl::{self as egl, Instance};
use memmap2::MmapMut;
use region::{EmbeddedRegion, RegionCapturer};
use screencopy::{DMAFrameFormat, DMAFrameGuard, EGLImageGuard, FrameData, FrameGuard};
use std::ops::Deref;
use std::{
    ffi::c_void,
    fs::File,
    os::fd::{AsFd, IntoRawFd, OwnedFd},
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use tracing::debug;
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor,
        wl_output::{Transform, WlOutput},
        wl_shm::{self, WlShm},
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
    },
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{
    convert::create_converter,
    dispatch::{CaptureFrameState, FrameState, OutputCaptureState, WayshotState},
    output::OutputInfo,
    region::{LogicalRegion, Size},
    screencopy::{FrameCopy, FrameFormat, create_shm_fd},
};

pub use crate::error::{Result, WayshotError};

pub mod reexport {
    use wayland_client::protocol::wl_output;
    pub use wl_output::{Transform, WlOutput};
}
use crate::ext_image_protocols::{AreaSelectCallback, CaptureInfo, CaptureOption, FrameInfo, ImageViewInfo, TopLevel};
use gbm::{BufferObject, BufferObjectFlags, Device as GBMDevice};
use wayland_backend::protocol::WEnum;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface;
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use crate::region::Region;

/// Struct to store wayland connection and globals list.
/// # Example usage
///
/// ```ignore
/// use libwayshot::WayshotConnection;
/// let wayshot_connection = WayshotConnection::new()?;
/// let image_buffer = wayshot_connection.screenshot_all()?;
/// ```

#[derive(Debug)]
pub struct ExtBase<T> {
    pub toplevels: Vec<TopLevel>,
    pub img_copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    pub output_image_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub shm: Option<WlShm>,
    pub qh: Option<QueueHandle<T>>,
    pub event_queue: Option<EventQueue<T>>,
	pub toplevel_image_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
}

#[derive(Debug)]
pub struct WayshotConnection {
    pub conn: Connection,
    pub globals: GlobalList,
    pub output_infos: Vec<OutputInfo>, // Make this pub so it can be used in ext_image_protocols
    dmabuf_state: Option<DMABUFState>,
    pub ext_image: Option<ExtBase<Self>>,
}

impl WayshotConnection {
    pub fn new() -> Result<
        Self, //, HaruhiError
    > {
        // Try to use ext_image protocol first
        match Self::create_connection(None, true) {
            Ok(connection) => {
                tracing::debug!("Successfully created connection with ext_image protocol");
                Ok(connection)
            }
            Err(err) => {
                tracing::debug!(
                    "ext_image protocol not available ({}), falling back to wlr-screencopy",
                    err
                );
                // Fall back to wlr_screencopy
                Self::create_connection(None, false)
            }
        }
    }

    /// Recommended if you already have a [`wayland_client::Connection`].
    /// Internal function that handles connection creation with protocol selection
    fn create_connection(
        connection: Option<Connection>,
        use_ext_image: bool,
    ) -> Result<Self, WayshotError> {
        let conn = if let Some(conn) = connection {
            conn
        } else {
            Connection::connect_to_env()?
        };

        let (globals, mut event_queue) = registry_queue_init::<WayshotConnection>(&conn)?;

        // Create a base WayshotConnection with common fields
        let mut initial_state = Self {
            conn,
            globals,
            output_infos: Vec::new(),
            dmabuf_state: None,
            ext_image: if use_ext_image {
                Some(ExtBase {
                    toplevels: Vec::new(),
                    img_copy_manager: None,
                    output_image_manager: None,
                    shm: None,
                    qh: None,
                    event_queue: None,
					toplevel_image_manager: None,
				})
            } else {
                None
            },
        };

        // Refresh outputs which is needed for both protocols
        initial_state.refresh_outputs()?;

        // If using ext_image protocol, initialize the specific components
        if use_ext_image {
            let qh = event_queue.handle();

            // Bind to ext_image specific globals
            match initial_state
                .globals
                .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())
            {
                Ok(image_manager) => {
                    match initial_state
                        .globals
                        .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
                    {
                        Ok(output_image_manager) => {
                            // Add binding for toplevel_image_manager here
                            let toplevel_image_manager = initial_state
                                .globals
                                .bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
                                .ok();

                            match initial_state.globals.bind::<WlShm, _, _>(&qh, 1..=2, ()) {
                                Ok(shm) => {
                                    // Try to bind to toplevel list, but don't fail if not available
                                    let _ = initial_state
                                        .globals
                                        .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ());

                                    // Process events to ensure all bound globals are initialized
                                    event_queue.blocking_dispatch(&mut initial_state)?;

                                    // Set toplevel_image_manager if available
                                    if let Some(toplevel_image_manager) = toplevel_image_manager {
                                        if let Some(state) = initial_state.ext_image.as_mut() {
                                            state
                                                .toplevel_image_manager
                                                .replace(toplevel_image_manager);
                                        }
                                    }

                                    // Store the globals we fetched
                                    if let Some(ext_image) = initial_state.ext_image.as_mut() {
                                        ext_image.img_copy_manager = Some(image_manager);
                                        ext_image.output_image_manager = Some(output_image_manager);
                                        ext_image.qh = Some(qh);
                                        ext_image.shm = Some(shm);
                                        ext_image.event_queue = Some(event_queue);
                                    }
                                }
                                Err(_) => {
                                    return Err(WayshotError::ProtocolNotFound(
                                        "WlShm not found".to_string(),
                                    ));
                                }
                            }
                        }
                        Err(_) => {
                            return Err(WayshotError::ProtocolNotFound(
                                "ExtOutputImageCaptureSourceManagerV1 not found".to_string(),
                            ));
                        }
                    }
                }
                Err(_) => {
                    return Err(WayshotError::ProtocolNotFound(
                        "ExtImageCopyCaptureManagerV1 not found".to_string(),
                    ));
                }
            }
        }

        Ok(initial_state)
    }

    ///Create a WayshotConnection struct having DMA-BUF support
    /// Using this connection is required to make use of the dmabuf functions
    ///# Parameters
    /// - conn: a Wayland connection
    /// - device_path: string pointing to the DRI device that is to be used for creating the DMA-BUFs on. For example: "/dev/dri/renderD128"
    pub fn from_connection_with_dmabuf(conn: Connection, device_path: &str) -> Result<Self> {
        let (globals, evq) = registry_queue_init::<WayshotState>(&conn)?;
        let linux_dmabuf =
            globals.bind(&evq.handle(), 4..=ZwpLinuxDmabufV1::interface().version, ())?;
        let gpu = dispatch::Card::open(device_path);
        // init a GBM device
        let gbm = GBMDevice::new(gpu).unwrap();
        let mut initial_state = Self {
            conn,
            globals,
            output_infos: Vec::new(),
            dmabuf_state: Some(DMABUFState {
                linux_dmabuf,
                gbmdev: gbm,
            }),
            ext_image: None,
        };

        initial_state.refresh_outputs()?;

        Ok(initial_state)
    }

    /// refresh the outputs, to get new outputs
    pub fn refresh_outputs(&mut self) -> Result<()> {
        // Connecting to wayland environment.
        let mut state = OutputCaptureState {
            outputs: Vec::new(),
        };
        let mut event_queue = self.conn.new_event_queue::<OutputCaptureState>();
        let qh = event_queue.handle();

        // Bind to xdg_output global.
        let zxdg_output_manager = match self.globals.bind::<ZxdgOutputManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create ZxdgOutputManagerV1 version 3. Does your compositor implement ZxdgOutputManagerV1?"
                );
                panic!("{:#?}", e);
            }
        };

        // Fetch all outputs; when their names arrive, add them to the list
        let _ = self.conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;

        // We loop over each output and request its position data.
        // Also store the xdg_output reference in the OutputInfo
        let xdg_outputs: Vec<ZxdgOutputV1> = state
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| zxdg_output_manager.get_xdg_output(&output.output, &qh, index))
            .collect();

        event_queue.roundtrip(&mut state)?;

        for xdg_output in xdg_outputs {
            xdg_output.destroy();
        }

        if state.outputs.is_empty() {
            tracing::error!("Compositor did not advertise any wl_output devices!");
            return Err(WayshotError::NoOutputs);
        }
        tracing::trace!("Outputs detected: {:#?}", state.outputs);
        self.output_infos = state.outputs;

        Ok(())
    }

    /// Fetch all accessible wayland outputs.
    pub fn get_all_outputs(&self) -> &[OutputInfo] {
        self.output_infos.as_slice()
    }

    /// print the displays' info
    pub fn print_displays_info(&self) {
        for OutputInfo {
            physical_size: Size { width, height },
            logical_region:
                LogicalRegion {
                    inner:
                        region::Region {
                            position: region::Position { x, y },
                            size:
                                Size {
                                    width: logical_width,
                                    height: logical_height,
                                },
                        },
                },
            name,
            description,
            scale,
            ..
        } in self.get_all_outputs()
        {
            println!("{name}");
            println!("description: {description}");
            println!("    Size: {width},{height}");
            println!("    LogicSize: {logical_width}, {logical_height}");
            println!("    Position: {x}, {y}");
            println!("    Scale: {scale}");
        }
    }

    /// Get a FrameCopy instance with screenshot pixel data for any wl_output object.
    ///  Data will be written to fd.
    pub fn capture_output_frame_shm_fd<T: AsFd>(
        &self,
        cursor_overlay: i32,
        output: &WlOutput,
        fd: T,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(FrameFormat, FrameGuard)> {
        let (state, event_queue, frame, frame_format) =
            self.capture_output_frame_get_state_shm(cursor_overlay, output, capture_region)?;
        let frame_guard =
            self.capture_output_frame_inner(state, event_queue, frame, frame_format, fd)?;

        Ok((frame_format, frame_guard))
    }

    fn capture_output_frame_shm_from_file(
        &self,
        cursor_overlay: bool,
        output: &WlOutput,
        file: &File,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(FrameFormat, FrameGuard)> {
        let (state, event_queue, frame, frame_format) =
            self.capture_output_frame_get_state_shm(cursor_overlay as i32, output, capture_region)?;

        file.set_len(frame_format.byte_size())?;

        let frame_guard =
            self.capture_output_frame_inner(state, event_queue, frame, frame_format, file)?;

        Ok((frame_format, frame_guard))
    }
    /// # Safety
    ///
    /// Helper function/wrapper that uses the OpenGL extension OES_EGL_image to convert the EGLImage obtained from [`WayshotConnection::capture_output_frame_eglimage`]
    /// into a OpenGL texture.
    /// - The caller is supposed to setup everything required for the texture binding. An example call may look like:
    /// ```no_run, ignore
    /// gl::BindTexture(gl::TEXTURE_2D, self.gl_texture);
    /// gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
    /// wayshot_conn
    ///     .bind_output_frame_to_gl_texture(
    ///         true,
    ///        &wayshot_conn.get_all_outputs()[0].wl_output,
    ///        None)
    ///```
    /// # Parameters
    /// - `cursor_overlay`: A boolean flag indicating whether the cursor should be included in the capture.
    /// - `output`: Reference to the `WlOutput` from which the frame is to be captured.
    /// - `capture_region`: Optional region specifying a sub-area of the output to capture. If `None`, the entire output is captured.
    /// # Returns
    /// - If the function was found and called, an OK(()), note that this does not necessarily mean that binding was successful, only that the function was called.
    ///   The caller may check for any OpenGL errors using the standard routes.
    /// - If the function was not found, [`WayshotError::EGLImageToTexProcNotFoundError`] is returned
    pub unsafe fn bind_output_frame_to_gl_texture(
        &self,
        cursor_overlay: bool,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<()> {
        let egl = khronos_egl::Instance::new(egl::Static);
        let eglimage_guard =
            self.capture_output_frame_eglimage(&egl, cursor_overlay, output, capture_region)?;
        unsafe {
            let gl_egl_image_texture_target_2d_oes: unsafe extern "system" fn(
                target: gl::types::GLenum,
                image: gl::types::GLeglImageOES,
            ) -> () =
                std::mem::transmute(match egl.get_proc_address("glEGLImageTargetTexture2DOES") {
                    Some(f) => {
                        tracing::debug!("glEGLImageTargetTexture2DOES found at address {:#?}", f);
                        f
                    }
                    None => {
                        tracing::error!("glEGLImageTargetTexture2DOES not found");
                        return Err(WayshotError::EGLImageToTexProcNotFoundError);
                    }
                });

            gl_egl_image_texture_target_2d_oes(gl::TEXTURE_2D, eglimage_guard.image.as_ptr());
            tracing::trace!("glEGLImageTargetTexture2DOES called");
            Ok(())
        }
    }

    /// Obtain a screencapture in the form of a EGLImage.
    /// The display on which this image is created is obtained from the Wayland Connection.
    /// Uses the dma-buf provisions of the wlr-screencopy copy protocol to avoid VRAM->RAM copies
    /// It returns the captured frame as an `EGLImage`, wrapped in an `EGLImageGuard`
    /// for safe handling and cleanup.
    /// # Parameters
    /// - `egl_instance`: Reference to an egl API instance obtained from the khronos_egl crate, which is used to create the `EGLImage`.
    /// - `cursor_overlay`: A boolean flag indicating whether the cursor should be included in the capture.
    /// - `output`: Reference to the `WlOutput` from which the frame is to be captured.
    /// - `capture_region`: Optional region specifying a sub-area of the output to capture. If `None`, the entire output is captured.
    ///
    /// # Returns
    /// If successful, an EGLImageGuard which contains a pointer 'image' to the created EGLImage
    /// On error, the EGL [error code](https://registry.khronos.org/EGL/sdk/docs/man/html/eglGetError.xhtml) is returned via this crates Error type
    pub fn capture_output_frame_eglimage<'a, T: khronos_egl::api::EGL1_5>(
        &self,
        egl_instance: &'a Instance<T>,
        cursor_overlay: bool,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<EGLImageGuard<'a, T>> {
        let egl_display = unsafe {
            match egl_instance.get_display(self.conn.display().id().as_ptr() as *mut c_void) {
                Some(disp) => disp,
                None => return Err(egl_instance.get_error().unwrap().into()),
            }
        };
        tracing::trace!("eglDisplay obtained from Wayland connection's display");

        egl_instance.initialize(egl_display)?;
        self.capture_output_frame_eglimage_on_display(
            egl_instance,
            egl_display,
            cursor_overlay,
            output,
            capture_region,
        )
    }

    /// Obtain a screencapture in the form of a EGLImage on the given EGLDisplay.
    ///
    /// Uses the dma-buf provisions of the wlr-screencopy copy protocol to avoid VRAM->RAM copies
    /// It returns the captured frame as an `EGLImage`, wrapped in an `EGLImageGuard`
    /// for safe handling and cleanup.
    /// # Parameters
    /// - `egl_instance`: Reference to an `EGL1_5` instance, which is used to create the `EGLImage`.
    /// - `egl_display`: The `EGLDisplay` on which the image should be created.
    /// - `cursor_overlay`: A boolean flag indicating whether the cursor should be included in the capture.
    /// - `output`: Reference to the `WlOutput` from which the frame is to be captured.
    /// - `capture_region`: Optional region specifying a sub-area of the output to capture. If `None`, the entire output is captured.
    ///
    /// # Returns
    /// If successful, an EGLImageGuard which contains a pointer 'image' to the created EGLImage
    /// On error, the EGL [error code](https://registry.khronos.org/EGL/sdk/docs/man/html/eglGetError.xhtml) is returned via this crates Error type
    pub fn capture_output_frame_eglimage_on_display<'a, T: khronos_egl::api::EGL1_5>(
        &self,
        egl_instance: &'a Instance<T>,
        egl_display: egl::Display,
        cursor_overlay: bool,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<EGLImageGuard<'a, T>> {
        type Attrib = egl::Attrib;
        let (frame_format, _guard, bo) =
            self.capture_output_frame_dmabuf(cursor_overlay, output, capture_region)?;
        let modifier: u64 = bo.modifier().into();
        let image_attribs = [
            egl::WIDTH as Attrib,
            frame_format.size.width as Attrib,
            egl::HEIGHT as Attrib,
            frame_format.size.height as Attrib,
            0x3271, //EGL_LINUX_DRM_FOURCC_EXT
            bo.format() as Attrib,
            0x3272, //EGL_DMA_BUF_PLANE0_FD_EXT
            bo.fd_for_plane(0).unwrap().into_raw_fd() as Attrib,
            0x3273, //EGL_DMA_BUF_PLANE0_OFFSET_EXT
            bo.offset(0) as Attrib,
            0x3274, //EGL_DMA_BUF_PLANE0_PITCH_EXT
            bo.stride_for_plane(0) as Attrib,
            0x3443, //EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT
            (modifier as u32) as Attrib,
            0x3444, //EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT
            (modifier >> 32) as Attrib,
            egl::ATTRIB_NONE as Attrib,
        ];
        tracing::debug!(
            "Calling eglCreateImage with attributes: {:#?}",
            image_attribs
        );
        unsafe {
            match egl_instance.create_image(
                egl_display,
                khronos_egl::Context::from_ptr(egl::NO_CONTEXT),
                0x3270, // EGL_LINUX_DMA_BUF_EXT
                khronos_egl::ClientBuffer::from_ptr(std::ptr::null_mut()), //NULL
                &image_attribs,
            ) {
                Ok(image) => Ok(EGLImageGuard {
                    image,
                    egl_instance,
                    egl_display,
                }),
                Err(e) => {
                    tracing::error!("eglCreateImage call failed with error {e}");
                    Err(e.into())
                }
            }
        }
    }

    /// Obtain a screencapture in the form of a WlBuffer backed by a GBM Bufferobject on the GPU.
    /// Uses the dma-buf provisions of the wlr-screencopy copy protocol to avoid VRAM->RAM copies
    /// The captured frame is returned as a tuple containing the frame format, a guard to manage
    /// the WlBuffer's cleanup on drop, and the underlying `BufferObject`.
    /// - `cursor_overlay`: A boolean flag indicating whether the cursor should be included in the capture.
    /// - `output`: Reference to the `WlOutput` from which the frame is to be captured.
    /// - `capture_region`: Optional region specifying a sub-area of the output to capture. If `None`, the entire output is captured.
    ///# Returns
    /// On success, returns a tuple containing the frame format,
    ///   a guard to manage the frame's lifecycle, and the GPU-backed `BufferObject`.
    /// # Errors
    /// - Returns `NoDMAStateError` if the DMA-BUF state is not initialized a the time of initialization of this struct.
    pub fn capture_output_frame_dmabuf(
        &self,
        cursor_overlay: bool,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(DMAFrameFormat, DMAFrameGuard, BufferObject<()>)> {
        match &self.dmabuf_state {
            Some(dmabuf_state) => {
                let (state, event_queue, frame, frame_format) = self
                    .capture_output_frame_get_state_dmabuf(
                        cursor_overlay as i32,
                        output,
                        capture_region,
                    )?;
                let gbm = &dmabuf_state.gbmdev;
                let bo = gbm.create_buffer_object::<()>(
                    frame_format.size.width,
                    frame_format.size.height,
                    gbm::Format::try_from(frame_format.format)?,
                    BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
                )?;

                let stride = bo.stride();
                let modifier: u64 = bo.modifier().into();
                tracing::debug!(
                    "Created GBM Buffer object with input frame format {:#?}, stride {:#?} and modifier {:#?} ",
                    frame_format,
                    stride,
                    modifier
                );
                let frame_guard = self.capture_output_frame_inner_dmabuf(
                    state,
                    event_queue,
                    frame,
                    frame_format,
                    stride,
                    modifier,
                    bo.fd_for_plane(0).unwrap(),
                )?;

                Ok((frame_format, frame_guard, bo))
            }
            None => Err(WayshotError::NoDMAStateError),
        }
    }

    // This API is exposed to provide users with access to window manager (WM)
    // information. For instance, enabling Vulkan in wlroots alters the display
    // format. Consequently, using PipeWire to capture streams without knowing
    // the current format can lead to color distortion. This function attempts
    // a trial screenshot to determine the screen's properties.
    pub fn capture_output_frame_get_state_shm(
        &self,
        cursor_overlay: i32,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(
        CaptureFrameState,
        EventQueue<CaptureFrameState>,
        ZwlrScreencopyFrameV1,
        FrameFormat,
    )> {
        let mut state = CaptureFrameState {
            formats: Vec::new(),
            dmabuf_formats: Vec::new(),
            state: None,
            buffer_done: AtomicBool::new(false),
        };
        let mut event_queue = self.conn.new_event_queue::<CaptureFrameState>();
        let qh = event_queue.handle();

        // Instantiating screencopy manager.
        let screencopy_manager = match self.globals.bind::<ZwlrScreencopyManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create screencopy manager. Does your compositor implement ZwlrScreencopy?"
                );
                tracing::error!("err: {e}");
                return Err(WayshotError::ProtocolNotFound(
                    "ZwlrScreencopy Manager not found".to_string(),
                ));
            }
        };

        tracing::debug!("Capturing output(shm buffer)...");
        let frame = if let Some(embedded_region) = capture_region {
            screencopy_manager.capture_output_region(
                cursor_overlay,
                output,
                embedded_region.inner.position.x,
                embedded_region.inner.position.y,
                embedded_region.inner.size.width as i32,
                embedded_region.inner.size.height as i32,
                &qh,
                (),
            )
        } else {
            screencopy_manager.capture_output(cursor_overlay, output, &qh, ())
        };

        // Empty internal event buffer until buffer_done is set to true which is when the Buffer done
        // event is fired, aka the capture from the compositor is successful.
        while !state.buffer_done.load(Ordering::SeqCst) {
            event_queue.blocking_dispatch(&mut state)?;
        }

        tracing::trace!(
            "Received compositor frame buffer formats: {:#?}",
            state.formats
        );
        // Filter advertised wl_shm formats and select the first one that matches.
        let frame_format = state
            .formats
            .iter()
            .find(|frame| {
                matches!(
                    frame.format,
                    wl_shm::Format::Xbgr2101010
						| wl_shm::Format::Xrgb2101010
                        | wl_shm::Format::Abgr2101010
                        | wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Xbgr8888
                        | wl_shm::Format::Bgr888
                )
            })
            .copied()
            // Check if frame format exists.
            .ok_or_else(|| {
                tracing::error!("No suitable frame format found");
                WayshotError::NoSupportedBufferFormat
            })?;
        tracing::trace!("Selected frame buffer format: {:#?}", frame_format);

        Ok((state, event_queue, frame, frame_format))
    }

    fn capture_output_frame_get_state_dmabuf(
        &self,
        cursor_overlay: i32,
        output: &WlOutput,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(
        CaptureFrameState,
        EventQueue<CaptureFrameState>,
        ZwlrScreencopyFrameV1,
        DMAFrameFormat,
    )> {
        let mut state = CaptureFrameState {
            formats: Vec::new(),
            dmabuf_formats: Vec::new(),
            state: None,
            buffer_done: AtomicBool::new(false),
        };
        let mut event_queue = self.conn.new_event_queue::<CaptureFrameState>();
        let qh = event_queue.handle();

        // Instantiating screencopy manager.
        let screencopy_manager = match self.globals.bind::<ZwlrScreencopyManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create screencopy manager. Does your compositor implement ZwlrScreencopy?"
                );
                tracing::error!("err: {e}");
                return Err(WayshotError::ProtocolNotFound(
                    "ZwlrScreencopy Manager not found".to_string(),
                ));
            }
        };

        tracing::debug!("Capturing output for DMA-BUF API...");
        let frame = if let Some(embedded_region) = capture_region {
            screencopy_manager.capture_output_region(
                cursor_overlay,
                output,
                embedded_region.inner.position.x,
                embedded_region.inner.position.y,
                embedded_region.inner.size.width as i32,
                embedded_region.inner.size.height as i32,
                &qh,
                (),
            )
        } else {
            screencopy_manager.capture_output(cursor_overlay, output, &qh, ())
        };

        // Empty internal event buffer until buffer_done is set to true which is when the Buffer done
        // event is fired, aka the capture from the compositor is successful.
        while !state.buffer_done.load(Ordering::SeqCst) {
            event_queue.blocking_dispatch(&mut state)?;
        }

        tracing::trace!(
            "Received compositor frame buffer formats: {:#?}",
            state.formats
        );
        // TODO select appropriate format if there is more than one
        let frame_format = state.dmabuf_formats[0];
        tracing::trace!("Selected frame buffer format: {:#?}", frame_format);

        Ok((state, event_queue, frame, frame_format))
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_output_frame_inner_dmabuf(
        &self,
        mut state: CaptureFrameState,
        mut event_queue: EventQueue<CaptureFrameState>,
        frame: ZwlrScreencopyFrameV1,
        frame_format: DMAFrameFormat,
        stride: u32,
        modifier: u64,
        fd: OwnedFd,
    ) -> Result<DMAFrameGuard> {
        match &self.dmabuf_state {
            Some(dmabuf_state) => {
                println!("The program screenshoted via dmabuf");
                // Connecting to wayland environment.
                let qh = event_queue.handle();

                let linux_dmabuf = &dmabuf_state.linux_dmabuf;
                let dma_width = frame_format.size.width;
                let dma_height = frame_format.size.height;

                let dma_params = linux_dmabuf.create_params(&qh, ());

                dma_params.add(
                    fd.as_fd(),
                    0,
                    0,
                    stride,
                    (modifier >> 32) as u32,
                    (modifier & 0xffffffff) as u32,
                );
                tracing::trace!("Called  ZwpLinuxBufferParamsV1::create_params ");
                let dmabuf_wlbuf = dma_params.create_immed(
                    dma_width as i32,
                    dma_height as i32,
                    frame_format.format,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                    &qh,
                    (),
                );
                tracing::trace!("Called  ZwpLinuxBufferParamsV1::create_immed to create WlBuffer ");
                // Copy the pixel data advertised by the compositor into the buffer we just created.
                frame.copy(&dmabuf_wlbuf);
                tracing::debug!("wlr-screencopy copy() with dmabuf complete");

                // On copy the Ready / Failed events are fired by the frame object, so here we check for them.
                loop {
                    // Basically reads, if frame state is not None then...
                    if let Some(state) = state.state {
                        match state {
                            FrameState::Failed(_) => {
                                tracing::error!("Frame copy failed");
                                return Err(WayshotError::FramecopyFailed);
                            }
                            FrameState::Succeeded => {
                                tracing::trace!("Frame copy finished");

                                return Ok(DMAFrameGuard {
                                    buffer: dmabuf_wlbuf,
                                });
                            }
                            FrameState::Pending => {
                                // If still pending, continue the event loop to wait for status change
                            }
                        }
                    }

                    event_queue.blocking_dispatch(&mut state)?;
                }
            }
            None => Err(WayshotError::NoDMAStateError),
        }
    }

    fn capture_output_frame_inner<T: AsFd>(
        &self,
        mut state: CaptureFrameState,
        mut event_queue: EventQueue<CaptureFrameState>,
        frame: ZwlrScreencopyFrameV1,
        frame_format: FrameFormat,
        fd: T,
    ) -> Result<FrameGuard> {
        // Connecting to wayland environment.
        println!("The program screenshoted via wlshm");
        let qh = event_queue.handle();

        // Instantiate shm global.
        let shm = self.globals.bind::<WlShm, _, _>(&qh, 1..=1, ())?;
        let shm_pool = shm.create_pool(
            fd.as_fd(),
            frame_format
                .byte_size()
                .try_into()
                .map_err(|_| WayshotError::BufferTooSmall)?,
            &qh,
            (),
        );
        let buffer = shm_pool.create_buffer(
            0,
            frame_format.size.width as i32,
            frame_format.size.height as i32,
            frame_format.stride as i32,
            frame_format.format,
            &qh,
            (),
        );

        // Copy the pixel data advertised by the compositor into the buffer we just created.
        frame.copy(&buffer);
        // On copy the Ready / Failed events are fired by the frame object, so here we check for them.
        loop {
            // Basically reads, if frame state is not None then...
            if let Some(state) = state.state {
                match state {
                    FrameState::Failed(_) => {
                        tracing::error!("Frame copy failed");
                        return Err(WayshotError::FramecopyFailed);
                    }
                    FrameState::Succeeded => {
                        tracing::trace!("Frame copy finished");
                        return Ok(FrameGuard { buffer, shm_pool });
                    }
                    FrameState::Pending => {
                        // If still pending, continue the event loop to wait for status change
                    }
                }
            }

            event_queue.blocking_dispatch(&mut state)?;
        }
    }

    /// Get a FrameCopy instance with screenshot pixel data for any wl_output object.
    #[tracing::instrument(skip_all, fields(output = format!("{output_info}"), region = capture_region.map(|r| format!("{:}", r)).unwrap_or("fullscreen".to_string())))]
    fn capture_frame_copy(
        &self,
        cursor_overlay: bool,
        output_info: &OutputInfo,
        capture_region: Option<EmbeddedRegion>,
    ) -> Result<(FrameCopy, FrameGuard)> {
        // Create an in memory file and return it's file descriptor.
        let fd = create_shm_fd()?;
        // Create a writeable memory map backed by a mem_file.
        let mem_file = File::from(fd);

        let (frame_format, frame_guard) = self.capture_output_frame_shm_from_file(
            cursor_overlay,
            &output_info.output,
            &mem_file,
            capture_region,
        )?;

        let mut frame_mmap = unsafe { MmapMut::map_mut(&mem_file)? };
        let data = &mut *frame_mmap;
        let frame_color_type = match create_converter(frame_format.format) {
            Some(converter) => converter.convert_inplace(data),
            _ => {
                tracing::error!("Unsupported buffer format: {:?}", frame_format.format);
                tracing::error!(
                    "You can send a feature request for the above format to the mailing list for wayshot over at https://sr.ht/~shinyzenith/wayshot."
                );
                return Err(WayshotError::NoSupportedBufferFormat);
            }
        };
        let rotated_physical_size = match output_info.transform {
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                Size {
                    width: frame_format.size.height,
                    height: frame_format.size.width,
                }
            }
            _ => frame_format.size,
        };
        let frame_copy = FrameCopy {
            frame_format,
            frame_color_type,
            frame_data: FrameData::Mmap(frame_mmap),
            transform: output_info.transform,
            logical_region: capture_region
                .map(|capture_region| capture_region.logical())
                .unwrap_or(output_info.logical_region),
            physical_size: rotated_physical_size,
        };
        tracing::debug!("Created frame copy: {:#?}", frame_copy);
        Ok((frame_copy, frame_guard))
    }

    pub fn capture_frame_copies(
        &self,
        output_capture_regions: &[(OutputInfo, Option<EmbeddedRegion>)],
        cursor_overlay: bool,
    ) -> Result<Vec<(FrameCopy, FrameGuard, OutputInfo)>> {
        output_capture_regions
            .iter()
            .map(|(output_info, capture_region)| {
                self.capture_frame_copy(cursor_overlay, output_info, *capture_region)
                    .map(|(frame_copy, frame_guard)| (frame_copy, frame_guard, output_info.clone()))
            })
            .collect()
    }

    /// Create a layer shell surface for each output,
    /// render the screen captures on them and use the callback to select a region from them
    fn overlay_frames_and_select_region<F>(
        &self,
        frames: &[(FrameCopy, FrameGuard, OutputInfo)],
        callback: F,
    ) -> Result<LogicalRegion>
    where
        F: Fn(&WayshotConnection) -> Result<LogicalRegion, WayshotError>,
    {
        let mut state = XdgShellState::new();
        let mut event_queue: EventQueue<XdgShellState> =
            self.conn.new_event_queue::<XdgShellState>();
        let qh = event_queue.handle();

        let compositor = match self.globals.bind::<WlCompositor, _, _>(&qh, 3..=3, ()) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create compositor. Does your compositor implement WlCompositor?"
                );
                tracing::error!("err: {e}");
                return Err(WayshotError::ProtocolNotFound(
                    "WlCompositor not found".to_string(),
                ));
            }
        };

        // Use XDG shell instead of layer shell
        let xdg_wm_base = match self
            .globals
            .bind::<wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase, _, _>(
            &qh,
            1..=1,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    "Failed to create xdg_wm_base. Does your compositor implement XdgWmBase?"
                );
                tracing::error!("err: {e}");
                return Err(WayshotError::ProtocolNotFound(
                    "XdgWmBase not found".to_string(),
                ));
            }
        };

        let viewporter = self.globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
        if viewporter.is_none() {
            tracing::info!(
                "Compositor does not support wp_viewporter, display scaling may be inaccurate."
            );
        }

        // Use a vector to store XDG surfaces instead of layer shell surfaces
        let mut xdg_surfaces = Vec::with_capacity(frames.len());

        for (frame_copy, frame_guard, output_info) in frames {
            tracing::span!(
                tracing::Level::DEBUG,
                "overlay_frames::surface",
                output = format!("{output_info}")
            )
            .in_scope(|| -> Result<()> {
                let surface = compositor.create_surface(&qh, ());

                // Create XDG surface and toplevel instead of layer shell surface
                let xdg_surface =
                    xdg_wm_base.get_xdg_surface(&surface, &qh, output_info.output.clone());
                let xdg_toplevel = xdg_surface.get_toplevel(&qh, ());

                // Configure the toplevel to be fullscreen on the specific output
                xdg_toplevel.set_fullscreen(Some(&output_info.output));
                xdg_toplevel.set_title("wayshot-overlay".to_string());
                xdg_toplevel.set_app_id("wayshot".to_string());

                debug!("Committing surface creation changes.");
                surface.commit();

                debug!("Waiting for XDG surface to be configured.");
                while !state.configured_surfaces.contains(&xdg_surface) {
                    event_queue.blocking_dispatch(&mut state)?;
                }

                surface.set_buffer_transform(output_info.transform);
                surface.attach(Some(&frame_guard.buffer), 0, 0);

                if let Some(viewporter) = viewporter.as_ref() {
                    let viewport = viewporter.get_viewport(&surface, &qh, ());
                    viewport.set_destination(
                        output_info.logical_region.inner.size.width as i32,
                        output_info.logical_region.inner.size.height as i32,
                    );
                }

                debug!("Committing surface with attached buffer.");
                surface.commit();
                xdg_surfaces.push((surface, xdg_surface, xdg_toplevel));
                event_queue.blocking_dispatch(&mut state)?;

                Ok(())
            })?;
        }

        let callback_result = callback(self);

        debug!("Unmapping and destroying XDG shell surfaces.");
        for (surface, xdg_surface, xdg_toplevel) in xdg_surfaces.iter() {
            surface.attach(None, 0, 0);
            surface.commit(); // unmap surface by committing a null buffer
            xdg_toplevel.destroy();
            xdg_surface.destroy();
        }
        event_queue.roundtrip(&mut state)?;

        callback_result
    }

    /// Take a screenshot from the specified region.
    #[tracing::instrument(skip_all, fields(max_scale = tracing::field::Empty))]
    fn screenshot_region_capturer(
        &self,
        region_capturer: RegionCapturer,
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        let outputs_capture_regions: Vec<(OutputInfo, Option<EmbeddedRegion>)> =
            match region_capturer {
                RegionCapturer::Outputs(ref outputs) => outputs
                    .iter()
                    .map(|output_info| (output_info.clone(), None))
                    .collect(),
                RegionCapturer::Region(capture_region) => self
                    .get_all_outputs()
                    .iter()
                    .filter_map(|output_info| {
                        tracing::span!(
                            tracing::Level::DEBUG,
                            "filter_map",
                            output = format!(
                                "{output_info} at {region}",
                                output_info = format!("{output_info}"),
                                region = LogicalRegion::from(output_info),
                            ),
                            capture_region = format!("{}", capture_region),
                        )
                        .in_scope(|| {
                            if let Some(relative_region) =
                                EmbeddedRegion::new(capture_region, output_info.into())
                            {
                                tracing::debug!("Intersection found: {}", relative_region);
                                Some((output_info.clone(), Some(relative_region)))
                            } else {
                                tracing::debug!("No intersection found");
                                None
                            }
                        })
                    })
                    .collect(),
                RegionCapturer::Freeze(_) => self
                    .get_all_outputs()
                    .iter()
                    .map(|output_info| (output_info.clone(), None))
                    .collect(),
            };

        let frames = self.capture_frame_copies(&outputs_capture_regions, cursor_overlay)?;

        let capture_region: LogicalRegion = match region_capturer {
            RegionCapturer::Outputs(outputs) => outputs.as_slice().try_into()?,
            RegionCapturer::Region(region) => region,
            RegionCapturer::Freeze(callback) => {
                self.overlay_frames_and_select_region(&frames, callback)?
            }
        };

        // TODO When freeze was used, we can still further remove the outputs
        // that don't intersect with the capture region.

        thread::scope(|scope| {
            let max_scale = outputs_capture_regions
                .iter()
                .map(|(output_info, _)| output_info.scale as f64)
                .fold(1.0, f64::max);

            tracing::Span::current().record("max_scale", max_scale);

            let rotate_join_handles = frames
                .into_iter()
                .map(|(frame_copy, _, _)| {
                    scope.spawn(move || {
                        let image = (&frame_copy).try_into()?;
                        Ok((
                            image_util::rotate_image_buffer(
                                image,
                                frame_copy.transform,
                                frame_copy.logical_region.inner.size,
                                max_scale,
                            ),
                            frame_copy,
                        ))
                    })
                })
                .collect::<Vec<_>>();

            rotate_join_handles
                .into_iter()
                .flat_map(|join_handle| join_handle.join())
                .fold(
                    None,
                    |composite_image: Option<Result<_>>, image: Result<_>| {
                        // Default to a transparent image.
                        let composite_image = composite_image.unwrap_or_else(|| {
                            Ok(DynamicImage::new_rgba8(
                                (capture_region.inner.size.width as f64 * max_scale) as u32,
                                (capture_region.inner.size.height as f64 * max_scale) as u32,
                            ))
                        });

                        Some(|| -> Result<_> {
                            let mut composite_image = composite_image?;
                            let (image, frame_copy) = image?;
                            let (x, y) = (
                                ((frame_copy.logical_region.inner.position.x as f64
                                    - capture_region.inner.position.x as f64)
                                    * max_scale) as i64,
                                ((frame_copy.logical_region.inner.position.y as f64
                                    - capture_region.inner.position.y as f64)
                                    * max_scale) as i64,
                            );
                            tracing::span!(
                                tracing::Level::DEBUG,
                                "replace",
                                frame_copy_region = format!("{}", frame_copy.logical_region),
                                capture_region = format!("{}", capture_region),
                                x = x,
                                y = y,
                            )
                            .in_scope(|| {
                                tracing::debug!("Replacing parts of the final image");
                                replace(&mut composite_image, &image, x, y);
                            });

                            Ok(composite_image)
                        }())
                    },
                )
                .ok_or_else(|| {
                    tracing::error!("Provided capture region doesn't intersect with any outputs!");
                    WayshotError::NoOutputs
                })?
        })
    }

    /// Take a screenshot from the specified region.
    pub fn screenshot(
        &self,
        capture_region: LogicalRegion,
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        self.screenshot_region_capturer(RegionCapturer::Region(capture_region), cursor_overlay)
    }

    /// Take a screenshot, overlay the screenshot, run the callback, and then
    /// unfreeze the screenshot and return the selected region.
    pub fn screenshot_freeze<F>(&self, callback: F, cursor_overlay: bool) -> Result<DynamicImage>
    where
        F: Fn(&WayshotConnection) -> Result<LogicalRegion> + 'static,
    {
        self.screenshot_region_capturer(RegionCapturer::Freeze(Box::new(callback)), cursor_overlay)
    }

    /// Take a screenshot from one output
    pub fn screenshot_single_output(
        &self,
        output_info: &OutputInfo,
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        let (frame_copy, _) = self.capture_frame_copy(cursor_overlay, output_info, None)?;
        (&frame_copy).try_into()
    }

    /// Take a screenshot from all of the specified outputs.
    pub fn screenshot_outputs(
        &self,
        outputs: &[OutputInfo],
        cursor_overlay: bool,
    ) -> Result<DynamicImage> {
        if outputs.is_empty() {
            return Err(WayshotError::NoOutputs);
        }

        self.screenshot_region_capturer(RegionCapturer::Outputs(outputs.to_owned()), cursor_overlay)
    }

    /// Take a screenshot from all accessible outputs.
    pub fn screenshot_all(&self, cursor_overlay: bool) -> Result<DynamicImage> {
        self.screenshot_outputs(self.get_all_outputs(), cursor_overlay)
    }
}

impl WayshotConnection {
    /// get all outputs and their info
    pub fn vector_of_Outputs(&self) -> &Vec<OutputInfo> {
        &self.output_infos
    }

    /// get all toplevels (windows) info
    pub fn toplevels(&self) -> &Vec<TopLevel> {
        self.ext_image
            .as_ref()
            .expect("ext_image should be initialized")
            .toplevels
            .as_ref()
    }

    pub(crate) fn reset_event_queue(&mut self, event_queue: EventQueue<Self>) {
        self.ext_image
            .as_mut()
            .expect("ext_image should be initialized")
            .event_queue = Some(event_queue);
    }
}
