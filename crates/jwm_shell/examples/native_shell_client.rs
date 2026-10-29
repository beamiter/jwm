//! Real xdg-shell client used by shared-shell Wayland acceptance. Requests
//! fullscreen through the native protocol, rather than a WM layout shortcut.
use anyhow::Result;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

fn main() -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Client>(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let window = shell.create_window(
        compositor.create_surface(&qh),
        WindowDecorations::RequestServer,
        &qh,
    );
    let fullscreen = std::env::args().any(|arg| arg == "--fullscreen");
    window.set_title(if fullscreen {
        "SharedShellNativeFullscreen"
    } else {
        "SharedShellNativeTask"
    });
    window.set_app_id("jwm.shared-shell.native-test");
    window.set_min_size(Some((200, 120)));
    if fullscreen {
        window.set_fullscreen(None);
    }
    window.commit();
    let pool = SlotPool::new(1024 * 1024, &shm)?;
    let mut state = Client {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        exit: false,
    };
    while !state.exit {
        queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}
struct Client {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    exit: bool,
}
impl CompositorHandler for Client {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Client {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl WindowHandler for Client {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let width = configure
            .new_size
            .0
            .map(|n| n.get())
            .unwrap_or(360)
            .clamp(1, 8192);
        let height = configure
            .new_size
            .1
            .map(|n| n.get())
            .unwrap_or(240)
            .clamp(1, 8192);
        match self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) {
            Ok((buffer, canvas)) => {
                for (index, pixel) in canvas.chunks_exact_mut(4).enumerate() {
                    let x = index % width as usize;
                    let y = index / width as usize;
                    pixel.copy_from_slice(&[
                        if (x / 40 + y / 40).is_multiple_of(2) {
                            140
                        } else {
                            190
                        },
                        110,
                        60,
                        255,
                    ]);
                }
                if buffer.attach_to(self.window.wl_surface()).is_err() {
                    self.exit = true;
                    return;
                }
                self.window
                    .wl_surface()
                    .damage_buffer(0, 0, width as i32, height as i32);
                self.window.commit();
            }
            Err(error) => {
                eprintln!("native test buffer failed: {error}");
                self.exit = true;
            }
        }
    }
}
impl ShmHandler for Client {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_compositor!(Client);
delegate_output!(Client);
delegate_shm!(Client);
delegate_xdg_shell!(Client);
delegate_xdg_window!(Client);
delegate_registry!(Client);
impl ProvidesRegistryState for Client {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
