#![allow(clippy::print_stderr, clippy::print_stdout)] // allowed in this module only

use std::num::NonZeroU32;

use raw_window_handle::{DisplayHandle, HasDisplayHandle};
use tokio::sync::mpsc;
use winit::dpi::LogicalPosition;
use winit::event::{self, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersKeyState;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowAttributes};

use crate::rdp::{RdpInputEvent, RdpOutputEvent};

pub struct GuiContext {
    window: Window,
    event_loop: EventLoop<RdpOutputEvent>,
    context: softbuffer::Context<DisplayHandle<'static>>,
}

impl GuiContext {
    pub fn init() -> anyhow::Result<Self> {
        let event_loop = EventLoop::<RdpOutputEvent>::with_user_event().build()?;

        let window_attributes = WindowAttributes::default().with_title("IronRdp");
        let window = event_loop.create_window(window_attributes)?;

        // SAFETY: we drop the context right before the event loop is stopped, thus making it safe.
        let context = softbuffer::Context::new(unsafe {
            std::mem::transmute::<DisplayHandle<'_>, DisplayHandle<'static>>(event_loop.display_handle().unwrap())
        })
        .map_err(|e| anyhow::Error::msg(format!("unable to initialize softbuffer context: {e}")))?;

        Ok(Self {
            window,
            event_loop,
            context,
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn create_event_proxy(&self) -> EventLoopProxy<RdpOutputEvent> {
        self.event_loop.create_proxy()
    }

    pub fn run(self, input_event_sender: mpsc::UnboundedSender<RdpInputEvent>) -> anyhow::Result<()> {
        let Self {
            window,
            event_loop,
            context,
        } = self;

        // SAFETY: both the context and the window are kept alive until the end of this function’s scope
        let mut surface = softbuffer::Surface::new(&context, &window).expect("surface");
        let mut buffer_size = (0, 0);

        let mut input_database = ironrdp::input::Database::new();

        event_loop.run(|event, aloop| {
            aloop.set_control_flow(ControlFlow::Wait);

            match event {
                Event::WindowEvent { window_id, event } if window_id == window.id() => match event {
                    WindowEvent::Resized(size) => {
                        let scale_factor = (window.scale_factor() * 100.0) as u32;

                        let _ = input_event_sender.send(RdpInputEvent::Resize {
                            width: u16::try_from(size.width).unwrap(),
                            height: u16::try_from(size.height).unwrap(),
                            scale_factor,
                            // TODO: it should be possible to get the physical size here, however winit doesn't make it straightforward.
                            // FreeRDP does it based on DPI reading grabbed via [`SDL_GetDisplayDPI`](https://wiki.libsdl.org/SDL2/SDL_GetDisplayDPI):
                            // https://github.com/FreeRDP/FreeRDP/blob/ba8cf8cf2158018fb7abbedb51ab245f369be813/client/SDL/sdl_monitor.cpp#L250-L262
                            physical_size: None,
                        });
                    }
                    WindowEvent::CloseRequested => {
                        if input_event_sender.send(RdpInputEvent::Close).is_err() {
                            error!("Failed to send graceful shutdown event, closing the window");
                            aloop.exit();
                        }
                    }
                    WindowEvent::DroppedFile(_) => {
                        // TODO(#110): File upload
                    }
                    // WindowEvent::ReceivedCharacter(_) => {
                    // Sadly, we can't use this winit event to send RDP unicode events because
                    // of the several reasons:
                    // 1. `ReceivedCharacter` event doesn't provide a way to distinguish between
                    //    key press and key release, therefore the only way to use it is to send
                    //    a key press + release events sequentially, which will not allow to
                    //    handle long press and key repeat events.
                    // 2. This event do not fire for non-printable keys (e.g. Control, Alt, etc.)
                    // 3. This event fies BEFORE `KeyboardInput` event, so we can't make a
                    //    reasonable workaround for `1` and `2` by collecting physical key press
                    //    information first via `KeyboardInput` before processing `ReceivedCharacter`.
                    //
                    // However, all of these issues can be solved by updating `winit` to the
                    // newer version.
                    //
                    // TODO(#376): Update winit
                    // TODO(#376): Implement unicode input in native client
                    // }
                    WindowEvent::KeyboardInput { event, .. } => {
                        if let Some(scancode) = event.physical_key.to_scancode() {
                            let scancode = ironrdp::input::Scancode::from_u16(u16::try_from(scancode).unwrap());

                            let operation = match event.state {
                                event::ElementState::Pressed => ironrdp::input::Operation::KeyPressed(scancode),
                                event::ElementState::Released => ironrdp::input::Operation::KeyReleased(scancode),
                            };

                            let input_events = input_database.apply(std::iter::once(operation));

                            send_fast_path_events(&input_event_sender, input_events);
                        }
                    }
                    WindowEvent::ModifiersChanged(state) => {
                        const SHIFT_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x2A);
                        const CONTROL_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x1D);
                        const ALT_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(false, 0x38);
                        const LOGO_LEFT: ironrdp::input::Scancode = ironrdp::input::Scancode::from_u8(true, 0x5B);

                        let mut operations = smallvec::SmallVec::<[ironrdp::input::Operation; 4]>::new();

                        let mut add_operation = |pressed: bool, scancode: ironrdp::input::Scancode| {
                            let operation = if pressed {
                                ironrdp::input::Operation::KeyPressed(scancode)
                            } else {
                                ironrdp::input::Operation::KeyReleased(scancode)
                            };
                            operations.push(operation);
                        };

                        add_operation(state.lshift_state() == ModifiersKeyState::Pressed, SHIFT_LEFT);
                        add_operation(state.lcontrol_state() == ModifiersKeyState::Pressed, CONTROL_LEFT);
                        add_operation(state.lalt_state() == ModifiersKeyState::Pressed, ALT_LEFT);
                        add_operation(state.lsuper_state() == ModifiersKeyState::Pressed, LOGO_LEFT);

                        let input_events = input_database.apply(operations);

                        send_fast_path_events(&input_event_sender, input_events);
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let win_size = window.inner_size();
                        let x = (position.x / win_size.width as f64 * buffer_size.0 as f64) as _;
                        let y = (position.y / win_size.height as f64 * buffer_size.1 as f64) as _;
                        let operation = ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition { x, y });

                        let input_events = input_database.apply(std::iter::once(operation));

                        send_fast_path_events(&input_event_sender, input_events);
                    }
                    WindowEvent::MouseWheel { delta, .. } => {
                        let mut operations = smallvec::SmallVec::<[ironrdp::input::Operation; 2]>::new();

                        match delta {
                            event::MouseScrollDelta::LineDelta(delta_x, delta_y) => {
                                if delta_x.abs() > 0.001 {
                                    operations.push(ironrdp::input::Operation::WheelRotations(
                                        ironrdp::input::WheelRotations {
                                            is_vertical: false,
                                            rotation_units: (delta_x * 100.) as i16,
                                        },
                                    ));
                                }

                                if delta_y.abs() > 0.001 {
                                    operations.push(ironrdp::input::Operation::WheelRotations(
                                        ironrdp::input::WheelRotations {
                                            is_vertical: true,
                                            rotation_units: (delta_y * 100.) as i16,
                                        },
                                    ));
                                }
                            }
                            event::MouseScrollDelta::PixelDelta(delta) => {
                                if delta.x.abs() > 0.001 {
                                    operations.push(ironrdp::input::Operation::WheelRotations(
                                        ironrdp::input::WheelRotations {
                                            is_vertical: false,
                                            rotation_units: delta.x as i16,
                                        },
                                    ));
                                }

                                if delta.y.abs() > 0.001 {
                                    operations.push(ironrdp::input::Operation::WheelRotations(
                                        ironrdp::input::WheelRotations {
                                            is_vertical: true,
                                            rotation_units: delta.y as i16,
                                        },
                                    ));
                                }
                            }
                        };

                        let input_events = input_database.apply(operations);

                        send_fast_path_events(&input_event_sender, input_events);
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        let mouse_button = match button {
                            event::MouseButton::Left => ironrdp::input::MouseButton::Left,
                            event::MouseButton::Right => ironrdp::input::MouseButton::Right,
                            event::MouseButton::Middle => ironrdp::input::MouseButton::Middle,
                            event::MouseButton::Back => ironrdp::input::MouseButton::X1,
                            event::MouseButton::Forward => ironrdp::input::MouseButton::X2,
                            event::MouseButton::Other(native_button) => {
                                if let Some(button) = ironrdp::input::MouseButton::from_native_button(native_button) {
                                    button
                                } else {
                                    return;
                                }
                            }
                        };

                        let operation = match state {
                            event::ElementState::Pressed => ironrdp::input::Operation::MouseButtonPressed(mouse_button),
                            event::ElementState::Released => {
                                ironrdp::input::Operation::MouseButtonReleased(mouse_button)
                            }
                        };

                        let input_events = input_database.apply(std::iter::once(operation));

                        send_fast_path_events(&input_event_sender, input_events);
                    }
                    _ => {}
                },
                // Event::RedrawRequested(window_id) if window_id == window.id() => {
                // TODO: is there something we should handle here?
                // }
                Event::UserEvent(RdpOutputEvent::Image { buffer, width, height }) => {
                    trace!(width = ?width, height = ?height, "Received image with size");
                    trace!(window_physical_size = ?window.inner_size(), "Drawing image to the window with size");
                    buffer_size = (width, height);
                    surface
                        .resize(
                            NonZeroU32::new(u32::from(width)).unwrap(),
                            NonZeroU32::new(u32::from(height)).unwrap(),
                        )
                        .expect("surface resize");

                    let mut sb_buffer = surface.buffer_mut().expect("surface buffer");
                    sb_buffer.copy_from_slice(buffer.as_slice());
                    sb_buffer.present().expect("buffer present");
                }
                Event::UserEvent(RdpOutputEvent::ConnectionFailure(error)) => {
                    error!(?error);
                    eprintln!("Connection error: {}", error.report());
                    // TODO set proc_exit::sysexits::PROTOCOL_ERR.as_raw());
                    aloop.exit();
                }
                Event::UserEvent(RdpOutputEvent::Terminated(result)) => {
                    let _exit_code = match result {
                        Ok(reason) => {
                            println!("Terminated gracefully: {reason}");
                            proc_exit::sysexits::OK
                        }
                        Err(error) => {
                            error!(?error);
                            eprintln!("Active session error: {}", error.report());
                            proc_exit::sysexits::PROTOCOL_ERR
                        }
                    };
                    // TODO set exit_code.as_raw());
                    aloop.exit();
                }
                Event::UserEvent(RdpOutputEvent::PointerHidden) => {
                    window.set_cursor_visible(false);
                }
                Event::UserEvent(RdpOutputEvent::PointerDefault) => {
                    window.set_cursor_visible(true);
                }
                Event::UserEvent(RdpOutputEvent::PointerPosition { x, y }) => {
                    if let Err(error) = window.set_cursor_position(LogicalPosition::new(x, y)) {
                        error!(?error, "Failed to set cursor position");
                    }
                }
                _ => {}
            }

            if input_event_sender.is_closed() {
                aloop.exit();
            }
        })?;
        Ok(())
    }
}

fn send_fast_path_events(
    input_event_sender: &mpsc::UnboundedSender<RdpInputEvent>,
    input_events: smallvec::SmallVec<[ironrdp::pdu::input::fast_path::FastPathInputEvent; 2]>,
) {
    if !input_events.is_empty() {
        let _ = input_event_sender.send(RdpInputEvent::FastPath(input_events));
    }
}
