// SPDX-License-Identifier: GPL-3.0-only
//
// Wayland client with foreign-toplevel enumeration, toplevel activation,
// and screencopy (shm) for window thumbnails.

use calloop_wayland_source::WaylandSource;
use cosmic::cctk::{
    self,
    cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1,
    screencopy::{
        CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, FailureReason, Formats,
        Frame, ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler,
        ScreencopySessionData, ScreencopySessionDataExt, ScreencopyState,
    },
    sctk::{
        self,
        registry::{ProvidesRegistryState, RegistryState},
        seat::{SeatHandler, SeatState},
        shm::{Shm, ShmHandler, slot::SlotPool},
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
    wayland_client::{
        Connection, QueueHandle, WEnum,
        globals::registry_queue_init,
        protocol::{wl_buffer, wl_seat, wl_shm},
    },
    wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1,
};
use std::{collections::HashMap, hash::Hash, io::Write, sync::Arc, thread};

fn dbg_log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/altswitcher-debug.log")
    {
        let _ = writeln!(f, "{}", msg);
    }
}

use super::{Cmd, Event, ExtForeignToplevelHandleV1};

struct ToplevelCaptureSession {
    session: CaptureSession,
    pool: Option<SlotPool>,
    size: Option<(u32, u32)>,
    handle: ExtForeignToplevelHandleV1,
    // Track buffer used in current capture so we can read from it when Ready
    current_slot: Option<sctk::shm::slot::Slot>,
    current_buffer: Option<wl_buffer::WlBuffer>,
    /// True if pixel bytes are BGRA (need swap to RGBA), false if already RGBA (ABGR wl format)
    needs_byte_swap: bool,
}

pub struct AppData {
    qh: QueueHandle<Self>,
    registry_state: RegistryState,
    seat_state: SeatState,
    shm_state: Shm,
    toplevel_info_state: ToplevelInfoState,
    toplevel_manager_state: ToplevelManagerState,
    screencopy_state: ScreencopyState,
    event_sender: futures::channel::mpsc::Sender<Event>,
    captures: HashMap<ExtForeignToplevelHandleV1, ToplevelCaptureSession>,
}

impl AppData {
    fn send_event(&mut self, event: Event) {
        let _ = self.event_sender.try_send(event);
    }

    fn start_capture_for(&mut self, handle: &ExtForeignToplevelHandleV1) {
        if self.captures.contains_key(handle) {
            dbg_log(&format!("[altswitcher] capture already running for toplevel"));
            return;
        }
        let udata = SessionData {
            session_data: Default::default(),
            handle: handle.clone(),
        };
        let source = CaptureSource::Toplevel(handle.clone());
        let Ok(session) = self
            .screencopy_state
            .capturer()
            .create_session(&source, CaptureOptions::empty(), &self.qh, udata)
        else {
            dbg_log(&format!("[altswitcher] FAILED to create capture session (compositor may not support toplevel capture)"));
            return;
        };
        dbg_log(&format!("[altswitcher] created capture session"));
        self.captures.insert(
            handle.clone(),
            ToplevelCaptureSession {
                session,
                pool: None,
                size: None,
                handle: handle.clone(),
                current_slot: None,
                current_buffer: None,
                needs_byte_swap: false,
            },
        );
    }
}

pub fn subscription() -> cosmic::iced::Subscription<Event> {
    #[derive(Clone)]
    struct Id;
    impl Hash for Id {
        fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
    }
    cosmic::iced::Subscription::run_with(Id, |_| {
        let (tx, rx) = futures::channel::mpsc::channel(64);
        thread::spawn(move || {
            if let Err(e) = run_wayland_thread(tx) {
                log::error!("wayland thread failed: {}", e);
            }
        });
        rx
    })
}

fn run_wayland_thread(
    mut event_sender: futures::channel::mpsc::Sender<Event>,
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, event_queue) = registry_queue_init(&conn).map_err(|e| e.to_string())?;
    let qh = event_queue.handle();

    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);
    let shm_state = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let toplevel_info_state = ToplevelInfoState::new(&registry_state, &qh);
    let toplevel_manager_state = ToplevelManagerState::new(&registry_state, &qh);
    let screencopy_state = ScreencopyState::new(&globals, &qh);

    let (cmd_tx, cmd_rx) = calloop::channel::channel::<Cmd>();
    let _ = event_sender.try_send(Event::CmdSender(cmd_tx));

    let mut event_loop = calloop::EventLoop::<AppData>::try_new().map_err(|e| e.to_string())?;
    let loop_handle = event_loop.handle();

    let mut app_data = AppData {
        qh,
        registry_state,
        seat_state,
        shm_state,
        toplevel_info_state,
        toplevel_manager_state,
        screencopy_state,
        event_sender,
        captures: HashMap::new(),
    };

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| e.to_string())?;

    let cmd_conn = conn.clone();
    loop_handle
        .insert_source(cmd_rx, move |event, _, data: &mut AppData| {
            if let calloop::channel::Event::Msg(cmd) = event {
                match cmd {
                    Cmd::ActivateToplevel(handle) => {
                        if let Some(info) = data.toplevel_info_state.info(&handle) {
                            if let Some(cosmic_handle) = info.cosmic_toplevel.clone() {
                                if let Some(seat) = data.seat_state.seats().next() {
                                    data.toplevel_manager_state
                                        .manager
                                        .activate(&cosmic_handle, &seat);
                                }
                            }
                        }
                    }
                    Cmd::CaptureToplevel(handle) => {
                        // If we already have a capture session with allocated buffer, just
                        // re-request a capture. Otherwise create a new session.
                        let needs_new = !data.captures.contains_key(&handle)
                            || data
                                .captures
                                .get(&handle)
                                .and_then(|c| c.current_buffer.as_ref())
                                .is_none();
                        if needs_new {
                            data.start_capture_for(&handle);
                        } else {
                            // Re-issue capture on existing session with same buffer
                            let cap = data.captures.get(&handle).unwrap();
                            if let Some(buffer) = cap.current_buffer.clone() {
                                let frame_data = FrameData {
                                    frame_data: Default::default(),
                                    handle: handle.clone(),
                                };
                                cap.session.capture(&buffer, &[], &data.qh, frame_data);
                                let _ = cmd_conn.flush();
                                dbg_log("[altswitcher] re-captured existing session");
                            }
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    loop {
        event_loop
            .dispatch(None, &mut app_data)
            .map_err(|e| e.to_string())?;
    }
}

sctk::delegate_registry!(AppData);
sctk::delegate_seat!(AppData);
sctk::delegate_shm!(AppData);
cctk::delegate_toplevel_info!(AppData);
cctk::delegate_toplevel_manager!(AppData);
cctk::delegate_screencopy!(AppData);

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    sctk::registry_handlers![SeatState];
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: sctk::seat::Capability,
    ) {
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: sctk::seat::Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ToplevelInfoHandler for AppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        if let Some(info) = self.toplevel_info_state.info(toplevel).cloned() {
            dbg_log(&format!("[altswitcher] new toplevel: '{}' app_id={}", info.title, info.app_id));
            self.send_event(Event::NewToplevel(toplevel.clone(), info));
        }
        // Auto-start capture for every new toplevel
        self.start_capture_for(toplevel);
    }

    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        if let Some(info) = self.toplevel_info_state.info(toplevel).cloned() {
            self.send_event(Event::UpdateToplevel(toplevel.clone(), info));
        }
    }

    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        toplevel: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        self.captures.remove(toplevel);
        self.send_event(Event::CloseToplevel(toplevel.clone()));
    }
}

impl ToplevelManagerHandler for AppData {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        &mut self.toplevel_manager_state
    }

    fn capabilities(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: Vec<WEnum<zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1>>,
    ) {
    }
}

// Screencopy handler
impl ScreencopyHandler for AppData {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        dbg_log(&format!("[altswitcher] init_done size={:?} shm={:?}", formats.buffer_size, formats.shm_formats));
        // Find which capture this is for
        let Some(handle) = session
            .data::<SessionData>()
            .map(|d| d.handle.clone())
        else {
            return;
        };
        let (w, h) = formats.buffer_size;
        if w == 0 || h == 0 {
            return;
        }

        // Pick a format we can use. The compositor may offer ABGR/XBGR (native RGBA byte order)
        // or ARGB/XRGB (native BGRA byte order).
        let format = if formats.shm_formats.contains(&wl_shm::Format::Abgr8888) {
            wl_shm::Format::Abgr8888
        } else if formats.shm_formats.contains(&wl_shm::Format::Xbgr8888) {
            wl_shm::Format::Xbgr8888
        } else if formats.shm_formats.contains(&wl_shm::Format::Argb8888) {
            wl_shm::Format::Argb8888
        } else if formats.shm_formats.contains(&wl_shm::Format::Xrgb8888) {
            wl_shm::Format::Xrgb8888
        } else {
            dbg_log(&format!("[altswitcher] no compatible shm format in {:?}", formats.shm_formats));
            return;
        };
        // Track whether we need to swap bytes to get RGBA
        let needs_swap = matches!(
            format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        );

        let Some(cap) = self.captures.get_mut(&handle) else {
            return;
        };

        let stride = w as i32 * 4;
        let pool_size = stride * h as i32 * 2; // double-buffer headroom
        let mut pool = match SlotPool::new(pool_size as usize, &self.shm_state) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("SlotPool::new failed: {e}");
                return;
            }
        };
        let (buffer, _canvas) = match pool.create_buffer(w as i32, h as i32, stride, format) {
            Ok(x) => x,
            Err(e) => {
                log::warn!("create_buffer failed: {e}");
                return;
            }
        };
        let slot = buffer.slot();

        cap.size = Some((w, h));
        cap.pool = Some(pool);
        cap.current_slot = Some(slot);
        cap.current_buffer = Some(buffer.wl_buffer().clone());
        cap.needs_byte_swap = needs_swap;

        // Request capture
        let frame_data = FrameData {
            frame_data: Default::default(),
            handle: handle.clone(),
        };
        cap.session.capture(buffer.wl_buffer(), &[], qh, frame_data);
        // CRITICAL: flush the wayland connection so the capture request
        // actually reaches the compositor.
        conn.flush().unwrap();
        dbg_log("[altswitcher] capture request flushed");
    }

    fn stopped(&mut self, _: &Connection, _: &QueueHandle<Self>, session: &CaptureSession) {
        let Some(handle) = session.data::<SessionData>().map(|d| d.handle.clone()) else {
            return;
        };
        self.captures.remove(&handle);
    }

    fn ready(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        frame: &CaptureFrame,
        _: Frame,
    ) {
        dbg_log(&format!("[altswitcher] frame ready"));
        let Some(handle) = frame.data::<FrameData>().map(|d| d.handle.clone()) else {
            return;
        };
        let Some(cap) = self.captures.get_mut(&handle) else {
            return;
        };
        let (Some((w, h)), Some(slot), Some(pool)) =
            (cap.size, cap.current_slot.clone(), cap.pool.as_mut())
        else {
            return;
        };

        let needs_swap = cap.needs_byte_swap;
        // Read pixels from SHM
        let canvas = match pool.canvas(&slot) {
            Some(c) => c,
            None => {
                dbg_log("[altswitcher] pool.canvas returned None");
                return;
            }
        };
        let mut rgba: Vec<u8> = canvas.to_vec();
        if needs_swap {
            // ARGB8888 / XRGB8888 → bytes in memory are BGRA → swap B and R
            for chunk in rgba.chunks_exact_mut(4) {
                chunk.swap(0, 2);
            }
        }
        // For XRGB/XBGR, ensure alpha byte is 0xFF (not 0x00)
        for chunk in rgba.chunks_exact_mut(4) {
            if chunk[3] == 0 {
                chunk[3] = 0xFF;
            }
        }
        dbg_log(&format!("[altswitcher] sending capture: {}x{} swap={}", w, h, needs_swap));
        let arc = Arc::new(rgba);
        self.send_event(Event::ToplevelCapture(handle.clone(), arc, w, h));

        // Schedule a follow-up capture periodically (live updates while open)
        let _ = qh;
    }

    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        frame: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        dbg_log(&format!("[altswitcher] screencopy FAILED reason={:?}", reason));
        let _ = frame;
    }
}

// Session + frame user-data structs
struct SessionData {
    session_data: ScreencopySessionData,
    handle: ExtForeignToplevelHandleV1,
}
impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    handle: ExtForeignToplevelHandleV1,
}
impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}
