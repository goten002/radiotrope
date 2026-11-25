//! A Slint platform with no screen that draws with FemtoVG, the renderer of
//! the desktop app: an OpenGL context on EGL with no surface (Mesa, on the
//! GPU or in software), and a window whose frames are read back from an
//! offscreen target. It opens no window anywhere and needs no display
//! server, so it runs in the tests and in CI.
//!
//! The software renderer of [`crate::headless`] shares Slint's text layout
//! with FemtoVG but not what the winit backend does to a FemtoVG window:
//! [`FemtoWindow::suspend`] is what happens to it when a Wayland window is
//! hidden (to the tray) and shown again.

use std::cell::Cell;
use std::error::Error;
use std::ffi::CStr;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};
use std::time::Duration;

use glutin::api::egl::context::PossiblyCurrentContext;
use glutin::api::egl::device::Device;
use glutin::api::egl::display::Display;
use glutin::config::{ConfigSurfaceTypes, ConfigTemplateBuilder};
use glutin::context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentGlContext, Version};
use glutin::display::{GetGlDisplay, GlDisplay};
// The calls the winit backend makes on the renderer to suspend and resume
// it: not in Slint's public API
use i_slint_renderer_femtovg::{FemtoVGOpenGLRendererExt, FemtoVGRendererExt};
use slint::platform::femtovg_renderer::{FemtoVGRenderer, OpenGLInterface};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{PhysicalSize, Rgba8Pixel, SharedPixelBuffer, Window, WindowSize};

type GlError = Box<dyn Error + Send + Sync>;

/// An OpenGL context that is current without a surface: FemtoVG draws into
/// its own offscreen target for a snapshot
struct SurfacelessContext {
    context: PossiblyCurrentContext,
}

impl SurfacelessContext {
    /// A context on the first EGL device that gives one. `LIBGL_ALWAYS_SOFTWARE=1`
    /// makes Mesa's the software one (llvmpipe), as on a machine with no GPU.
    fn new() -> Result<Self, GlError> {
        let mut last_error: GlError = "EGL lists no devices".into();
        for device in Device::query_devices()? {
            match Self::on_device(&device) {
                Ok(context) => return Ok(context),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    fn on_device(device: &Device) -> Result<Self, GlError> {
        // Safety: no native display handle is passed, so there is none to
        // keep alive
        let display = unsafe { Display::with_device(device, None) }?;
        let template = ConfigTemplateBuilder::new()
            .with_surface_type(ConfigSurfaceTypes::empty())
            .with_alpha_size(8)
            .build();
        // Safety: the template has no native window
        let config = unsafe { display.find_configs(template) }?
            .next()
            .ok_or("EGL has no config without a surface")?;
        let mut last_error: GlError = "no OpenGL context".into();
        for api in [
            ContextApi::Gles(Some(Version::new(3, 0))),
            ContextApi::Gles(Some(Version::new(2, 0))),
            ContextApi::OpenGl(Some(Version::new(3, 3))),
        ] {
            let attributes = ContextAttributesBuilder::new()
                .with_context_api(api)
                .build(None);
            // Safety: the attributes have no native window
            match unsafe { display.create_context(&config, &attributes) } {
                Ok(context) => {
                    let context = context.make_current_surfaceless()?;
                    return Ok(Self { context });
                }
                Err(error) => last_error = error.into(),
            }
        }
        Err(last_error)
    }
}

// Safety: `get_proc_address` forwards to EGL, whose pointers live as long as
// the library, which is never unloaded
unsafe impl OpenGLInterface for SurfacelessContext {
    fn ensure_current(&self) -> Result<(), GlError> {
        if !self.context.is_current() {
            self.context.make_current_surfaceless()?;
        }
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), GlError> {
        Ok(())
    }

    fn resize(&self, _width: NonZeroU32, _height: NonZeroU32) -> Result<(), GlError> {
        Ok(())
    }

    fn get_proc_address(&self, name: &CStr) -> *const std::ffi::c_void {
        self.context.display().get_proc_address(name)
    }
}

/// Whether FemtoVG can draw here: EGL gives an OpenGL context with no
/// display (Mesa does, on a GPU or in software)
pub fn available() -> Result<(), String> {
    SurfacelessContext::new()
        .map(drop)
        .map_err(|error| error.to_string())
}

/// A window drawn by FemtoVG into memory
pub struct FemtoWindow {
    window: Window,
    renderer: FemtoVGRenderer,
    size: Cell<PhysicalSize>,
}

impl FemtoWindow {
    pub fn new() -> Result<Rc<Self>, PlatformError> {
        let context = SurfacelessContext::new()
            .map_err(|error| format!("no OpenGL context without a display: {error}"))?;
        let renderer = FemtoVGRenderer::new(context)?;
        Ok(Rc::new_cyclic(|this: &Weak<Self>| Self {
            window: Window::new(this.clone()),
            renderer,
            size: Cell::new(PhysicalSize::new(800, 600)),
        }))
    }

    /// The frame FemtoVG draws now
    pub fn snapshot(&self) -> Result<SharedPixelBuffer<Rgba8Pixel>, PlatformError> {
        self.window.take_snapshot()
    }

    /// What the winit backend does when a Wayland window is hidden (to the
    /// tray): Wayland can't hide a window, so it is destroyed, and the
    /// renderer gives up its OpenGL context and what it kept for it. The
    /// same calls as `WinitWindowAdapter::suspend` makes.
    pub fn suspend(&self) -> Result<(), PlatformError> {
        self.renderer.clear_graphics_context()
    }

    /// The window is shown again: the renderer gets a new OpenGL context,
    /// as from `WinitCompatibleRenderer::resume`
    pub fn resume(&self) -> Result<(), PlatformError> {
        let context = SurfacelessContext::new()
            .map_err(|error| format!("no OpenGL context without a display: {error}"))?;
        self.renderer.set_opengl_context(context)
    }
}

impl WindowAdapter for FemtoWindow {
    fn window(&self) -> &Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: WindowSize) {
        let scale = self.window.scale_factor();
        self.size.set(size.to_physical(scale));
        self.window.dispatch_event(WindowEvent::Resized {
            size: size.to_logical(scale),
        });
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
}

#[derive(Default)]
pub struct FemtoPlatform {
    /// The window Slint made last, taken by whoever shows it
    pub window: Rc<Cell<Option<Rc<FemtoWindow>>>>,
    /// The clock Slint's timers and animations run on
    pub time: Rc<Cell<Duration>>,
}

impl Platform for FemtoPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = FemtoWindow::new()?;
        self.window.set(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.time.get()
    }
}
