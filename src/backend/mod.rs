// SPDX-License-Identifier: GPL-3.0-only

pub mod wayland;

pub use cosmic::cctk::{
    toplevel_info::ToplevelInfo,
    wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
};

pub use wayland::subscription;

#[derive(Clone, Debug)]
pub enum Event {
    CmdSender(calloop::channel::Sender<Cmd>),
    NewToplevel(ExtForeignToplevelHandleV1, ToplevelInfo),
    UpdateToplevel(ExtForeignToplevelHandleV1, ToplevelInfo),
    CloseToplevel(ExtForeignToplevelHandleV1),
    /// A captured screenshot for a toplevel: (handle, rgba_pixels, width, height)
    ToplevelCapture(ExtForeignToplevelHandleV1, std::sync::Arc<Vec<u8>>, u32, u32),
}

#[derive(Debug, Clone)]
pub enum Cmd {
    ActivateToplevel(ExtForeignToplevelHandleV1),
    /// Request a capture of the given toplevel
    CaptureToplevel(ExtForeignToplevelHandleV1),
}
