// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod backend;

fn main() -> cosmic::iced::Result {
    env_logger::init();
    app::run()
}
